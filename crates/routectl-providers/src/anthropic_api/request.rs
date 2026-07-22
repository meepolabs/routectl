//! Request normalization: routectl shape -> Anthropic wire format.
//!
//! v0.4.0: rewritten to consume the typed canonical (ContentPart,
//! SystemContent, ToolDef) so cache_control round-trips end-to-end on
//! the Anthropic-in / Anthropic-out and Anthropic-in / Bedrock-Invoke-out
//! paths. Forward-compat: ContentPart::Other and ToolDef::Other pass
//! through verbatim, so a new Anthropic block or builtin tool ships
//! without code edits here.
//!
//! Translation rules:
//! - `req.system` is read directly into the wire `system` field (Text or
//!   Blocks). Backwards-compatible fallback: when `req.system` is None,
//!   any Role::System messages in `req.messages` get lifted (today's
//!   behavior) so direct callers without an ingress aren't broken.
//! - User content is translated typed-block-by-typed-block. Unknown
//!   blocks pass through via ContentPart::Other -> ContentBlock::Other.
//! - Assistant content with reasoning_details (multi-turn tool-use)
//!   continues to require a signature on each thinking block.
//! - Tool message: the canonical Tool role becomes a user message with
//!   a tool_result block, same as today.
//! - Tools: ToolDef::Custom -> AnthropicTool::Custom (cache_control,
//!   defer_loading, strict, optional type_tag); ToolDef::Other ->
//!   AnthropicTool::Builtin (passthrough Value).
//! - Top-level cache_control and anthropic_beta are set on the body.
//! - cache_control::validate runs before serialization
//!   unconditionally (release builds too): it protects direct /
//!   library callers without an ingress from cap/ordering
//!   violations, in all build modes.
//!
//! This file is the orchestrator. The per-shape translation primitives
//! live in sibling modules: `system` (system prompt), `tools` (tool +
//! tool_choice), `messages` (per-role content blocks + replay
//! invariants), and `extras` (thinking-budget composition + post-merge
//! body reconciliation). `normalize` wires them together and owns the
//! top-level body assembly plus the cache_control breakpoint validation.

use serde_json::Value;

use routectl_core::cache_control::{self, BreakpointPosition};
use routectl_core::{ChatRequest, CoreHistoryReasoning, Error, Result};

// `MessageContent`, `ReasoningDetail`, and `ReasoningDetailKind` are
// referenced by the inline test modules below via `use super::*;`; the
// orchestrator code does not use them directly, so the import is
// test-gated to avoid unused-import warnings in release builds.
#[cfg(test)]
use routectl_core::{MessageContent, ReasoningDetail, ReasoningDetailKind};

use super::types::{
    AnthropicContent, AnthropicRequest, AnthropicSystem, AnthropicTool, ContentBlock,
    ThinkingConfig,
};

// Primitives used only by the orchestrator below.
use super::extras::{
    build_output_config, merge_provider_extras, reconcile_output_config_effort,
    reconcile_sampling_params, resolve_max_tokens, strip_thinking_when_tool_choice_forces_use,
};
use super::messages::{normalize_replay_invariants, translate_messages};
use super::tools::translate_tool_choice;

// Re-exports for callers outside this module. The Bedrock egress reuses
// the canonical-side Anthropic-shape primitives via
// `crate::anthropic_api::request::<name>`, and `mod.rs` reaches
// `filter_anthropic_betas` the same way; keeping these paths stable
// means those call sites need no edits across the file split.
pub(crate) use super::extras::{build_thinking, filter_anthropic_betas};
pub(crate) use super::system::translate_system;
// `lift_legacy_system` (the unfiltered lift) is consumed only by the
// Bedrock Converse egress; the anthropic-api orchestrator below uses the
// billing-aware `lift_legacy_system_stripped`. Gate the re-export so the
// lean (no-bedrock) build does not flag it as unused.
#[cfg(feature = "bedrock")]
pub(crate) use super::system::lift_legacy_system;

use super::system::lift_legacy_system_stripped;
pub(crate) use super::tools::translate_tool;

// `effort_ratio` and `is_routectl_managed_key` are surfaced only for the
// inline test modules below (via `use super::*;`); test-gated so they do
// not register as unused re-exports in release builds.
#[cfg(test)]
use super::extras::{effort_ratio, is_routectl_managed_key};

// ---------------------------------------------------------------------------
// cache_control validation
// ---------------------------------------------------------------------------

