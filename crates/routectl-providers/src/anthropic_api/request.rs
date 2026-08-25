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
//!   Blocks). The `Role::System` lift is the `req.system`-ABSENT path
//!   only: when no canonical system is supplied, any Role::System
//!   messages in `req.messages` get lifted so direct callers without an
//!   ingress aren't broken. When a canonical system IS supplied, the lift
//!   does not run and those turns instead ride the wire in place as
//!   `role: "system"` messages (see `messages::SystemTurnPolicy`).
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
use super::messages::{SystemTurnPolicy, normalize_replay_invariants, translate_messages};
use super::tools::{apply_parallel_tool_use, parallel_tool_calls_extra, translate_tool_choice};

// Re-exports for callers outside this module. The Bedrock egress reuses
// the canonical-side Anthropic-shape primitives via
// `crate::anthropic_api::request::<name>`, and `mod.rs` reaches
// `filter_anthropic_betas` the same way; keeping these paths stable
// means those call sites need no edits across the file split.
pub(crate) use super::extras::{build_thinking, filter_anthropic_betas};
pub(crate) use super::system::translate_system;
// The structured-outputs body-beta carrier is applied by the body-shape
// Bedrock-Invoke egress AFTER its own beta allowlist filter, not here --
// see `apply_structured_outputs_beta_to_body`.
#[cfg(feature = "bedrock")]
pub(crate) use super::extras::apply_structured_outputs_beta_to_body;
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
// Sampling clamp (shared with the Bedrock egresses)
// ---------------------------------------------------------------------------

/// Clamp sampling params for Claude thinking mode. Anthropic requires
/// `temperature = 1.0` when thinking is enabled (legacy `Enabled` and
/// `Adaptive` both): no alternative-continuation sampling while spending
/// reasoning budget. It also rejects a request carrying both `temperature`
/// and `top_p` (and rejects `top_p` while thinking is active), so `top_p`
/// survives only when no temperature is in play; temperature wins.
///
/// Shared by the Anthropic-API egress (`normalize` below, inherited by the
/// Bedrock-Invoke seam) and the Bedrock-Converse `inferenceConfig` builder so
/// the clamp cannot drift between the two seams that build sampling
/// independently. Returns `(temperature, top_p)`.
pub(crate) const fn clamp_sampling_for_thinking(
    thinking: Option<&ThinkingConfig>,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let temperature = match thinking {
        Some(ThinkingConfig::Enabled { .. } | ThinkingConfig::Adaptive { .. }) => Some(1.0f64),
        _ => temperature,
    };
    let top_p = if temperature.is_some() { None } else { top_p };
    (temperature, top_p)
}

/// Map the canonical OpenAI-shape `response_format` onto the Anthropic-shape
/// `output_config.format` object. This is the inverse of the openai-compat
/// wire-lift (`openai_compat::wire_lift::response_format`):
///
///   `{type:json_schema, json_schema:{schema, name?, strict?}}`
///       -> `{type:json_schema, schema}`
///   `{type:json_object}` -> `{type:json_object}`
///
/// `name` and `strict` are NOT carried across: Anthropic's
/// `output_config.format` accepts only `type` and `schema`, and a body
/// carrying either key is rejected outright (measured 2026-08-11: HTTP 400
/// `output_config.format.name: Extra inputs are not permitted`, same for
/// `.strict`). The caller's `json_schema.schema` rides through untouched.
/// See `drop_unrepresentable_output_format_keys`, which closes the same gap
/// for a caller-supplied `output_config.format` that never passes through
/// here.
///
/// Returns the mapped format plus which unrepresentable keys the source
/// carried, so the caller can fold them into the single per-request
/// diagnostic. Returns `None` for an absent or unrecognized shape so the
/// caller emits nothing. Shared with the Bedrock-Converse bag builder so both
/// Claude seams map the directive the same way.
pub(crate) fn response_format_to_anthropic_format(
    response_format: &Value,
) -> Option<(Value, DroppedFormatKeys)> {
    let Some(obj) = response_format.as_object() else {
        tracing::warn!(
            "response_format is not an object; dropping structured-output \
             directive on Anthropic egress"
        );
        return None;
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        tracing::warn!(
            "response_format carries no string type token; dropping \
             structured-output directive on Anthropic egress"
        );
        return None;
    };
    match kind {
        "json_schema" => {
            let Some(js) = obj.get("json_schema").and_then(Value::as_object) else {
                tracing::warn!(
                    "response_format json_schema is absent or not an object; \
                     dropping structured-output directive on Anthropic egress"
                );
                return None;
            };
            let Some(schema) = js.get("schema").cloned() else {
                tracing::warn!(
                    "response_format json_schema carries no json_schema.schema; \
                     dropping structured-output directive on Anthropic egress"
                );
                return None;
            };
            let mut format = serde_json::Map::new();
            format.insert("type".into(), Value::from("json_schema"));
            format.insert("schema".into(), schema);
            let dropped = DroppedFormatKeys {
                name: js.contains_key("name"),
                strict: js.contains_key("strict"),
            };
            Some((Value::Object(format), dropped))
        }
        "json_object" => Some((
            serde_json::json!({"type": "json_object"}),
            DroppedFormatKeys::default(),
        )),
        other => {
            tracing::warn!(
                response_format_type = other,
                "unrecognized response_format shape; dropping structured-output \
                 directive on Anthropic egress"
            );
            None
        }
    }
}

