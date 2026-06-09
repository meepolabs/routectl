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

use routectl_core::cache_control::{self, Breakpoint, BreakpointPosition};
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
    build_output_config, merge_provider_extras, reconcile_output_config_effort, resolve_max_tokens,
    strip_thinking_when_tool_choice_forces_use,
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

/// Walk all positions of an AnthropicRequest and call
/// `cache_control::validate` against the collected breakpoint sequence.
/// Catches 1h-after-5m ordering violations and 5+ breakpoint counts
/// before they reach upstream.
fn validate_breakpoints(ar: &AnthropicRequest) -> Result<()> {
    let mut bps: Vec<Breakpoint<'_>> = Vec::new();

    // Owned cache_control values pulled out of `AnthropicTool::Builtin`'s
    // raw JSON. Lives here so the Breakpoint slice below can reference
    // them without lifetime issues. Indexed by position in `ar.tools`.
    let builtin_tool_ccs: Vec<Option<routectl_core::CacheControl>> = ar
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| match t {
                    AnthropicTool::Builtin(v) => v
                        .as_object()
                        .and_then(|o| o.get("cache_control"))
                        .and_then(|cc| {
                            serde_json::from_value::<routectl_core::CacheControl>(cc.clone()).ok()
                        }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Tools come first in the cache prefix.
    if let Some(tools) = &ar.tools {
        for (i, t) in tools.iter().enumerate() {
            if let Some(cc) = anthropic_tool_cache_control(t) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            } else if let Some(cc) = builtin_tool_ccs.get(i).and_then(|o| o.as_ref()) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            }
        }
    }

    // Then system blocks.
    if let Some(AnthropicSystem::Blocks(blocks)) = &ar.system {
        for b in blocks {
            if let Some(cc) = b.cache_control.as_ref() {
                bps.push(Breakpoint {
                    position: BreakpointPosition::System,
                    control: cc,
                });
            }
        }
    }

    // Then messages.
    for m in &ar.messages {
        if let AnthropicContent::Blocks(blocks) = &m.content {
            for b in blocks {
                if let Some(cc) = content_block_cache_control(b) {
                    bps.push(Breakpoint {
                        position: BreakpointPosition::Messages,
                        control: cc,
                    });
                }
            }
        }
    }

    // Top-level auto-cache marker.
    if let Some(cc) = ar.cache_control.as_ref() {
        bps.push(Breakpoint {
            position: BreakpointPosition::TopLevel,
            control: cc,
        });
    }

    cache_control::validate(&bps)
}

fn content_block_cache_control(b: &ContentBlock) -> Option<&routectl_core::CacheControl> {
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

fn anthropic_tool_cache_control(t: &AnthropicTool) -> Option<&routectl_core::CacheControl> {
    match t {
        AnthropicTool::Custom { cache_control, .. } => cache_control.as_ref(),
        AnthropicTool::Builtin(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Top-level normalize
// ---------------------------------------------------------------------------

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
        Some(ThinkingConfig::Enabled { .. }) | Some(ThinkingConfig::Adaptive) => Some(1.0f64),
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
    if context_management {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("context_management");
        }
    }

    // Soft-fail: if cache misses occurred (cold-start or TTL eviction) and
    // the body still has a `thinking` key, the upstream would receive a
    // request that demands thinking tokens but no thinking blocks were
    // injected into history. Non-Anthropic providers 400 on this shape.
    // Strip `thinking` defensively and emit a structured warning so
    // operators can diagnose the gap.
    if !clear_thinking_misses.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            if obj.contains_key("thinking") {
                obj.remove("thinking");
                tracing::warn!(
                    provider = id,
                    missed_tool_ids = ?clear_thinking_misses,
                    "context_management: cache miss for tool_use ids; \
                     stripped `thinking` from body to avoid upstream 400 \
                     (cold-start or TTL eviction)"
                );
            }
        }
    }
    reconcile_output_config_effort(&mut body, adaptive, &req.routectl_internal.effort_levels);
    strip_thinking_when_tool_choice_forces_use(id, &mut body);
    Ok(body)
}