/// Walk all positions of the ASSEMBLED `AnthropicRequest` and validate the
/// collected breakpoint sequence (1h-after-5m ordering, 5+ count) before it
/// ships upstream.
///
/// This deliberately validates the POST-assembly wire body, NOT the canonical
/// `ChatRequest`. Assembly is lossy -- `tool_choice="none"` suppresses tools,
/// the billing-attribution strip drops a block, a legacy `Role::System` lift
/// flattens its cache_control away, and `Role::Tool` Parts collapse into one
/// unmarked `ToolResult` -- so this walk counts what ACTUALLY ships. It is
/// load-bearing and is NOT replaceable with `validate_source(req)` on the
/// canonical request: that would change the cap/ordering outcome for every
/// suppressed / stripped / lifted / collapsed request. The canonical
/// pre-assembly walk lives in routectl-core cache_control.rs
/// (`CacheBreakpointSource for ChatRequest`).
fn validate_breakpoints(ar: &AnthropicRequest) -> Result<()> {
    cache_control::validate_source(ar)
}

impl cache_control::CacheBreakpointSource for AnthropicRequest {
    fn cache_breakpoints(&self) -> Vec<cache_control::OwnedBreakpoint> {
        use cache_control::OwnedBreakpoint;
        let mut bps: Vec<OwnedBreakpoint> = Vec::new();

        // Tools come first in the cache prefix. `Custom` carries a typed
        // marker; `Builtin` carries it inside raw JSON, parsed on demand.
        if let Some(tools) = &self.tools {
            for t in tools {
                if let Some(cc) = anthropic_tool_cache_control(t) {
                    // borrowed ref -> clone to own (asymmetry: the
                    // builtin helper below already returns owned).
                    bps.push(OwnedBreakpoint::new(BreakpointPosition::Tools, cc.clone()));
                } else if let Some(cc) = builtin_tool_cache_control(t) {
                    bps.push(OwnedBreakpoint::new(BreakpointPosition::Tools, cc));
                }
            }
        }

        // Then system blocks.
        if let Some(AnthropicSystem::Blocks(blocks)) = &self.system {
            for b in blocks {
                if let Some(cc) = b.cache_control.as_ref() {
                    bps.push(OwnedBreakpoint::new(BreakpointPosition::System, cc.clone()));
                }
            }
        }

        // Then messages.
        for m in &self.messages {
            if let AnthropicContent::Blocks(blocks) = &m.content {
                for b in blocks {
                    if let Some(cc) = content_block_cache_control(b) {
                        bps.push(OwnedBreakpoint::new(
                            BreakpointPosition::Messages,
                            cc.clone(),
                        ));
                    }
                }
            }
        }

        // Top-level auto-cache marker.
        if let Some(cc) = self.cache_control.as_ref() {
            bps.push(OwnedBreakpoint::new(
                BreakpointPosition::TopLevel,
                cc.clone(),
            ));
        }

        bps
    }
}

/// Pull an owned `cache_control` out of an `AnthropicTool::Builtin`'s
/// raw JSON. Returns `None` for the typed `Custom` variant (handled by
/// `anthropic_tool_cache_control`) and for any builtin without a
/// parseable marker.
fn builtin_tool_cache_control(t: &AnthropicTool) -> Option<routectl_core::CacheControl> {
    match t {
        AnthropicTool::Builtin(v) => v
            .as_object()
            .and_then(|o| o.get("cache_control"))
            .and_then(|cc| serde_json::from_value::<routectl_core::CacheControl>(cc.clone()).ok()),
        _ => None,
    }
}

const fn content_block_cache_control(b: &ContentBlock) -> Option<&routectl_core::CacheControl> {
    match b {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::Document { cache_control, .. }
        | ContentBlock::Thinking { cache_control, .. }
        | ContentBlock::RedactedThinking { cache_control, .. }
        | ContentBlock::ToolUse { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. }
        | ContentBlock::Other { cache_control, .. } => cache_control.as_ref(),
    }
}