/// Insert `format` under `output_config.format` in `obj`, preserving any
/// existing `output_config` sub-keys (e.g. `effort`). A `format` already
/// present is left untouched (a caller-supplied `output_config.format` wins
/// over the canonical `response_format`). Creates `output_config` when
/// absent. Shared with the Bedrock-Converse bag builder.
pub(crate) fn set_output_config_format(obj: &mut serde_json::Map<String, Value>, format: Value) {
    let oc = obj
        .entry("output_config")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !oc.is_object() {
        // A pre-existing non-object output_config (null / scalar / array,
        // e.g. from a malformed provider_extras forward-compat sweep) cannot
        // carry a `format` sibling. Anthropic requires output_config to be an
        // object, so replace the malformed value with a fresh object rather
        // than silently dropping the caller's structured-output directive.
        *oc = Value::Object(serde_json::Map::new());
    }
    if let Some(oc_obj) = oc.as_object_mut() {
        oc_obj.entry("format").or_insert(format);
    }
}

/// Structured event name for the `output_config.format` key drop, so an
/// operator can filter the diagnostic without matching on message text.
pub(crate) const OUTPUT_FORMAT_KEY_DROP_EVENT: &str = "output_config_format_keys_dropped";

/// Which `output_config.format` keys a normalization omitted, accumulated
/// across the two paths that can carry them (the `response_format` converter
/// and the assembled-body scrub) so a request yields ONE diagnostic rather
/// than one per path.
///
/// A fixed pair of static field names, so no sampling: `BoundedLogSample`
/// exists to cap arbitrarily-large diagnostic collections and would be
/// cargo-culting here.
#[derive(Clone, Copy, Default)]
pub(crate) struct DroppedFormatKeys {
    name: bool,
    strict: bool,
}

impl DroppedFormatKeys {
    /// Union of two paths' omissions.
    pub(crate) const fn merged(self, other: Self) -> Self {
        Self {
            name: self.name || other.name,
            strict: self.strict || other.strict,
        }
    }

    /// Emit the aggregated WARN, if either key was omitted. Called exactly
    /// once per normalization, after every path that can carry the keys has
    /// run.
    ///
    /// The text states only what is certain: the keys are not representable
    /// in Anthropic's `output_config.format`, so they were omitted. It must
    /// NOT claim that output validation was weakened -- Anthropic's format
    /// object has no `strict` member to honor in the first place, so what the
    /// omission changes upstream is not something routectl can assert.
    pub(crate) fn warn(self, provider: &str) {
        if !self.name && !self.strict {
            return;
        }
        // Never log the caller's `name` VALUE -- it is caller-controlled.
        tracing::warn!(
            provider = provider,
            event = OUTPUT_FORMAT_KEY_DROP_EVENT,
            dropped_name = self.name,
            dropped_strict = self.strict,
            "output_config.format keys omitted: Anthropic's structured-output \
             format object cannot represent them (it accepts `type` and \
             `schema`) and rejects a body carrying either. The caller's schema \
             ships unchanged."
        );
    }
}

/// The two deferred `output_config` diagnostics of ONE normalization, kept
/// together so a seam that keeps writing to `output_config` after assembly
/// cannot emit one of them and forget the other.
///
/// Both records defer for the same reason: the Bedrock-Invoke
/// `additional_model_request_fields` merge runs after assembly and can
/// replace `output_config` wholesale, so that seam re-runs BOTH passes on the
/// body it actually ships and folds the second run's records in here before
/// emitting once. See [`normalize_deferring_format_key_warn`].
pub(crate) struct DeferredOutputConfigDiagnostics {
    dropped_format_keys: DroppedFormatKeys,
    repair: super::output_schema::AdditionalPropertiesRepair,
}