#[cfg(test)]
mod allowlist_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, Role};
    use serde_json::json;

    fn req_with_betas(betas: Vec<String>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            anthropic_beta: betas,
            ..Default::default()
        }
    }

    /// Pin: empty allowlist = pass-through. Default behavior, no
    /// operator surprise on upgrade.
    #[test]
    fn empty_allowlist_passes_all_betas() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let body = normalize("p", &req, false, &[], false, None).unwrap();
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: non-empty allowlist drops entries not in the list.
    #[test]
    fn non_empty_allowlist_drops_unknown() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "secret-experimental-flag".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let allowed = vec![
            "context-1m-2025-08-07".to_string(),
            "prompt-caching-2024-07-31".to_string(),
        ];
        let body = normalize("p", &req, false, &allowed, false, None).unwrap();
        // Order preserved, unknown flag dropped.
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: every requested beta is rejected when none are on the
    /// allowlist. The wire field is either absent or an empty array;
    /// both mean "no betas reach upstream" and either serialization
    /// is acceptable.
    #[test]
    fn allowlist_can_drop_all_requested() {
        let req = req_with_betas(vec!["totally-unknown".into()]);
        let allowed = vec!["context-1m-2025-08-07".to_string()];
        let body = normalize("p", &req, false, &allowed, false, None).unwrap();
        let got = &body["anthropic_beta"];
        assert!(
            got.is_null() || got.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "expected absent or empty array, got: {got}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests for context_management emulation in normalize()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod context_management_normalize_tests {
    use super::*;
    use crate::anthropic_api::context_management::{
        snapshot_to_cache, ThinkingCache, CLEAR_THINKING_EDIT_TYPE,
    };
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
    use serde_json::json;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, RwLock};

    fn simple_req() -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    fn req_with_cm_extras() -> ChatRequest {
        ChatRequest {
            provider_extras: Some(json!({
                "context_management": {
                    "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
                }
            })),
            ..simple_req()
        }
    }

    fn req_with_tool_use_history_and_cm() -> ChatRequest {
        // Build a request whose messages contain an assistant turn with
        // tool_calls so translate_messages produces an AnthropicMessage
        // with a ToolUse block -- required for apply_clear_thinking_edit
        // to find qualifying messages.
        ChatRequest {
            model: "claude-sonnet-4".into(),
            max_tokens: Some(4096),
            reasoning: Some(ReasoningConfig {
                enabled: Some(true),
                max_tokens: Some(2048),
                effort: None,
                exclude: None,
            }),
            messages: vec![
                Message {
                    refusal: None,
                    role: Role::User,
                    content: MessageContent::Text("use the calc tool".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("calling calc".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "toolu_t1",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{}"}
                    })]),
                },
                Message {
                    refusal: None,
                    role: Role::Tool,
                    content: MessageContent::Text("42".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: Some("toolu_t1".into()),
                    tool_calls: None,
                },
            ],
            provider_extras: Some(json!({
                "context_management": {
                    "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
                }
            })),
            ..Default::default()
        }
    }

    fn small_cache(cap: usize) -> RwLock<ThinkingCache> {
        RwLock::new(lru::LruCache::new(NonZeroUsize::new(cap).expect("cap > 0")))
    }

    /// When context_management=true, the `context_management` body key
    /// (which came from provider_extras) must be stripped before returning.
    /// Non-Anthropic upstreams reject unknown top-level body keys.
    #[test]
    fn normalize_strips_context_management_body_key_when_flag_true() {
        let req = req_with_cm_extras();
        let body = normalize("test", &req, false, &[], true, None).expect("normalize must succeed");
        assert!(
            body.get("context_management").is_none(),
            "context_management body key must be stripped when flag=true; got: {body}"
        );
    }

    /// When context_management=false, the `context_management` body key
    /// must be forwarded verbatim to the upstream (e.g. real Anthropic).
    #[test]
    fn normalize_keeps_context_management_body_key_when_flag_false() {
        let req = req_with_cm_extras();
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        assert!(
            body.get("context_management").is_some(),
            "context_management body key must survive when flag=false; got: {body}"
        );
    }

    /// The anthropic-api egress strips the Claude Code billing/attribution
    /// system block unconditionally in normalize -- it runs for ALL
    /// anthropic-api requests, including those routed to a third-party
    /// host (e.g. a `/anthropic`-compatible upstream), so the client
    /// fingerprint never reaches a non-Anthropic party. The retained
    /// system blocks are forwarded in order.
    #[test]
    fn normalize_strips_billing_system_block() {
        use routectl_core::{SystemBlock, SystemContent};
        let mut req = simple_req();
        req.system = Some(SystemContent::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: "x-anthropic-billing-header: v=1; fp=secret".into(),
                cache_control: None,
                citations: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: "you are helpful".into(),
                cache_control: None,
                citations: None,
            },
        ]));
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let sys = body["system"].as_array().expect("system survives as array");
        assert_eq!(
            sys.len(),
            1,
            "billing block must be stripped, leaving only the prompt block; got: {body}"
        );
        assert_eq!(sys[0]["text"], "you are helpful");
    }

    /// A pure-billing `Text` system collapses to absent: the whole string
    /// is the fingerprint-bearing block, so the `system` key is dropped.
    #[test]
    fn normalize_drops_pure_billing_text_system() {
        use routectl_core::SystemContent;
        let mut req = simple_req();
        req.system = Some(SystemContent::Text(
            "x-anthropic-billing-header: v=1; fp=secret".into(),
        ));
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        assert!(
            body.get("system").is_none() || body["system"].is_null(),
            "pure-billing Text system must collapse to absent; got: {body}"
        );
    }

    /// Legacy-path defense-in-depth: when `req.system` is None, the billing
    /// block carried in a `Role::System` message must still be stripped
    /// before it is lifted into the Anthropic `system`. The HTTP ingress
    /// always lands system in `req.system` (covered above); this pins the
    /// direct-caller / legacy gap so the Claude Code fingerprint never
    /// reaches a third-party anthropic-api host via the lift fallback.
    #[test]
    fn normalize_strips_billing_from_legacy_system_message() {
        use routectl_core::Role;
        let mut req = simple_req();
        // req.system stays None: force the lift_legacy_system fallback.
        req.system = None;
        req.messages = vec![
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("x-anthropic-billing-header: v=1; fp=secret".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("you are helpful".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let sys = body
            .get("system")
            .and_then(|s| s.as_str())
            .expect("legacy lift produces a flat-text system");
        assert!(
            !sys.contains("x-anthropic-billing-header:"),
            "billing block must be stripped from the lifted legacy system; got: {sys:?}"
        );
        assert!(
            sys.contains("you are helpful"),
            "the non-billing system prompt must survive the strip; got: {sys:?}"
        );
    }

    /// A `Role::System` message whose ONLY content is the billing block
    /// must collapse to an absent `system` on the legacy lift path -- the
    /// fingerprint is the whole prompt, so nothing lands upstream.
    #[test]
    fn normalize_drops_pure_billing_legacy_system_message() {
        use routectl_core::Role;
        let mut req = simple_req();
        req.system = None;
        req.messages = vec![
            Message {
                refusal: None,
                role: Role::System,
                content: MessageContent::Text("x-anthropic-billing-header: v=1; fp=secret".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        assert!(
            body.get("system").is_none() || body["system"].is_null(),
            "a pure-billing legacy system must collapse to absent; got: {body}"
        );
    }

    /// Soft-fail: when context_management=true and the thinking cache has
    /// no entry for the qualifying tool_use id (cold-start or TTL eviction),
    /// the `thinking` key must be stripped from the outgoing body so the
    /// upstream (which does not honour the beta) does not 400.
    #[test]
    fn normalize_soft_fail_strips_thinking_on_cache_miss() {
        let req = req_with_tool_use_history_and_cm();
        let cache = Arc::new(small_cache(8)); // nothing seeded
        let body = normalize("test", &req, false, &[], true, Some(&cache))
            .expect("normalize must succeed even on cache miss");
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped on cache miss; got: {body}"
        );
    }

    /// No soft-fail when the cache has an entry for the qualifying
    /// tool_use id: the `thinking` key must remain in the outgoing body.
    #[test]
    fn normalize_no_soft_fail_when_cache_hits() {
        let req = req_with_tool_use_history_and_cm();
        let cache = Arc::new(small_cache(8));
        // Seed the cache for the tool_use id used in req_with_tool_use_history_and_cm.
        snapshot_to_cache(
            &cache,
            "test",
            "toolu_t1",
            vec![routectl_core::ReasoningDetail {
                kind: routectl_core::ReasoningDetailKind::Text,
                id: Some("rd-1".into()),
                format: Some(super::super::ANTHROPIC_FORMAT.to_string()),
                index: Some(0),
                payload: json!({"text": "my reasoning", "signature": "sig"}),
            }],
            super::super::context_management::DEFAULT_MAX_THINKING_ENTRY_BYTES,
            super::super::context_management::THINKING_CACHE_TTL,
            "test",
        );
        let body = normalize("test", &req, false, &[], true, Some(&cache))
            .expect("normalize must succeed with cache hit");
        assert!(
            body.get("thinking").is_some(),
            "thinking must NOT be stripped when cache has an entry; got: {body}"
        );
    }
}

#[cfg(test)]
mod multi_turn_tool_use_tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine};
    use routectl_core::{ChatRequest, CoreHistoryReasoning, Message, Role};
    use serde_json::json;

    /// A genuine Claude-shaped thinking signature: E-prefixed base64 of a
    /// payload whose first byte is 0x12. The egress strip preserves only
    /// Claude-shaped signatures, so test fixtures that must survive the
    /// strip use this rather than an arbitrary placeholder string.
    fn claude_signature() -> String {
        B64_STANDARD.encode([0x12u8, 0x34, 0x56, 0x78])
    }

    /// A distinct Claude-shaped signature, varied by a trailing byte so two
    /// surviving thinking blocks in one fixture stay distinguishable.
    fn claude_signature_variant(tag: u8) -> String {
        B64_STANDARD.encode([0x12u8, 0x34, 0x56, tag])
    }

    /// Minimal in-process tracing capture used by
    /// `emits_warn_when_stripping_occurs` to assert structured fields
    /// without taking on a `tracing-test` dev-dependency. Scoped via
    /// `tracing::subscriber::with_default` so concurrent unit tests do
    /// not leak captured state across threads.
    mod test_capture {
        // TODO(consolidation): this is the third copy of the same in-process
        // tracing-capture pattern. The other two live at:
        //   - crates/routectl-cli/tests/anthropic_forward_compat_stream.rs
        //     (lines 175-269): async with_capture for #[tokio::test].
        //   - crates/routectl-core/tests/common/mod.rs:
        //     synchronous capture_events with a TRACE level hint.
        // Next person to touch any of these three: extract a shared helper
        // (likely in routectl-core/tests/common/) that supports both sync
        // and async closures plus an opt-in TRACE level hint, then collapse
        // the copies. Keeping the inline copy for now because each consumer
        // wants a slightly different shape and full extraction is a larger
        // refactor than the original strip-instead-of-reject change.
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};

        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub struct CapturedEvent {
            pub level: tracing::Level,
            pub target: String,
            pub message: String,
            pub fields: Vec<(String, String)>,
        }

        #[derive(Default)]
        struct Collector {
            message: String,
            fields: Vec<(String, String)>,
        }

        impl Visit for Collector {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    self.fields.push((field.name().into(), value.into()));
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let s = format!("{value:?}");
                if field.name() == "message" {
                    self.message = s.trim_matches('"').to_string();
                } else {
                    self.fields.push((field.name().into(), s));
                }
            }
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.fields.push((field.name().into(), value.to_string()));
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                self.fields.push((field.name().into(), value.to_string()));
            }
            fn record_bool(&mut self, field: &Field, value: bool) {
                self.fields.push((field.name().into(), value.to_string()));
            }
        }

        struct CaptureSubscriber {
            captured: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let meta = event.metadata();
                let mut visitor = Collector::default();
                event.record(&mut visitor);
                let captured_event = CapturedEvent {
                    level: *meta.level(),
                    target: meta.target().to_string(),
                    message: visitor.message,
                    fields: visitor.fields,
                };
                if let Ok(mut guard) = self.captured.lock() {
                    guard.push(captured_event);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        /// Run `f` with the capture subscriber installed as the
        /// thread-local default. Returns the captured events.
        pub fn with_capture<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
            let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                captured: captured.clone(),
            };
            let _guard = tracing::subscriber::set_default(subscriber);
            f();
            let events = captured.lock().expect("capture lock poisoned").clone();
            events
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(text: &str, tool_calls: Option<Vec<Value>>) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    /// On a multi-turn assistant turn, `Message.tool_calls` (the
    /// canonical OpenAI-shape representation produced by
    /// `walk_content_blocks` on the response side) must be re-emitted
    /// as Anthropic `ContentBlock::ToolUse` entries. Without this,
    /// echoing a canonical Message back through the Anthropic egress
    /// drops the tool_use blocks and the next user `tool_result` turn
    /// fails upstream with "tool_use ids were found without preceding
    /// tool_use blocks".
    #[test]
    fn assistant_message_with_tool_calls_emits_tool_use_blocks() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("calculate 2+2"),
                assistant_msg(
                    "Let me calculate.",
                    Some(vec![json!({
                        "id": "toolu_abc123",
                        "type": "function",
                        "function": {
                            "name": "calc",
                            "arguments": "{\"expr\":\"2+2\"}",
                        }
                    })]),
                ),
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message must be present");
        let blocks = assistant
            .get("content")
            .and_then(|v| v.as_array())
            .expect("assistant content must be Blocks form when tool_calls present");

        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .expect("assistant must carry a tool_use block on multi-turn replay");
        assert_eq!(tool_use["id"], "toolu_abc123");
        assert_eq!(tool_use["name"], "calc");
        assert_eq!(tool_use["input"], json!({"expr": "2+2"}));
    }

    #[test]
    fn strips_unsigned_thinking_block_keeps_other_blocks() {
        // Multi-turn input with [text, signed_thinking, unsigned_thinking,
        // tool_use] -> outgoing assistant content has [text,
        // signed_thinking, tool_use]. The unsigned block is dropped;
        // every other content part survives unmodified.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("compute 2+2"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "Let me think.".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "signed analysis".into(),
                            signature: Some(claude_signature()),
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "unsigned analysis".into(),
                            signature: None,
                        }),
                        ContentPart::Known(KnownContentPart::ToolUse {
                            id: "toolu_1".into(),
                            name: "calc".into(),
                            input: json!({"expr": "2+2"}),
                            cache_control: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message present");
        let blocks = assistant
            .get("content")
            .and_then(|v| v.as_array())
            .expect("assistant content is Blocks form");

        let types: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            types,
            vec!["text", "thinking", "tool_use"],
            "expected unsigned thinking dropped, others preserved; got {types:?}"
        );

        // The signed thinking block survives with its signature intact.
        let signed = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
            .unwrap();
        assert_eq!(signed["signature"], claude_signature());

        // Other survivors keep their fields.
        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .unwrap();
        assert_eq!(tool_use["id"], "toolu_1");
        assert_eq!(tool_use["name"], "calc");
    }

    #[test]
    fn passes_through_when_all_thinking_signed() {
        // No mutation when every thinking block carries a signature.
        // Pin: signed-only histories must produce the same body the
        // pre-strip code produced.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "first".into(),
                            signature: Some(claude_signature_variant(0x01)),
                        }),
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "second".into(),
                            signature: Some(claude_signature_variant(0x02)),
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let assistant = body
            .get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let types: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            types,
            vec!["thinking", "text", "thinking"],
            "all blocks pass through unchanged when every thinking is signed"
        );
        assert_eq!(blocks[0]["signature"], claude_signature_variant(0x01));
        assert_eq!(blocks[2]["signature"], claude_signature_variant(0x02));
    }

    #[test]
    fn drops_assistant_message_when_only_block_was_unsigned_thinking() {
        // When stripping leaves the assistant message with content: []
        // AND the message has no reasoning_details / tool_calls to fill
        // the wire content array, drop the whole message. Anthropic's
        // wire spec rejects content: []; emitting it would just trade
        // one 400 for another.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hello"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                user_msg("any update?"),
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        // The empty-after-strip assistant message is gone; only the
        // two user messages remain.
        assert_eq!(
            messages.len(),
            2,
            "empty-after-strip assistant message must be dropped, got: {messages:?}"
        );
        let assistant_present = messages
            .iter()
            .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"));
        assert!(
            !assistant_present,
            "no assistant message must remain when its only block was an unsigned thinking, \
             got: {messages:?}"
        );
    }

    #[test]
    fn keeps_message_with_only_unsigned_thinking_when_tool_calls_present() {
        // Pin the corner: stripping leaves Parts empty BUT the message
        // carries tool_calls. The wire content array still gets blocks
        // from `emit_tool_use_blocks_from_calls`, so the message must
        // be kept (don't drop the tool_calls along with the empty Parts).
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "toolu_xyz",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{\"x\":1}"}
                    })]),
                },
            ],
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message must survive when tool_calls fill content");
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let has_tool_use = blocks
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"));
        assert!(
            has_tool_use,
            "tool_use block must reach the wire from tool_calls; got: {blocks:?}"
        );
        // Pin id + name so a translation regression that emits a
        // tool_use block with the wrong identity still fails.
        let tool_block = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tool_block["id"], "toolu_xyz");
        assert_eq!(tool_block["name"], "calc");
        // No thinking block leaks through; the unsigned was dropped.
        let has_thinking = blocks
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"));
        assert!(
            !has_thinking,
            "unsigned thinking must not appear; got: {blocks:?}"
        );
    }

    #[test]
    fn emits_warn_when_stripping_occurs() {
        // Capture the WARN log emitted during normalize and assert:
        // - structured fields `provider`, `dropped_blocks`,
        //   `affected_messages` are present
        // - block content (the `thinking` text) is NEVER logged --
        //   could be reasoning over sensitive data.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "TOPSECRET-REASONING-PAYLOAD".into(),
                            signature: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let captured = test_capture::with_capture(|| {
            normalize("provider-x", &req, false, &[], false, None).expect("normalize succeeds");
        });

        let strip_event = captured
            .iter()
            .find(|e| e.message.contains("stripping unsigned thinking blocks"))
            .unwrap_or_else(|| panic!("expected strip WARN, got events: {captured:?}"));
        assert_eq!(strip_event.level, tracing::Level::WARN);

        // Structured fields present.
        let field_keys: Vec<&str> = strip_event.fields.iter().map(|(k, _)| k.as_str()).collect();
        for key in &["provider", "dropped_blocks", "affected_messages"] {
            assert!(
                field_keys.contains(key),
                "expected field `{key}` in WARN, got fields: {:?}",
                strip_event.fields
            );
        }
        let provider_value = strip_event
            .fields
            .iter()
            .find(|(k, _)| k == "provider")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(provider_value, "provider-x");

        // Block content must not appear anywhere in the captured events.
        for evt in &captured {
            assert!(
                !evt.message.contains("TOPSECRET-REASONING-PAYLOAD"),
                "thinking block content leaked into log message: {evt:?}"
            );
            for (_, v) in &evt.fields {
                assert!(
                    !v.contains("TOPSECRET-REASONING-PAYLOAD"),
                    "thinking block content leaked into log fields: {evt:?}"
                );
            }
        }
    }

    #[test]
    fn tool_message_without_tool_call_id_is_rejected() {
        // Anthropic requires `tool_result` to reference the
        // `tool_use.id` it answers. An empty / missing
        // `tool_call_id` on a Role::Tool message used to fall
        // through as `unwrap_or_default()` (empty string) and
        // upstream returned a vague 400. Reject locally with a
        // precise NormalizeRequest error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("result content".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        let err = normalize("test-anthropic", &req, false, &[], false, None).unwrap_err();
        assert!(
            err.to_string().contains("tool_call_id"),
            "must mention tool_call_id; got: {err}"
        );
    }

    #[test]
    fn unsigned_thinking_block_is_stripped_not_rejected() {
        // Regression: prior behavior was a HTTP 400
        // ("thinking block without signature"). New behavior STRIPS
        // the unsigned block from the outgoing body and forwards the
        // rest. Cross-provider fallback (deepseek -> Anthropic) and
        // SDKs that fail to round-trip the signature field rely on
        // this -- a hard reject would 400 every multi-turn after
        // such a turn.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };
        // Must NOT error: the new behavior is to strip the unsigned
        // block, not reject the request.
        let body = normalize("test-anthropic", &req, false, &[], false, None).expect(
            "normalize must accept the request and strip the unsigned block; \
             a hard reject would regress the cross-provider fallback path",
        );
        let assistant = body
            .get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        // Only the text block survives; the unsigned thinking is dropped.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn assistant_tool_call_with_unparseable_arguments_wraps_under_underscore() {
        // Defensive fallback: a tool_call.arguments string that
        // isn't valid JSON shouldn't silently produce a malformed
        // upstream body. We wrap under {"_arguments": "..."} and
        // emit a WARN, so the upstream returns a useful error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![assistant_msg(
                "",
                Some(vec![json!({
                    "id": "toolu_xyz",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "this is not json"}
                })]),
            )],
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .unwrap();
        assert_eq!(tool_use["input"], json!({"_arguments": "this is not json"}));
    }

    /// With `adaptive = true`, the wire shape is the
    /// Opus 4.7+ form -- `thinking: {type:"adaptive"}` (no
    /// `budget_tokens`) plus a top-level `output_config: {effort:...}`
    /// carrying the canonical `reasoning.effort` string verbatim.
    #[test]
    fn adaptive_emits_adaptive_shape_with_output_config() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("xhigh".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();

        // thinking serializes to {"type":"adaptive"} -- no budget_tokens.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "adaptive");
        assert!(
            thinking.get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {thinking:?}"
        );

        // output_config carries the effort verbatim.
        let oc = body.get("output_config").expect("output_config present");
        assert_eq!(oc["effort"], "xhigh");

        // Anthropic requires temperature == 1.0 with thinking active --
        // both Enabled and Adaptive variants trigger the same constraint.
        assert_eq!(body["temperature"], 1.0);
    }

    /// End-to-end: a ChatRequest whose canonical `reasoning.effort` was
    /// set (as the OpenAI ingress does when promoting a top-level
    /// `reasoning_effort`) must compose thinking on the egress AND carry
    /// no stray top-level `reasoning_effort` key. Proves both halves of
    /// the fix: thinking composed + leak gone.
    #[test]
    fn reasoning_effort_composes_thinking_and_does_not_leak() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        // Thinking composed from the effort string.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");

        // No stray reasoning_effort key leaked into the egress body.
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must not leak into egress body, got {body:?}"
        );
    }

    /// `reasoning.effort == "none"` must disable thinking (the
    /// `thinking` field emits `{"type":"disabled"}`, not a budget) and
    /// never leak a top-level `reasoning_effort` key.
    #[test]
    fn reasoning_effort_none_disables_thinking_and_does_not_leak() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("none".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        // Disabled thinking emits the disabled shape, not a budget.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "disabled");
        assert!(
            thinking.get("budget_tokens").is_none(),
            "disabled thinking must not carry a budget, got {thinking:?}"
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must not leak into egress body, got {body:?}"
        );
    }
    /// shape is the legacy `Enabled { budget_tokens }` form. Older
    /// Claude models (4.5/4.6 family) still want this shape and would
    /// 400 on the adaptive form.
    #[test]
    fn legacy_thinking_unchanged_when_flag_false() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        // table("high")=24576 clamped to window ceiling max_tokens-1 = 2047.
        assert_eq!(thinking["budget_tokens"], 2047);

        // No output_config on the legacy path.
        assert!(
            body.get("output_config").is_none(),
            "legacy shape must not emit output_config, got {body:?}"
        );

        assert_eq!(body["temperature"], 1.0);
    }

    /// `effort = "max"` on the legacy path maps via the exact table to
    /// 128000, which the `[1024, max_tokens-1]` window clamps down to
    /// `max_tokens - 1`. The adaptive path passes "max" verbatim into
    /// `output_config.effort` and never consults the table. This test
    /// pins the legacy mapping so a non-adaptive provider receiving
    /// `max` from the canonical surface still produces a serializable
    /// body.
    #[test]
    fn effort_max_maps_to_window_ceiling_legacy_path() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2000),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "enabled");
        // table("max")=128000 clamped to window ceiling max_tokens-1 = 1999.
        assert_eq!(thinking["budget_tokens"], 1999);
    }

    /// `reasoning.effort = "none"` produces `Disabled` on both
    /// paths. The adaptive flag does not coerce a Disabled into an
    /// Adaptive -- if the caller said no thinking, we honor it.
    #[test]
    fn disabled_thinking_unchanged_under_adaptive_flag() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(512),
            reasoning: Some(ReasoningConfig {
                effort: Some("none".into()),
                max_tokens: None,
                exclude: None,
                enabled: None,
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    /// The barefoot adaptive case -- `reasoning.enabled = true`
    /// with no effort and no budget. Adaptive shape applies; effort
    /// defaults to "medium". This is the only path where
    /// `derive_effort` returns the fallback string, so we pin it
    /// explicitly. (Without this test the default would silently
    /// drift if anyone changed `derive_effort`.)
    #[test]
    fn adaptive_defaults_effort_to_medium_when_unset() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    /// When `adaptive = true` AND the caller sets an
    /// explicit `reasoning.max_tokens`, the budget is dropped (the
    /// adaptive wire shape has no field for it) and a tracing::warn
    /// fires at normalize time. We can't easily assert the warn in a
    /// unit test without `tracing-test`, but we CAN pin that the
    /// resulting body is the adaptive shape with the caller's
    /// effort string (or "medium" fallback), with no budget_tokens
    /// leaking into the wire.
    #[test]
    fn adaptive_drops_max_tokens_silently() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("low".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        // budget_tokens MUST NOT leak into the adaptive shape.
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {body:?}"
        );
        // The caller's effort string survives even though the budget
        // was dropped.
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// Real claude-code probe shape: `max_tokens=64` + operator
    /// `effort="high"`. The legacy `Enabled` wire shape would emit
    /// `budget_tokens=51` (64*0.80) which Anthropic 400s on the
    /// `budget_tokens >= 1024` validator. routectl must drop thinking
    /// for this request rather than emit a body that cannot succeed.
    /// Caller's `max_tokens` is preserved verbatim.
    #[test]
    fn small_max_tokens_drops_legacy_thinking() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Companion of the effort=high case for `effort="medium"` (ratio
    /// 0.50): `max_tokens=64` derives `budget_tokens=32`, well below
    /// the 1024 floor. routectl must drop thinking; caller's
    /// `max_tokens` is preserved verbatim (the contract that motivated
    /// rejecting clamp-and-raise).
    #[test]
    fn small_max_tokens_drops_legacy_thinking_effort_medium() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on effort=medium probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Companion for `effort="xhigh"` (ratio 0.95): `max_tokens=64`
    /// derives `budget_tokens=60`, still well below the 1024 floor.
    /// Even at the highest sub-`max` ratio the gate must fire and
    /// the caller's `max_tokens` survives unchanged.
    #[test]
    fn small_max_tokens_drops_legacy_thinking_effort_xhigh() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("xhigh".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on effort=xhigh probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Variant of the above with an explicit sub-1024 `reasoning
    /// .max_tokens`. Even an explicit caller budget must be dropped
    /// when `max_tokens` cannot carry it: emitting `Enabled
    /// { budget_tokens: 500 }` would still 400.
    #[test]
    fn small_max_tokens_drops_thinking_with_explicit_budget() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(500),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(body.get("thinking").is_none());
    }

    /// The adaptive shape is unaffected by the legacy floor: probe-
    /// sized `max_tokens` still receives adaptive thinking because
    /// the wire has no `budget_tokens` field and no Anthropic minimum
    /// to violate. Pins that the new gate is legacy-only.
    #[test]
    fn small_max_tokens_keeps_adaptive() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// `effort="high"` on `max_tokens=1100` looks up the exact table
    /// (24576), which the `[1024, max_tokens-1]` window then clamps down
    /// to the ceiling `max_tokens-1 = 1099`. 1099 < 1100 holds, so
    /// Anthropic's `max_tokens > budget_tokens` constraint is satisfied;
    /// visible-output budget shrinks to 1. Pins the ceiling clamp on the
    /// effort path in the just-above-floor band.
    #[test]
    fn effort_budget_ceiling_clamped_in_carryable_band() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1100),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1099);
    }

    /// Boundary: `max_tokens=1025` is the smallest value the gate
    /// admits (`max > MIN`, not `max >= MIN`). Pins the off-by-one
    /// and confirms the clamp lands at exactly 1024.
    #[test]
    fn exactly_1025_max_tokens_keeps_thinking() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1025),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    /// Anthropic also requires `max_tokens > budget_tokens`. A caller
    /// who sends an explicit `reasoning.max_tokens` larger than
    /// `req.max_tokens` would otherwise produce a wire body that
    /// 400s. The clamp caps the budget at `max_tokens - 1`, leaving
    /// at least one visible-output token. Pins that the cap fires on
    /// the explicit-budget arm.
    #[test]
    fn explicit_budget_above_max_tokens_capped_to_max_minus_one() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1100),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(1200),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1099);
        // Anthropic invariant: max_tokens > budget_tokens.
        assert_eq!(body["max_tokens"], 1100);
    }

    /// Caller's `reasoning.max_tokens` of 500 sits BELOW the
    /// Anthropic floor (1024). With `req.max_tokens=2048` the gate
    /// accepts, and the per-arm clamp raises the budget to 1024.
    /// Pins the silent-promotion behavior on the explicit arm; the
    /// accompanying WARN is observable in production via
    /// `ROUTECTL_LOG=routectl=warn`.
    #[test]
    fn explicit_budget_below_floor_clamped_up_to_min() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(500),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    /// `reasoning.enabled = false` short-circuits to `Disabled`
    /// before the new gate runs. Without this pin, a future refactor
    /// that moved the gate above the `enabled=false` check would
    /// silently rewrite an explicit opt-out into absent-thinking.
    #[test]
    fn explicit_disable_wins_over_small_max_tokens() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(false),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    /// Tool-choice translation lives in the egress (different upstreams
    /// want different shapes; the OpenAI ingress passes wire `tool_choice`
    /// through verbatim). Pin the canonical -> Anthropic mapping for
    /// every shape we expect callers to send.
    #[test]
    fn tool_choice_string_auto_translates_to_anthropic_object() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("auto")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"auto"}));
    }

    #[test]
    fn tool_choice_string_required_translates_to_any() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("required")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"any"}));
    }

    #[test]
    fn tool_choice_string_none_drops_field() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected no tools field when caller sent neither tools nor tool_choice"
        );
    }

    /// `tool_choice = "none"` plus `tools` present must drop BOTH on the
    /// Anthropic wire. Anthropic has no native "none" -- if we send the
    /// tools but no tool_choice, Anthropic defaults to auto-select and
    /// the caller's "do not call tools" intent silently flips to "auto".
    #[test]
    fn tool_choice_none_with_tools_strips_tools_too() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            tools: Some(vec![routectl_core::ToolDef::Custom(
                routectl_core::CustomTool {
                    name: "get_weather".into(),
                    description: Some("weather lookup".into()),
                    input_schema: json!({"type":"object"}),
                    cache_control: None,
                    defer_loading: None,
                    strict: None,
                    type_tag: None,
                },
            )]),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected tools dropped alongside tool_choice=none, got: {body:?}"
        );
    }

    #[test]
    fn tool_choice_function_object_translates_to_anthropic_tool() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!({"type":"function","function":{"name":"get_weather"}})),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({"type":"tool","name":"get_weather"})
        );
    }

    /// Anthropic-shape tool_choice (e.g. from claude-code via Anthropic
    /// ingress) must passthrough verbatim. Without this, the Anthropic
    /// ingress -> Anthropic egress path would double-translate and
    /// silently corrupt the field.
    #[test]
    fn tool_choice_already_anthropic_shape_passes_through_verbatim() {
        for tc in [
            json!({"type":"auto"}),
            json!({"type":"any"}),
            json!({"type":"tool","name":"X"}),
            json!({"type":"none"}),
        ] {
            let req = ChatRequest {
                model: "claude-sonnet-4-5-20250929".into(),
                messages: vec![user_msg("hi")],
                tool_choice: Some(tc.clone()),
                ..Default::default()
            };
            let body = normalize("test", &req, false, &[], false, None).unwrap();
            assert_eq!(body["tool_choice"], tc, "expected passthrough for {tc:?}");
        }
    }

    /// Unknown shapes are not coerced; let the upstream surface its
    /// own error. The OpenAI ingress still passes them through the
    /// canonical body, so the egress sees them here.
    #[test]
    fn tool_choice_unknown_object_passes_through_verbatim() {
        let weird = json!({"type":"some_future_mode","extra":"bag"});
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(weird.clone()),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], weird);
    }

    /// `output_config` arriving via `provider_extras` (the path used
    /// by the Anthropic ingress for structured-output requests) is
    /// merged into the upstream body so `output_config.format` reaches
    /// api.anthropic.com unchanged. The egress doesn't need a
    /// dedicated field for this -- the provider_extras allow-list
    /// already lets `output_config` through.
    #[test]
    fn structured_output_format_merges_from_provider_extras() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            provider_extras: Some(json!({
                "output_config": {
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object"}
                    }
                }
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
    }

    /// Review follow-up to Bug K: when the provider is NOT adaptive
    /// (Sonnet, Haiku -- no adaptive capability declared), the
    /// `output_config.effort` field set by cc must be stripped from
    /// the outgoing body. Anthropic 400s with "This model does not
    /// support the effort parameter" otherwise.
    #[test]
    fn output_config_effort_stripped_on_non_adaptive_provider() {
        let req = ChatRequest {
            model: "claude-haiku-4-5".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {"effort": "high"}
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
        // effort stripped; output_config now empty, so the whole
        // object is removed for wire cleanliness.
        assert!(
            body.get("output_config").is_none(),
            "non-adaptive provider must have output_config removed when effort \
             was the only sub-key, got body: {body}",
        );
    }

    /// Companion to the above: when output_config carries BOTH effort
    /// and a structured-output `format` field, the strip removes only
    /// effort; `format` is preserved (orthogonal to the effort beta
    /// and supported across the model family).
    #[test]
    fn output_config_effort_stripped_preserves_sibling_format_on_non_adaptive() {
        let req = ChatRequest {
            model: "claude-haiku-4-5".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {
                    "effort": "high",
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object", "required": ["x"]}
                    }
                }
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
        let oc = body.get("output_config").expect("output_config preserved");
        assert!(oc.get("effort").is_none(), "effort stripped: {oc}");
        assert_eq!(oc["format"]["type"], "json_schema");
        assert_eq!(oc["format"]["schema"]["required"][0], "x");
    }

    /// Adaptive providers (Opus 4.7 with supports_adaptive_thinking=true)
    /// must preserve `output_config.effort` -- the model accepts it. Pin
    /// this so a future refactor doesn't accidentally strip on the
    /// adaptive path too.
    #[test]
    fn output_config_effort_preserved_on_adaptive_provider() {
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {"effort": "high"}
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();
        let oc = body.get("output_config").expect("output_config preserved");
        assert_eq!(oc["effort"], "high");
    }

    // -----------------------------------------------------------------
    // tool_choice + thinking conflict resolution
    //
    // Anthropic's extended-thinking docs explicitly forbid pairing
    // `thinking` with a `tool_choice` value that forces tool use:
    // `{"type":"any"}` or `{"type":"tool", "name": "..."}`. The
    // Messages API 400s the request with "Thinking may not be enabled
    // when tool_choice forces tool use." Real-world trigger: Claude
    // Code's WebSearch tool fires sub-requests with
    // `tool_choice: {type:"tool", name:"web_search"}` AND
    // `thinking: {type:"adaptive"}`. The strip preserves the caller's
    // tool_choice (which carries intent) and drops thinking (which is
    // a routectl-composed convenience) so the request can complete.
    // -----------------------------------------------------------------

    /// Helper: build a request with both reasoning (-> thinking) and
    /// the provided `tool_choice`. `max_tokens=2048` keeps thinking on
    /// the legacy `Enabled` path above the 1024 floor; the legacy and
    /// adaptive paths share the same conflict resolution.
    fn req_with_thinking_and_tool_choice(tool_choice: Option<Value>) -> ChatRequest {
        use routectl_core::ReasoningConfig;
        ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            tool_choice,
            ..Default::default()
        }
    }

    #[test]
    fn tool_choice_any_with_thinking_strips_thinking() {
        // Arrange
        let req = req_with_thinking_and_tool_choice(Some(json!({"type": "any"})));

        // Act
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        // Assert: thinking dropped, tool_choice preserved verbatim.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped when tool_choice forces tool use, got: {body}"
        );
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn tool_choice_tool_with_thinking_strips_thinking() {
        // Arrange: the Claude Code WebSearch shape that motivated the fix.
        let req =
            req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "web_search"})));

        // Act
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        // Assert: thinking dropped, tool_choice preserved verbatim.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped when tool_choice.type=tool, got: {body}"
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "web_search"})
        );
    }

    #[test]
    fn adaptive_thinking_forced_tool_choice_strips_thinking_and_output_config_effort() {
        // Arrange: the adaptive-thinking path emits both `thinking:
        // {type:adaptive}` AND a top-level `output_config: {effort}`.
        // A forcing tool_choice must strip BOTH -- `output_config.effort`
        // is only valid alongside adaptive thinking, so an orphaned
        // effort 400s.
        let req = req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "ws"})));

        // Act: adaptive=true so build_output_config emits output_config.effort.
        let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();

        // Assert: thinking dropped AND the orphaned effort dropped.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped when tool_choice forces tool use, got: {body}"
        );
        assert!(
            body.get("output_config")
                .and_then(|oc| oc.get("effort"))
                .is_none(),
            "output_config.effort must be stripped alongside thinking, got: {body}"
        );
    }

    #[test]
    fn forced_tool_choice_strips_effort_but_preserves_sibling_format() {
        // Arrange: adaptive output_config.effort plus a structured-output
        // `format` sibling layered in via provider_extras. The strip must
        // drop only effort; format is orthogonal and must survive.
        let mut req =
            req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "ws"})));
        req.provider_extras = Some(json!({
            "output_config": {
                "format": {"type": "json_schema", "schema": {"type": "object"}}
            }
        }));

        // Act
        let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();

        // Assert: thinking + effort gone, format preserved.
        assert!(body.get("thinking").is_none(), "thinking stripped: {body}");
        let oc = body
            .get("output_config")
            .expect("output_config preserved for format");
        assert!(oc.get("effort").is_none(), "effort stripped: {oc}");
        assert_eq!(oc["format"]["type"], "json_schema");
    }

    #[test]
    fn tool_choice_auto_with_thinking_keeps_thinking() {
        // Regression guard: `auto` does not force tool use, so thinking
        // must survive.
        let req = req_with_thinking_and_tool_choice(Some(json!("auto")));

        // translate_tool_choice normalizes bare "auto" -> {"type":"auto"}
        // before strip_thinking_when_tool_choice_forces_use runs.
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tool_choice_none_with_thinking_keeps_thinking() {
        // Regression guard: `none` translates to no tool_choice on the
        // wire AND drops the tools array; thinking is unaffected.
        let req = req_with_thinking_and_tool_choice(Some(json!("none")));

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "tool_choice=none must drop the field"
        );
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn no_tool_choice_with_thinking_keeps_thinking() {
        // Regression guard: absent tool_choice never triggers the strip.
        let req = req_with_thinking_and_tool_choice(None);

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(body.get("tool_choice").is_none() || body["tool_choice"].is_null());
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tool_choice_any_without_thinking_no_op() {
        // Regression guard: when thinking was never composed, the strip
        // is harmless and tool_choice survives.
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            tool_choice: Some(json!({"type": "any"})),
            ..Default::default()
        };

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(body.get("thinking").is_none());
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    // ----------------------------------------------------------------
    // history_reasoning gating of the unsigned-thinking strip.
    //
    // deepseek v4's `/anthropic` endpoint (provider kind anthropic-api)
    // emits thinking blocks WITHOUT a signature yet 400s the next turn
    // unless that thinking is echoed back. `history_reasoning =
    // "preserve"` tells the egress to skip the unsigned-thinking strip
    // for those endpoints; Auto/Strip/unset keep the real-Anthropic-safe
    // strip.
    // ----------------------------------------------------------------

    /// Build a multi-turn assistant message shaped `[text, thinking,
    /// tool_use]`. `signature = None` makes the thinking block unsigned
    /// (deepseek shape); `Some(..)` makes it signed.
    fn assistant_with_thinking(signature: Option<&str>) -> Message {
        use routectl_core::{ContentPart, KnownContentPart};
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "Let me think.".into(),
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::Thinking {
                    thinking: "deepseek reasoning".into(),
                    signature: signature.map(|s| s.to_string()),
                }),
                ContentPart::Known(KnownContentPart::ToolUse {
                    id: "toolu_1".into(),
                    name: "calc".into(),
                    input: json!({"expr": "2+2"}),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Multi-turn request carrying the given `history_reasoning` policy
    /// on the dispatch carrier. `None` mirrors the dispatch default (no
    /// per-model policy resolved).
    fn req_with_hr(hr: Option<CoreHistoryReasoning>, assistant: Message) -> ChatRequest {
        let mut req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![user_msg("compute 2+2"), assistant],
            ..Default::default()
        };
        req.routectl_internal.history_reasoning = hr;
        req
    }

    /// Pull the assistant message's wire content blocks from a
    /// normalized body.
    fn assistant_blocks(body: &Value) -> Vec<Value> {
        body.get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_array())
            .cloned()
            .expect("assistant message with Blocks-form content present")
    }

    fn block_types(blocks: &[Value]) -> Vec<&str> {
        blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect()
    }

    #[test]
    fn preserve_history_reasoning_keeps_unsigned_thinking_for_anthropic_api() {
        // Arrange: deepseek-shape unsigned thinking + history_reasoning =
        // Preserve.
        let req = req_with_hr(
            Some(CoreHistoryReasoning::Preserve),
            assistant_with_thinking(None),
        );

        // Act: normalize under a capture so we can also assert no strip
        // WARN fires.
        let mut body = None;
        let captured = test_capture::with_capture(|| {
            body = Some(
                normalize("deepseek", &req, false, &[], false, None).expect("normalize succeeds"),
            );
        });
        let body = body.expect("normalize ran");

        // Assert: all three blocks survive; the unsigned thinking is
        // preserved (deepseek requires it echoed back).
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "thinking", "tool_use"],
            "Preserve must retain the unsigned thinking block"
        );
        let thinking = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
            .expect("thinking block present under Preserve");
        assert_eq!(thinking["thinking"], "deepseek reasoning");
        // Unsigned: signature serializes as the empty string, not dropped.
        assert_eq!(thinking["signature"], "");

        // No strip => no WARN.
        assert!(
            !captured
                .iter()
                .any(|e| e.message.contains("stripping unsigned thinking blocks")),
            "Preserve must not emit the strip WARN; got events: {captured:?}"
        );
    }

    #[test]
    fn strip_mode_still_strips_unsigned_thinking() {
        // Arrange.
        let req = req_with_hr(
            Some(CoreHistoryReasoning::Strip),
            assistant_with_thinking(None),
        );

        // Act.
        let body =
            normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

        // Assert: unsigned thinking removed, text + tool_use survive.
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "tool_use"],
            "Strip must drop the unsigned thinking block"
        );
    }

    #[test]
    fn auto_and_unset_default_to_strip() {
        // The dispatch default (None) and explicit Auto both resolve to
        // strip for the anthropic-api egress: there is no dialect-default
        // concept here, so Auto means strip (real-Anthropic-safe). Pins
        // that the default path is unchanged by the Preserve gate.
        for hr in [None, Some(CoreHistoryReasoning::Auto)] {
            // Arrange.
            let req = req_with_hr(hr, assistant_with_thinking(None));

            // Act.
            let body =
                normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

            // Assert.
            let blocks = assistant_blocks(&body);
            assert_eq!(
                block_types(&blocks),
                vec!["text", "tool_use"],
                "Auto/unset ({hr:?}) must strip unsigned thinking"
            );
        }
    }

    #[test]
    fn signed_thinking_passes_through_in_all_modes() {
        // A SIGNED thinking block is never the target of the
        // unsigned-strip, so it survives under both Preserve and Strip.
        // Pins that the gate only ever affects unsigned blocks.
        let sig = claude_signature();
        for hr in [CoreHistoryReasoning::Preserve, CoreHistoryReasoning::Strip] {
            // Arrange.
            let req = req_with_hr(Some(hr), assistant_with_thinking(Some(&sig)));

            // Act.
            let body =
                normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

            // Assert.
            let blocks = assistant_blocks(&body);
            assert_eq!(
                block_types(&blocks),
                vec!["text", "thinking", "tool_use"],
                "signed thinking must survive under {hr:?}"
            );
            let thinking = blocks
                .iter()
                .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
                .unwrap_or_else(|| panic!("thinking block absent under {hr:?}"));
            assert_eq!(
                thinking["signature"], sig,
                "signed thinking keeps its signature under {hr:?}"
            );
        }
    }

    #[test]
    fn tool_call_id_reject_stays_unconditional_under_preserve() {
        // The tool_result/tool_call_id hard-reject is a separate
        // correctness invariant from the thinking-strip. Preserve must
        // NOT relax it: a Role::Tool message lacking tool_call_id still
        // errors regardless of history_reasoning.
        let mut req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("result content".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        req.routectl_internal.history_reasoning = Some(CoreHistoryReasoning::Preserve);

        let err = normalize("deepseek", &req, false, &[], false, None).unwrap_err();
        assert!(
            err.to_string().contains("tool_call_id"),
            "must reject missing tool_call_id even under Preserve; got: {err}"
        );
    }

    /// `routectl_internal` field path consulted: when `supports_adaptive_thinking`
    /// is read from `req.routectl_internal` and is `true`, the adaptive wire
    /// shape is emitted. This pins that normalize reads the canonical internal
    /// carrier rather than a hardcoded literal passed by the caller.
    #[test]
    fn normalize_reads_supports_adaptive_thinking_from_routectl_internal() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(8192),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Set the flag via the routectl_internal carrier (not a parameter).
        req.routectl_internal.supports_adaptive_thinking = true;

        let body = normalize(
            "test",
            &req,
            req.routectl_internal.supports_adaptive_thinking,
            &[],
            false,
            None,
        )
        .expect("normalize must succeed");

        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(
            thinking["type"], "adaptive",
            "routectl_internal.supports_adaptive_thinking=true must yield adaptive shape"
        );
        assert!(
            thinking.get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens"
        );
    }

    /// Operator cap applied: max_thinking_budget=2000 with max_tokens=10000
    /// clamps the budget DOWN to 2000 before Anthropic's window clamp runs.
    #[test]
    fn max_thinking_budget_nonzero_clamps_budget_down() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(10000),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Operator cap of 2000 < caller's explicit 8000.
        req.routectl_internal.max_thinking_budget = 2000;

        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(
            thinking["budget_tokens"], 2000,
            "max_thinking_budget=2000 must cap the explicit budget of 8000 down to 2000"
        );
    }

    /// No operator cap: max_thinking_budget=0 passes the budget through
    /// unchanged (only Anthropic's window clamp applies).
    #[test]
    fn max_thinking_budget_zero_no_op() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(10000),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(3000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Zero = no operator cap.
        req.routectl_internal.max_thinking_budget = 0;

        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        // budget=3000 fits in [1024, 9999] unchanged.
        assert_eq!(
            thinking["budget_tokens"], 3000,
            "max_thinking_budget=0 must not alter the budget; got {thinking:?}"
        );
    }

    // -----------------------------------------------------------------
    // emit_reasoning_blocks: non-anthropic format WARN (Finding 2)
    // -----------------------------------------------------------------

    /// When `emit_reasoning_blocks` encounters reasoning details whose
    /// `format` is not `anthropic-claude-v1` it must drop them (behavior-
    /// preserving) AND emit a structured WARN that aggregates the skipped
    /// count and the distinct format strings so operators can diagnose why
    /// blocks are absent from the replay.
    #[test]
    fn emit_reasoning_blocks_warns_on_non_anthropic_format() {
        // Arrange: assistant message with two reasoning details that carry
        // non-anthropic formats (one foreign string, one absent / None).
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![
                user_msg("think then reply"),
                Message {
                    refusal: None,
                    role: Role::Assistant,
                    content: MessageContent::Text("I thought about it.".into()),
                    reasoning: None,
                    reasoning_details: vec![
                        ReasoningDetail {
                            kind: ReasoningDetailKind::Text,
                            id: None,
                            format: Some("openai-o-format".to_string()),
                            index: Some(0),
                            payload: json!({"text": "some reasoning", "signature": "sig"}),
                        },
                        ReasoningDetail {
                            kind: ReasoningDetailKind::Encrypted,
                            id: None,
                            // format = None -> not anthropic-claude-v1 -> must also be skipped
                            format: None,
                            index: Some(1),
                            payload: json!({"data": "encrypted-blob"}),
                        },
                    ],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        // Act: normalize under capture to observe the emitted WARN.
        let mut body_out: Option<Value> = None;
        let captured = test_capture::with_capture(|| {
            body_out = Some(
                normalize("prov-test", &req, false, &[], false, None)
                    .expect("normalize must succeed"),
            );
        });
        let _body = body_out.expect("normalize ran");

        // Assert: the skipped-format WARN must be emitted.
        let warn_event = captured
            .iter()
            .find(|e| {
                e.message.contains(
                    "skipping reasoning blocks on replay: format is not anthropic-claude-v1",
                )
            })
            .unwrap_or_else(|| {
                panic!("expected non-anthropic-format WARN; got events: {captured:?}",)
            });
        assert_eq!(warn_event.level, tracing::Level::WARN);

        // provider field must identify the caller.
        let provider_val = warn_event
            .fields
            .iter()
            .find(|(k, _)| k == "provider")
            .map(|(_, v)| v.as_str())
            .expect("provider field present");
        assert_eq!(provider_val, "prov-test");

        // skipped_count: both details were dropped.
        let count_val = warn_event
            .fields
            .iter()
            .find(|(k, _)| k == "skipped_count")
            .map(|(_, v)| v.as_str())
            .expect("skipped_count field present");
        assert_eq!(count_val, "2", "both non-anthropic details must be counted");

        // skipped_formats: must contain the foreign format string and the
        // placeholder for the absent format.
        let formats_val = warn_event
            .fields
            .iter()
            .find(|(k, _)| k == "skipped_formats")
            .map(|(_, v)| v.as_str())
            .expect("skipped_formats field present");
        assert!(
            formats_val.contains("openai-o-format"),
            "skipped_formats must include the foreign format string; got: {formats_val:?}",
        );
        assert!(
            formats_val.contains("<none>"),
            "skipped_formats must include <none> for format=None details; got: {formats_val:?}",
        );
    }

    /// Claude 4.x rejects a body carrying both sampling knobs. When the
    /// caller sends both, temperature wins and top_p is dropped.
    #[test]
    fn drops_top_p_when_temperature_also_set() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(256),
            temperature: Some(0.7),
            top_p: Some(0.9),
            ..Default::default()
        };

        // Act
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        // Assert
        assert_eq!(body["temperature"], 0.7);
        assert!(
            body.get("top_p").is_none(),
            "top_p must be dropped when temperature is set, got {body:?}"
        );
    }

    /// With only top_p set the body carries top_p and no temperature.
    #[test]
    fn keeps_top_p_when_temperature_unset() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(256),
            temperature: None,
            top_p: Some(0.9),
            ..Default::default()
        };

        // Act
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        // Assert
        assert_eq!(body["top_p"], 0.9);
        assert!(
            body.get("temperature").is_none(),
            "temperature must be absent when only top_p is set, got {body:?}"
        );
    }

    /// With only temperature set the body carries temperature and no top_p.
    #[test]
    fn keeps_temperature_when_top_p_unset() {
        // Arrange
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(256),
            temperature: Some(0.3),
            top_p: None,
            ..Default::default()
        };

        // Act
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        // Assert
        assert_eq!(body["temperature"], 0.3);
        assert!(
            body.get("top_p").is_none(),
            "top_p must be absent when only temperature is set, got {body:?}"
        );
    }

    /// Thinking forces temperature to 1.0; top_p must then be dropped too,
    /// since Anthropic also rejects top_p while thinking is active.
    #[test]
    fn drops_top_p_when_thinking_forces_temperature() {
        use routectl_core::ReasoningConfig;
        // Arrange
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            top_p: Some(0.9),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };

        // Act
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();

        // Assert
        assert_eq!(body["temperature"], 1.0);
        assert!(
            body.get("top_p").is_none(),
            "top_p must be dropped while thinking is active, got {body:?}"
        );
    }
}