const fn anthropic_tool_cache_control(t: &AnthropicTool) -> Option<&routectl_core::CacheControl> {
    match t {
        AnthropicTool::Custom { cache_control, .. } => cache_control.as_ref(),
        AnthropicTool::Builtin(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Top-level normalize
// ---------------------------------------------------------------------------

/// `adaptive` now controls ONLY the thinking wire shape via `build_thinking`;
/// it no longer drives `output_config.effort` reconciliation, which the late
/// enforcer `reconcile_output_config_effort` derives from the assembled body.
pub(crate) fn normalize(
    id: &str,
    req: &ChatRequest,
    adaptive: bool,
    allowed_betas: &[String],
    context_management: bool,
    thinking_cache: Option<
        &std::sync::RwLock<crate::anthropic_api::context_management::ThinkingCache>,
    >,
) -> Result<Value> {
    // Anthropic's wire requires every tool_result carry the
    // `tool_use_id` of the tool_use it answers; missing ids are
    // rejected upfront (always, independent of history_reasoning).
    //
    // Thinking blocks must carry a `signature` for multi-turn replay on
    // real Anthropic. Cross-provider fallback (e.g. deepseek ->
    // Anthropic) and SDKs that don't round-trip the signature field can
    // produce unsigned blocks, so by default routectl STRIPS them and
    // forwards a body Anthropic accepts rather than 400ing the request.
    //
    // The strip is gated on `history_reasoning`: `Preserve` keeps
    // unsigned thinking on the wire because deepseek v4's `/anthropic`
    // endpoint emits unsigned thinking AND 400s the next turn unless it
    // is echoed back verbatim. `Auto` (the unset/None default) and
    // `Strip` both strip -- real-Anthropic-safe. The dispatch layer
    // resolves the per-model policy onto `routectl_internal`; library
    // callers that never set it get `Auto` = strip.
    let hr = req
        .routectl_internal
        .history_reasoning
        .unwrap_or(CoreHistoryReasoning::Auto);
    let messages = normalize_replay_invariants(id, req, hr)?;

    let max_tokens = resolve_max_tokens(req);
    let thinking = build_thinking(req, adaptive);
    let output_config = build_output_config(req, &thinking);

    // Prefer canonical req.system; fall back to lifting Role::System
    // messages for direct callers that bypass an ingress.
    //
    // Strip the Claude Code billing/attribution block unconditionally.
    // An anthropic-api provider can be pointed at a third-party host
    // (api-key OR oauth, non-Anthropic base_url); the OAuth-gated cloak
    // would not fire there, so the client fingerprint would otherwise
    // leak. Stripping here -- on the always-run normalize path -- closes
    // that for every anthropic-api egress.
    let mut billing_dropped = false;
    let filtered_system = req
        .system
        .as_ref()
        .and_then(|s| crate::system_filter::strip_billing_attribution(s, &mut billing_dropped));
    if billing_dropped {
        tracing::warn!(
            provider = id,
            "anthropic-api egress: Claude Code billing/attribution system block dropped",
        );
    }
    let system = filtered_system.as_ref().map(translate_system).or_else(|| {
        // Legacy lift: strip the billing block from the lifted text too.
        // lift_legacy_system joins Role::System messages into a single
        // AnthropicSystem::Text. Filter each message's text through the
        // same billing predicate so the fingerprint never reaches a
        // third-party host via this path either. A separate flag keeps
        // the WARN one-per-strip: the req.system branch above already
        // warned if it dropped, and that branch is mutually exclusive
        // with this fallback running at all.
        let mut legacy_dropped = false;
        let lifted_content = lift_legacy_system_stripped(&req.messages, &mut legacy_dropped);
        if legacy_dropped {
            tracing::warn!(
                provider = id,
                "anthropic-api egress: Claude Code billing/attribution system block \
                     dropped (legacy Role::System path)",
            );
        }
        lifted_content.as_ref().map(translate_system)
    });

    let mut anthropic_messages = translate_messages(id, &messages)?;

    // When context_management emulation is active, re-inject cached
    // thinking blocks before ToolUse blocks per the clear_thinking_20251015
    // edit spec. Collect any cache-miss ids for soft-fail below.
    let clear_thinking_misses: Vec<String> = if context_management {
        if let Some(tc) = thinking_cache {
            let apply_result = crate::anthropic_api::context_management::apply_clear_thinking_edit(
                &mut anthropic_messages,
                req.provider_extras.as_ref(),
                tc,
                id,
            );
            apply_result.missed_tool_ids
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // tool_choice="none" forbids tool use; Anthropic has no native
    // equivalent for the bare-string OpenAI form, so strip BOTH the
    // field and the tools list. The Anthropic-shape `{"type":"none"}`
    // object form passes through above and Anthropic suppresses tool
    // use server-side, so it doesn't need the extra strip.
    let suppress_tools = matches!(
        req.tool_choice.as_ref(),
        Some(Value::String(s)) if s == "none"
    );
    let has_tools = req.tools.as_ref().is_some_and(|t| !t.is_empty());
    let tools = if suppress_tools {
        None
    } else {
        req.tools
            .as_ref()
            .map(|ts| ts.iter().map(translate_tool).collect::<Vec<_>>())
    };

    // Anthropic requires temperature=1.0 when thinking is enabled
    // (legacy and adaptive both): no alternative-continuation sampling
    // while spending reasoning budget.
    let temperature = match &thinking {
        Some(ThinkingConfig::Enabled { .. } | ThinkingConfig::Adaptive) => Some(1.0f64),
        _ => req.temperature,
    };

    // Claude 4.x rejects requests that carry both `temperature` and
    // `top_p`, and also rejects `top_p` while thinking is active. Emit
    // `top_p` only when no temperature is in play; temperature wins.
    let top_p = if temperature.is_some() {
        None
    } else {
        req.top_p
    };

    let ar = AnthropicRequest {
        model: req.model.clone(),
        messages: anthropic_messages,
        max_tokens,
        system,
        thinking,
        output_config,
        temperature,
        top_p,
        stop_sequences: req.stop.clone(),
        stream: None, // caller sets this
        tools,
        tool_choice: translate_tool_choice(req.tool_choice.as_ref(), has_tools),
        cache_control: req.cache_control.clone(),
        anthropic_beta: filter_anthropic_betas(id, &req.anthropic_beta, allowed_betas).into_owned(),
    };

    // Belt-and-braces: validate in release too. The Anthropic ingress
    // already runs this at parse time; running it again here catches
    // direct callers (library users without an ingress) and protects
    // upstream from cap/ordering violations regardless of build mode.
    validate_breakpoints(&ar)?;

    let mut body =
        serde_json::to_value(&ar).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    merge_provider_extras(id, &mut body, req.provider_extras.as_ref());

    // When context_management emulation is active we have already applied
    // the edits above. Strip the `context_management` body key so it is
    // never forwarded to the upstream (non-Anthropic providers reject it).
    if context_management && let Some(obj) = body.as_object_mut() {
        obj.remove("context_management");
    }

    // Soft-fail: if cache misses occurred (cold-start or TTL eviction) and
    // the body still has a `thinking` key, the upstream would receive a
    // request that demands thinking tokens but no thinking blocks were
    // injected into history. Non-Anthropic providers 400 on this shape.
    // Strip `thinking` defensively and emit a structured warning so
    // operators can diagnose the gap.
    if !clear_thinking_misses.is_empty()
        && let Some(obj) = body.as_object_mut()
        && obj.contains_key("thinking")
    {
        obj.remove("thinking");
        tracing::warn!(
            provider = id,
            missed_tool_ids = ?clear_thinking_misses,
            "context_management: cache miss for tool_use ids; \
             stripped `thinking` from body to avoid upstream 400 \
             (cold-start or TTL eviction)"
        );
    }
    strip_thinking_when_tool_choice_forces_use(id, &mut body);
    // Late enforcer, runs LAST: output_config.effort is present IFF the
    // assembled body carries thinking with type == adaptive. Reads the
    // final body shape, so any earlier pass that stripped thinking
    // (cache-miss soft-fail above, tool_choice strip just now) is
    // correctly reflected -- no stale `adaptive` flag is trusted.
    reconcile_output_config_effort(req, &mut body);
    // Sampling analogue of the enforcer above, same final-body discipline:
    // assembly forces temperature=1.0 (dropping top_p) when thinking is
    // composed, and the strip passes above may then remove thinking. Recompute
    // the caller's sampling from the source request when no thinking survives,
    // so a stripped-thinking body never ships the forced 1.0.
    reconcile_sampling_params(id, req, &mut body);
    Ok(body)
}

#[cfg(test)]
#[path = "request_allowlist_tests.rs"]
mod allowlist_tests;

// Tests for context_management emulation in normalize().
#[cfg(test)]
#[path = "request_context_management_normalize_tests.rs"]
mod context_management_normalize_tests;

#[cfg(test)]
#[path = "request_multi_turn_tool_use_tests.rs"]
mod multi_turn_tool_use_tests;

// Anthropic effort clamping: operator-declared effort_levels cap the
// caller's effort on the Anthropic-shape egress (adaptive and legacy),
// matching the existing OpenAI-shape behavior.
#[cfg(test)]
#[path = "request_anthropic_effort_clamp_tests.rs"]
mod anthropic_effort_clamp_tests;

// effort_ratio parity: every token in VALID_EFFORT_TOKENS must have a
// non-default arm in effort_ratio, guarding against a new token silently
// falling through to the 0.50 default.
#[cfg(test)]
#[path = "request_effort_ratio_parity_tests.rs"]
mod effort_ratio_parity_tests;