impl DeferredOutputConfigDiagnostics {
    /// Re-run both `output_config` passes on a body rewritten after assembly
    /// and fold their records into this one, so the request still yields one
    /// WARN per diagnostic however many times the passes ran.
    ///
    /// Bedrock-gated: `bedrock/invoke.rs` is its only caller.
    #[cfg(feature = "bedrock")]
    pub(crate) fn rescanning(
        mut self,
        provider: &str,
        obj: &mut serde_json::Map<String, Value>,
    ) -> Result<Self> {
        self.dropped_format_keys = self
            .dropped_format_keys
            .merged(drop_unrepresentable_output_format_keys(obj));
        self.repair = self
            .repair
            .merged(super::output_schema::inject_additional_properties_false(
                provider, obj,
            )?);
        Ok(self)
    }

    /// Emit both aggregated WARNs. Called exactly once per request, after
    /// every pass that can write `output_config` has run.
    pub(crate) fn warn(&self, provider: &str) {
        self.dropped_format_keys.warn(provider);
        self.repair.warn(provider);
    }
}

/// Remove `name` and `strict` from `output_config.format` on an ASSEMBLED
/// body (or Converse bag), whatever path put them there. Reports what it
/// removed; the caller aggregates and emits the single WARN.
///
/// Anthropic's `output_config.format` accepts only `type` and `schema`;
/// measured 2026-08-11 against the live wire, a body carrying either key is
/// rejected with HTTP 400 (`output_config.format.name: Extra inputs are not
/// permitted`, same for `.strict`) while the bare `{type, schema}` shape is
/// accepted. `{name, schema, strict}` is the conventional OpenAI-shape
/// structured-output request, so this is the common path, not an edge case.
///
/// Reading the assembled object is load-bearing rather than belt-and-braces:
/// `output_config` is deliberately not a routectl-managed key (see
/// `extras::is_routectl_managed_key`), so `merge_provider_extras` forwards a
/// caller's whole `output_config` verbatim and
/// `set_output_config_format`'s `or_insert` leaves it untouched -- a
/// caller-supplied `output_config.format` never passes through
/// `response_format_to_anthropic_format` at all. Same posture as
/// `extras::body_has_output_config_format` and
/// `reconcile_output_config_effort`, and for the same reason.
///
/// Applies uniformly to all three Claude seams (Anthropic egress,
/// Bedrock-Converse bag, Bedrock-Invoke via the shared normalizer). Bedrock
/// acceptance of the two keys was MEASURED (2026-08-12, us-west-2,
/// claude-haiku-4-5, both lanes direct to AWS): Bedrock rejects both with the
/// identical "Extra inputs are not permitted" string a valid json_schema
/// format is otherwise accepted (200) -- because AWS forwards the field
/// verbatim to the same Anthropic validator. So the uniform drop is correct
/// and the seams do not diverge; if that ever changes, re-splitting is a
/// change to this one function and its three call sites.
pub(crate) fn drop_unrepresentable_output_format_keys(
    obj: &mut serde_json::Map<String, Value>,
) -> DroppedFormatKeys {
    let Some(format) = obj
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
        .and_then(|oc| oc.get_mut("format"))
        .and_then(Value::as_object_mut)
    else {
        return DroppedFormatKeys::default();
    };
    DroppedFormatKeys {
        name: format.remove("name").is_some(),
        strict: format.remove("strict").is_some(),
    }
}

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
/// flattens its cache_control away (that lift runs only when NO canonical
/// `req.system` is present; with one present the system turns ride the wire in
/// place and keep their per-block markers), and consecutive `Role::Tool` turns
/// collapse into one message of unmarked `ToolResult` blocks -- so this walk
/// counts what ACTUALLY ships. It is load-bearing and is NOT replaceable with
/// `validate_source(req)` on the
/// canonical request: that would change the cap/ordering outcome for every
/// suppressed / stripped / lifted / collapsed request. The canonical
/// pre-assembly walk lives in routectl-core cache_control.rs
/// (`CacheBreakpointSource for ChatRequest`), whose doc comment points back
/// here for this list rather than restating it.
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
        for m in &*self.messages {
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
///
/// `terminal_anthropic_host` states whether this body egresses to the
/// GENUINE Anthropic host. It is the resolved value of
/// `routectl_core::identity::anthropic::is_anthropic_api_host` on the
/// egress base_url -- resolved once by the caller that owns the base_url,
/// never re-derived here or below. It gates exactly one wire-byte
/// difference: a routectl reasoning envelope inside a `redacted_thinking`
/// block ships as its unwrapped inner blob on the terminal host and
/// byte-for-byte everywhere else. Callers with no Anthropic host in play
/// (the Bedrock Invoke lane) pass `false`.
/// Assemble the body and RETURN the deferred `output_config` diagnostics
/// instead of emitting them, for callers that keep writing to
/// `output_config` after this returns.
///
/// Bedrock Invoke is the only such caller: it merges
/// `additional_model_request_fields` AFTER normalization, a post-normalize
/// write path `is_bedrock_invoke_managed_key` does not cover for
/// `output_config`, so an operator-supplied object can both reintroduce the
/// unrepresentable keys this pass removed and replace the repaired schema
/// wholesale. It re-runs both passes on the body it ships and emits ONE WARN
/// per diagnostic covering both sources. Emitting here as well would
/// double-warn for a single request.
///
/// `forward_system_turns` states whether this lane targets the Anthropic
/// Messages API, which accepts a mid-conversation `role: "system"` turn in
/// `messages[]`. When true and a canonical `req.system` is shipping, the
/// legacy lift does not run and each `Role::System` turn rides the wire in
/// place. When false the turns are treated as lift-consumed, so a lane whose
/// target API has no such role never ships one. Stated at each call site, not
/// defaulted.
///
/// Every other caller wants [`normalize`], which emits before returning.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub(crate) fn normalize_deferring_format_key_warn(
    id: &str,
    req: &ChatRequest,
    adaptive: bool,
    allowed_betas: &[String],
    context_management: bool,
    thinking_cache: Option<
        &std::sync::RwLock<crate::anthropic_api::context_management::ThinkingCache>,
    >,
    terminal_anthropic_host: bool,
    forward_system_turns: bool,
) -> Result<(Value, DeferredOutputConfigDiagnostics)> {
    // The canonical sampling knobs have no Anthropic Messages home and are
    // gated out of the provider_extras merge as canonical keys; WARN once so
    // the loss isn't silent. Bedrock-Invoke delegates body construction here,
    // so this single call also covers that lane (with its own provider id).
    crate::sampling_drop_guard::warn_dropped_sampling_fields(id, req, &[]);

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
        // A blank canonical system (`"system": ""`, whitespace-only, or
        // blocks whose every text is blank) is treated as "no canonical
        // system supplied" -- the same reading as None -- so it falls
        // through to the Role::System lift below rather than suppressing
        // it. It carries no instruction, so it must never reach the wire as
        // `system: ""`, and it must not silently discard a system prompt a
        // direct caller put in the messages array.
        .filter(|s| !s.is_blank())
        .and_then(|s| crate::system_filter::strip_billing_attribution(s, &mut billing_dropped));
    if billing_dropped {
        tracing::warn!(
            provider = id,
            "anthropic-api egress: Claude Code billing/attribution system block dropped",
        );
    }

    // The wire system field and the messages array are two halves of one
    // decision, so the discriminator is resolved ONCE here from the same
    // filtered canonical system the system-field branch below consumes.
    // With a canonical system present the lift never ran, so nothing else
    // owns the Role::System turns and they ride the wire in place; with it
    // absent the lift consumed them. Resolved BEFORE the replay-invariant
    // walk because that walk's whole-turn drop is positional: it must only
    // refuse a drop after a system turn that actually reaches the wire.
    let system_turns = if forward_system_turns && filtered_system.is_some() {
        SystemTurnPolicy::Forward
    } else {
        SystemTurnPolicy::Lift
    };

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
    let messages = normalize_replay_invariants(id, req, hr, system_turns)?;

    let max_tokens = resolve_max_tokens(req);
    let thinking = build_thinking(req, adaptive);
    let output_config = build_output_config(req, &thinking);

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

    let mut envelopes = crate::anthropic_api::envelope_policy::EnvelopeUnwrapTally::new(
        id,
        terminal_anthropic_host,
    );
    let mut anthropic_messages = translate_messages(id, &messages, system_turns, &mut envelopes)?;

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
                &mut envelopes,
            );
            apply_result.missed_tool_ids
        } else {
            vec![]
        }
    } else {
        vec![]
    };
    // Both channels that can construct a `redacted_thinking` block have
    // run, so the tally is complete: one WARN per request, never one per
    // channel.
    envelopes.flush();

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

    let (temperature, top_p) =
        clamp_sampling_for_thinking(thinking.as_ref(), req.temperature, req.top_p);

    // Fold the OpenAI-dialect `parallel_tool_calls` toggle (riding
    // provider_extras) into Anthropic's native `disable_parallel_tool_use`
    // on the translated tool_choice. `has_wire_tools` reflects the tools
    // that actually ship (post `tool_choice="none"` suppression), so a
    // suppressed request never synthesizes an `auto` carrier. The raw
    // `parallel_tool_calls` key is stripped from the wire in the
    // Anthropic managed-key path (`is_routectl_managed_key`).
    let parallel = parallel_tool_calls_extra(req.provider_extras.as_ref());
    let has_wire_tools = tools.as_ref().is_some_and(|t| !t.is_empty());
    let tool_choice = apply_parallel_tool_use(
        id,
        translate_tool_choice(req.tool_choice.as_ref(), has_tools),
        parallel,
        has_wire_tools,
    );

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
        tool_choice,
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

    // Honor the canonical structured-output directive: map req.response_format
    // (OpenAI-shape) onto Anthropic's output_config.format. Runs after the
    // provider_extras merge so an Anthropic-ingress round-trip that already
    // carried output_config.format keeps its value (caller wins).
    let mut dropped_format_keys = DroppedFormatKeys::default();
    if let Some(rf) = req.response_format.as_ref()
        && let Some((format, dropped)) = response_format_to_anthropic_format(rf)
        && let Some(obj) = body.as_object_mut()
    {
        dropped_format_keys = dropped;
        set_output_config_format(obj, format);
    }

    // Scrub the two keys Anthropic's output_config.format cannot represent,
    // on the assembled body: the converter above no longer emits them, but a
    // caller-supplied output_config rides through provider_extras verbatim and
    // wins over the converter, so this is the only pass that sees that path.
    // The record is RETURNED, not emitted: the emitting wrapper owns the one
    // WARN per request, so a caller that writes to `output_config` after this
    // returns can fold its own scrub in rather than warning twice.
    if let Some(obj) = body.as_object_mut() {
        dropped_format_keys =
            dropped_format_keys.merged(drop_unrepresentable_output_format_keys(obj));
    }

    // Repair the ONE field Anthropic requires on every object in
    // output_config.format.schema: `additionalProperties: false`. Runs on the
    // assembled body AFTER the provider-extras merge and the converter, for
    // the same reason the key scrub above does -- a caller-supplied
    // output_config.format wins via `entry().or_insert()`, so a converter-side
    // repair would be a no-op for exactly the callers that omit the key.
    //
    // The record is RETURNED, not emitted, for the same reason as the key
    // scrub above: the Bedrock-Invoke `additional_model_request_fields` merge
    // runs after this function and can replace `output_config` wholesale, so
    // that seam RE-RUNS the repair on the body it actually ships and folds
    // both records into the one WARN it owns. Emitting here as well would
    // report a single request twice.
    let repair = match body.as_object_mut() {
        Some(obj) => super::output_schema::inject_additional_properties_false(id, obj)?,
        None => super::output_schema::AdditionalPropertiesRepair::default(),
    };

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
    Ok((
        body,
        DeferredOutputConfigDiagnostics {
            dropped_format_keys,
            repair,
        },
    ))
}