// -----------------------------------------------------------------
// Anthropic effort clamping: operator-declared effort_levels must
// cap the caller's effort on the Anthropic-shape egress (adaptive
// and legacy) matching the existing OpenAI-shape behavior.
// -----------------------------------------------------------------
#[cfg(test)]
mod anthropic_effort_clamp_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

    fn user_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Operator declares effort_levels = ["low","medium","high"] on an
    /// Anthropic adaptive model. Caller sends effort="max". The outgoing
    /// output_config.effort must be "high" (clamped down to the operator
    /// cap), not "max".
    #[test]
    fn adaptive_clamps_effort_to_operator_cap() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped from "max" down to "high" (operator cap).
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "high",
            "effort must clamp from max to high against operator-declared effort_levels; got: {oc}"
        );
    }

    /// Operator declares effort_levels = [] (empty). Caller sends
    /// effort="max". The outgoing output_config.effort must be "max"
    /// (pass-through; current Anthropic behavior).
    #[test]
    fn adaptive_passthrough_when_effort_levels_empty() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Empty = pass-through semantics (default).
        req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort passes through unchanged.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "max",
            "empty effort_levels must not clamp; got: {oc}"
        );
    }

    /// Operator declares effort_levels = ["low","medium"] on an
    /// Anthropic legacy (non-adaptive) model. Caller sends effort="high".
    /// The legacy budget must be derived from "medium" (clamped down to
    /// the operator cap), not "high".
    ///
    /// Concretely: max_tokens=4096. The exact table maps "medium" to
    /// 8192, which the `[1024, max_tokens-1]` window then clamps to
    /// 4095. The high band (24576) would clamp to the same ceiling, so
    /// the cost cap is observed at the table-lookup layer: this test
    /// pins that effort is clamped to "medium" before the budget lookup.
    #[test]
    fn legacy_clamps_effort_to_operator_cost_cap() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(4096),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        req.routectl_internal.effort_levels =
            std::sync::Arc::from(vec!["low".to_string(), "medium".to_string()]);

        // Act
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");

        // Assert: "medium" table budget 8192 window-clamped to 4095.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(
            thinking["budget_tokens"], 4095,
            "legacy path must clamp effort from high to medium against operator cap; got: {thinking}"
        );
    }

    /// Companion to `adaptive_clamps_effort_to_operator_cap`: the clamp
    /// must hold even when the caller's raw `output_config.effort`
    /// arrives via `provider_extras`. claude-code 2.1.153+ sends
    /// `output_config: {effort: "max"}` on every request; the Anthropic
    /// ingress preserves the whole `output_config` object verbatim in
    /// `provider_extras` so the orthogonal `output_config.format`
    /// sub-key (structured-output) passes through. derive_effort clamps
    /// "max" -> "high" on the typed struct, but merge_provider_extras
    /// then overwrites the clamped wire value with the raw caller
    /// value. Without a re-clamp on the adaptive branch of
    /// reconcile_output_config_effort, the operator's effort_levels
    /// cap is silently bypassed.
    ///
    /// The pre-existing `adaptive_clamps_effort_to_operator_cap` test
    /// leaves `provider_extras=None` so `merge_provider_extras` early-
    /// returns and the bug is masked; the
    /// `output_config_effort_preserved_on_adaptive_provider` test has
    /// empty `effort_levels` so there is no cap to violate. This test
    /// pins both: non-empty `effort_levels` AND raw `output_config.effort`
    /// in `provider_extras`.
    #[test]
    fn adaptive_clamps_effort_to_operator_cap_even_when_provider_extras_carries_raw() {
        use serde_json::json;

        // Arrange: caller asks for effort="max" both via the canonical
        // lift (req.reasoning) and via the raw output_config that the
        // ingress mirrored into provider_extras (claude-code shape);
        // operator caps effort_levels at "high".
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {"effort": "max"}
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped to "high" even though raw "max" was
        // layered back in by merge_provider_extras.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "high",
            "effort_levels cap (high) must override caller-supplied output_config.effort=max \
             even when carried via provider_extras; got: {oc}"
        );
    }

    /// Companion: empty effort_levels = intentional pass-through, no
    /// re-clamp. Even when provider_extras carries
    /// `output_config.effort = "max"`, an operator who declared
    /// `effort_levels = []` (or omitted it) wants the raw value to flow
    /// through verbatim.
    #[test]
    fn adaptive_passes_through_provider_extras_effort_when_levels_empty() {
        use serde_json::json;

        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {"effort": "max"}
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "max",
            "empty effort_levels must pass provider_extras output_config.effort through unchanged; got: {oc}"
        );
    }

    /// Companion: `output_config.format` (structured-output) and other
    /// sibling sub-keys inside `output_config` must continue to flow
    /// through verbatim from provider_extras. The re-clamp must only
    /// touch the `effort` sub-key, never `format`.
    #[test]
    fn adaptive_reclamp_preserves_sibling_output_config_keys() {
        use serde_json::json;

        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {
                    "effort": "max",
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object", "required": ["x"]}
                    }
                }
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped, format preserved verbatim.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(oc["effort"], "high", "effort must clamp; got: {oc}");
        assert_eq!(oc["format"]["type"], "json_schema");
        assert_eq!(oc["format"]["schema"]["required"][0], "x");
    }

    #[test]
    fn output_config_is_not_routectl_managed() {
        // Pinning this invariant: output_config must remain a non-managed
        // key so provider_extras-carried sub-fields like
        // `output_config.format` flow through verbatim. The adaptive-branch
        // re-clamp at reconcile_output_config_effort relies on output_config
        // surviving merge_provider_extras intact.
        assert!(!is_routectl_managed_key("output_config"));
    }
}