/// Assemble the Anthropic Messages body and emit the deferred
/// `output_config` diagnostics.
///
/// The emitting wrapper over [`normalize_deferring_format_key_warn`]: it owns
/// the ONE `output_config` warning per diagnostic for the request. Callers
/// that keep writing to `output_config` after assembly must use the deferring
/// variant and emit once themselves, or the request warns twice.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub(crate) fn normalize(
    id: &str,
    req: &ChatRequest,
    adaptive: bool,
    allowed_betas: &[String],
    context_management: bool,
    thinking_cache: Option<
        &std::sync::RwLock<crate::anthropic_api::context_management::ThinkingCache>,
    >,
    terminal_anthropic_host: bool,
    forward_system_turns: bool,
) -> Result<Value> {
    let (body, deferred) = normalize_deferring_format_key_warn(
        id,
        req,
        adaptive,
        allowed_betas,
        context_management,
        thinking_cache,
        terminal_anthropic_host,
        forward_system_turns,
    )?;
    deferred.warn(id);
    Ok(body)
}

#[cfg(test)]
#[path = "request_allowlist_tests.rs"]
mod allowlist_tests;

// A blank canonical req.system never reaches the wire as `system: ""`.
#[cfg(test)]
#[path = "request_empty_system_tests.rs"]
mod empty_system_tests;

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

// The inbound `thinking.display` carrier survives body assembly and the
// OAuth cloak.
#[cfg(test)]
#[path = "request_thinking_display_carrier_tests.rs"]
mod thinking_display_carrier_tests;

// effort_ratio parity: every token in VALID_EFFORT_TOKENS must have a
// non-default arm in effort_ratio, guarding against a new token silently
// falling through to the 0.50 default.
#[cfg(test)]
#[path = "request_effort_ratio_parity_tests.rs"]
mod effort_ratio_parity_tests;

// response_format honoring: the canonical OpenAI-shape structured-output
// directive maps onto Anthropic's output_config.format.
#[cfg(test)]
mod reasoning_leak_guard_tests {
    use super::normalize;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn user_req() -> ChatRequest {
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
            }]
            .into(),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    #[test]
    fn responses_reasoning_context_mode_dropped() {
        // A Responses-ingress request carrying reasoning context/mode routed
        // to the Anthropic egress does NOT emit them. The fidelity WARN for
        // the drop is emitted router-side, per dispatched target.
        let mut req = user_req();
        req.provider_extras = Some(json!({"reasoning": {"context": "all_turns", "mode": "pro"}}));

        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();

        assert!(body.get("reasoning").is_none());
        assert!(body.get("context").is_none());
        assert!(body.get("mode").is_none());
    }
}