// -----------------------------------------------------------------
// effort_ratio parity test: every token in VALID_EFFORT_TOKENS must
// have a non-default arm in effort_ratio. Guards against a new token
// being added to the const without a matching arm, which would
// silently return the 0.50 default ratio.
// -----------------------------------------------------------------
#[cfg(test)]
mod effort_ratio_parity_tests {
    use super::effort_ratio;
    use crate::effort::VALID_EFFORT_TOKENS;

    /// Assert that every token listed in VALID_EFFORT_TOKENS returns a
    /// ratio distinct from the default fallback arm (0.50). The only
    /// token that should legitimately equal 0.50 is "medium". All
    /// others must have a dedicated arm.
    ///
    /// If a new token is added to VALID_EFFORT_TOKENS without a
    /// matching arm in effort_ratio, it will silently receive 0.50
    /// (the default). This test surfaces that gap.
    #[test]
    fn every_valid_effort_token_has_non_default_ratio_or_is_medium() {
        // Tokens that are EXPECTED to map to 0.50 (the default ratio).
        // Only "medium" is intentional.
        const EXPECTED_DEFAULT: &[&str] = &["medium"];

        for &token in &VALID_EFFORT_TOKENS {
            let ratio = effort_ratio(token);
            if EXPECTED_DEFAULT.contains(&token) {
                // "medium" is intentionally 0.50.
                assert_eq!(
                    ratio, 0.50,
                    "token \"{token}\" expected 0.50 but got {ratio}"
                );
            } else {
                // All other tokens must have a dedicated arm (not the 0.50 default).
                assert_ne!(
                    ratio, 0.50,
                    "token \"{token}\" maps to the default ratio 0.50; \
                     add a dedicated arm to effort_ratio for this token"
                );
            }
        }
    }
}