#[cfg(test)]
mod sampling_leak_guard_tests {
    use super::normalize;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use tracing_test::traced_test;

    fn user_req() -> ChatRequest {
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
            }]
            .into(),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    #[test]
    #[traced_test]
    fn sampling_fields_warn_once_naming_dropped_fields() {
        let mut req = user_req();
        req.n = Some(3);
        req.seed = Some(42);
        req.logprobs = Some(true);
        req.top_logprobs = Some(5);
        req.logit_bias = Some(serde_json::json!({"1": -100}));
        req.presence_penalty = Some(0.5);
        req.frequency_penalty = Some(0.25);

        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();

        assert!(body.get("n").is_none());
        assert!(body.get("logprobs").is_none());
        logs_assert(crate::sampling_drop_guard::test_support::exactly_one_sampling_warn);
        // This egress honors none of the seven, so the WARN names all of
        // them -- unaffected by any other egress gaining a translation.
        for name in [
            "\"n\"",
            "\"seed\"",
            "logprobs",
            "top_logprobs",
            "logit_bias",
            "presence_penalty",
            "frequency_penalty",
        ] {
            assert!(logs_contain(name), "WARN must name {name}");
        }
    }

    #[test]
    #[traced_test]
    fn no_sampling_warn_when_no_sampling_field_set() {
        let req = user_req();

        let _ = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();

        assert!(!logs_contain("sampling fields dropped"));
    }
}

#[cfg(test)]
mod response_format_tests {
    use super::normalize;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;
    use tracing_test::traced_test;

    /// The exact member set Anthropic's `output_config.format` accepts for a
    /// json_schema directive. Asserted as a SET rather than snapshotted: an
    /// absent-key assertion is what catches a re-added `name`/`strict`, while
    /// a snapshot merely records whatever shape ships.
    fn assert_json_schema_format_members(fmt: &serde_json::Value) {
        let obj = fmt.as_object().expect("format must be an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["schema", "type"],
            "output_config.format must carry exactly {{type, schema}}; got: {fmt}"
        );
        assert!(
            obj.get("name").is_none(),
            "`name` is rejected by Anthropic and must be absent; got: {fmt}"
        );
        assert!(
            obj.get("strict").is_none(),
            "`strict` is rejected by Anthropic and must be absent; got: {fmt}"
        );
    }

    fn user_req(response_format: Option<serde_json::Value>) -> ChatRequest {
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
            }]
            .into(),
            max_tokens: Some(1024),
            response_format,
            ..Default::default()
        }
    }

    /// The conventional OpenAI-shape structured-output request: `name` and
    /// `strict` alongside the schema. Anthropic 400s on either key
    /// (`Extra inputs are not permitted`), so the emitted format carries the
    /// schema and nothing else.
    #[test]
    fn json_schema_response_format_maps_to_output_config_format() {
        let req = user_req(Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "widget",
                "schema": {"type": "object", "required": ["x"]},
                "strict": true
            }
        })));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        let fmt = &body["output_config"]["format"];
        assert_eq!(fmt["type"], "json_schema", "got: {body}");
        assert_eq!(fmt["schema"]["required"][0], "x", "got: {body}");
        assert_json_schema_format_members(fmt);
    }

    /// The caller's schema is forwarded intact -- only the two unrepresentable
    /// sibling keys are omitted. Discarding a caller's JSON-schema keyword
    /// would be silent constraint loss and is deliberately NOT done.
    ///
    /// `additionalProperties: false` IS added, on the root and on every nested
    /// object: Anthropic rejects an object schema that omits it, and its only
    /// accepted value is `false`, so supplying it discards no caller intent.
    /// Adding a mandatory key is not the same act as dropping a constraint.
    #[test]
    fn caller_schema_keywords_survive_the_key_drop() {
        let schema = json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "minimum": 3}},
            "required": ["n"]
        });
        let req = user_req(Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": schema.clone(), "strict": true}
        })));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        let mut expected = schema;
        expected["additionalProperties"] = json!(false);
        assert_eq!(
            body["output_config"]["format"]["schema"], expected,
            "every caller keyword must survive; only the mandatory \
             additionalProperties is added: {body}"
        );
    }

    /// The `provider_extras` bypass: a caller-supplied `output_config.format`
    /// wins over the canonical `response_format` and so never passes through
    /// `response_format_to_anthropic_format`. The assembled-body scrub is the
    /// only pass that closes this path.
    #[test]
    fn caller_supplied_output_config_format_loses_name_and_strict() {
        let mut req = user_req(None);
        req.provider_extras = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "caller-widget",
                    "schema": {"type": "object"},
                    "strict": true
                }
            }
        }));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        let fmt = &body["output_config"]["format"];
        assert_json_schema_format_members(fmt);
        assert!(
            !body.to_string().contains("caller-widget"),
            "the caller's schema name must not reach the wire: {body}"
        );
    }

    #[test]
    #[traced_test]
    fn one_warn_per_normalization_names_no_caller_value() {
        let req = user_req(Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "secret-widget-name",
                "schema": {"type": "object"},
                "strict": true
            }
        })));

        let _ = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();

        logs_assert(|lines: &[&str]| {
            let matches: Vec<&&str> = lines
                .iter()
                .filter(|l| l.contains(super::OUTPUT_FORMAT_KEY_DROP_EVENT))
                .collect();
            let warns = matches.iter().filter(|l| l.contains("WARN")).count();
            if matches.len() == 1 && warns == 1 {
                return Ok(());
            }
            Err(format!(
                "expected exactly one WARN for the dropped format keys; got \
                 {} line(s), {warns} at WARN: {matches:?}",
                matches.len()
            ))
        });
        assert!(logs_contain("dropped_name=true"));
        assert!(logs_contain("dropped_strict=true"));
        assert!(
            !logs_contain("secret-widget-name"),
            "the caller-controlled schema name must never be logged"
        );
    }

    /// The drop diagnostic is feature-triggered: a conforming directive that
    /// carries neither key produces no WARN.
    #[test]
    #[traced_test]
    fn no_warn_when_neither_key_present() {
        let req = user_req(Some(json!({
            "type": "json_schema",
            "json_schema": {"schema": {"type": "object"}}
        })));

        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();

        assert_json_schema_format_members(&body["output_config"]["format"]);
        assert!(!logs_contain(super::OUTPUT_FORMAT_KEY_DROP_EVENT));
    }

    #[test]
    fn json_object_response_format_maps_to_output_config_format() {
        let req = user_req(Some(json!({"type": "json_object"})));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_object",
            "got: {body}"
        );
    }

    #[test]
    fn text_response_format_emits_no_output_config() {
        // A plain-text directive is not structured output; nothing maps.
        let req = user_req(Some(json!({"type": "text"})));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert!(body.get("output_config").is_none(), "got: {body}");
    }

    #[test]
    fn absent_response_format_emits_no_output_config() {
        let req = user_req(None);
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert!(body.get("output_config").is_none(), "got: {body}");
    }

    #[test]
    fn caller_provider_extras_output_config_format_wins() {
        // An Anthropic-ingress round-trip carries output_config.format in
        // provider_extras; the canonical response_format must not clobber it.
        let mut req = user_req(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "string"}}}
        }));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_schema",
            "provider_extras format must win: {body}"
        );
    }

    #[test]
    fn null_provider_extras_output_config_does_not_drop_response_format() {
        // A malformed forward-compat sweep leaves output_config as JSON null
        // in provider_extras; merge_provider_extras copies it into the body.
        // response_format honoring must still emit output_config.format by
        // replacing the non-object value, not silently no-op.
        let mut req = user_req(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({"output_config": null}));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_object",
            "response_format must survive a null provider_extras output_config: {body}"
        );
    }

    #[test]
    fn scalar_provider_extras_output_config_does_not_drop_response_format() {
        let mut req = user_req(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({"output_config": 7}));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_object",
            "response_format must survive a scalar provider_extras output_config: {body}"
        );
    }

    #[test]
    fn array_provider_extras_output_config_does_not_drop_response_format() {
        let mut req = user_req(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({"output_config": [1, 2, 3]}));
        let body = normalize("anthropic:test", &req, false, &[], false, None, false, true).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_object",
            "response_format must survive an array provider_extras output_config: {body}"
        );
    }
}

// Unit coverage for the shared set_output_config_format helper: a
// pre-existing non-object output_config (null / scalar / array) must be
// replaced with an object carrying the format rather than dropping it.
#[cfg(test)]
mod set_output_config_format_tests {
    use super::set_output_config_format;
    use serde_json::{Map, Value, json};

    fn format() -> Value {
        json!({"type": "json_object"})
    }

    #[test]
    fn creates_output_config_when_absent() {
        let mut obj: Map<String, Value> = Map::new();
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["format"]["type"], "json_object");
    }

    #[test]
    fn preserves_existing_object_siblings() {
        let mut obj: Map<String, Value> = Map::new();
        obj.insert("output_config".into(), json!({"effort": "high"}));
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["effort"], "high");
        assert_eq!(obj["output_config"]["format"]["type"], "json_object");
    }

    #[test]
    fn caller_format_wins_over_response_format() {
        let mut obj: Map<String, Value> = Map::new();
        obj.insert(
            "output_config".into(),
            json!({"format": {"type": "json_schema", "schema": {"type": "string"}}}),
        );
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn replaces_null_output_config() {
        let mut obj: Map<String, Value> = Map::new();
        obj.insert("output_config".into(), Value::Null);
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["format"]["type"], "json_object");
    }

    #[test]
    fn replaces_scalar_output_config() {
        let mut obj: Map<String, Value> = Map::new();
        obj.insert("output_config".into(), json!(7));
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["format"]["type"], "json_object");
    }

    #[test]
    fn replaces_array_output_config() {
        let mut obj: Map<String, Value> = Map::new();
        obj.insert("output_config".into(), json!([1, 2, 3]));
        set_output_config_format(&mut obj, format());
        assert_eq!(obj["output_config"]["format"]["type"], "json_object");
    }
}

// The OpenAI-dialect parallel_tool_calls toggle folds into
// Anthropic's disable_parallel_tool_use on tool_choice, and the raw key
// never reaches the assembled Anthropic body.
#[cfg(test)]
mod parallel_tool_calls_tests {
    use super::normalize;
    use routectl_core::{ChatRequest, Message, MessageContent, Role, ToolDef};
    use serde_json::{Value, json};

    fn tool_req(
        tool_choice: Option<Value>,
        provider_extras: Option<Value>,
        with_tools: bool,
    ) -> ChatRequest {
        let tools = with_tools.then(|| {
            vec![ToolDef::Other(json!({
                "type": "function",
                "function": {"name": "get_weather", "parameters": {"type": "object"}}
            }))]
        });
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
            }]
            .into(),
            max_tokens: Some(1024),
            tools,
            tool_choice,
            provider_extras,
            ..Default::default()
        }
    }

    fn run(req: &ChatRequest) -> Value {
        normalize("anthropic:test", req, false, &[], false, None, false, true).unwrap()
    }

    #[test]
    fn parallel_false_sets_disable_on_existing_choice() {
        let req = tool_req(
            Some(json!("get_weather")),
            Some(json!({"parallel_tool_calls": false})),
            true,
        );
        let body = run(&req);
        assert_eq!(body["tool_choice"]["type"], "tool", "got: {body}");
        assert_eq!(body["tool_choice"]["name"], "get_weather", "got: {body}");
        assert_eq!(
            body["tool_choice"]["disable_parallel_tool_use"], true,
            "got: {body}"
        );
    }

    #[test]
    fn parallel_false_synthesizes_auto_when_no_choice_but_tools() {
        let req = tool_req(None, Some(json!({"parallel_tool_calls": false})), true);
        let body = run(&req);
        assert_eq!(body["tool_choice"]["type"], "auto", "got: {body}");
        assert_eq!(
            body["tool_choice"]["disable_parallel_tool_use"], true,
            "got: {body}"
        );
    }

    #[test]
    fn parallel_true_omits_disable_field() {
        let req = tool_req(
            Some(json!("auto")),
            Some(json!({"parallel_tool_calls": true})),
            true,
        );
        let body = run(&req);
        assert_eq!(body["tool_choice"]["type"], "auto", "got: {body}");
        assert!(
            body["tool_choice"]
                .get("disable_parallel_tool_use")
                .is_none(),
            "Some(true) must not add the field: {body}"
        );
    }

    #[test]
    fn absent_toggle_leaves_native_disable_untouched() {
        // Anthropic-ingress round-trip carried disable_parallel_tool_use;
        // no parallel_tool_calls key means we must not overwrite it.
        let req = tool_req(
            Some(json!({"type": "auto", "disable_parallel_tool_use": true})),
            None,
            true,
        );
        let body = run(&req);
        assert_eq!(
            body["tool_choice"]["disable_parallel_tool_use"], true,
            "native value must survive: {body}"
        );
    }

    #[test]
    fn raw_parallel_tool_calls_key_never_on_body() {
        let req = tool_req(
            Some(json!("get_weather")),
            Some(json!({"parallel_tool_calls": false})),
            true,
        );
        let body = run(&req);
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "raw parallel_tool_calls must be stripped from the Anthropic wire: {body}"
        );
    }
}
