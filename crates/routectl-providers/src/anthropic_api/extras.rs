//! Thinking-budget composition and post-assembly body reconciliation.
//!
//! Two concerns live here. (1) The emitted `thinking`: `compose_thinking`
//! forwards an Anthropic client's own object and otherwise falls back to
//! the canonical `req.reasoning` -> Anthropic `thinking` mapping, where
//! `build_thinking` emits the legacy `Enabled { budget_tokens }` shape and
//! clamps the budget into Anthropic's `[1024, max_tokens-1]` window.
//! `shipped_thinking` then
//! applies the passes that remove composed thinking (a forcing
//! `tool_choice`, a context-management cache miss), so the body and the
//! public `emitted_thinking_type` read share one answer, and
//! `build_output_config` pairs a shipped `Adaptive` with a top-level
//! `output_config`. The Bedrock Converse egress composes its bag through
//! `compose_thinking` and `build_output_config` too. (2) The post-merge
//! body reconciliation: `merge_provider_extras` layers forward-compat
//! extras in while shielding routectl-managed keys
//! (`managed_keys::is_managed_key`), and `reconcile_output_config_effort`
//! supplies the canonical effort beside adaptive thinking that has none.

use serde_json::Value;

use routectl_core::{AnthropicThinking, ChatRequest, ThinkingObject, sanitize_for_log};

use crate::effort::{budget_from_level, wire_effort};
use crate::managed_keys::{Dialect, is_managed_key};

use super::tools::forced_tool_choice_type;
use super::types::{OutputConfig, ThinkingConfig, WireThinking};

/// Hardcoded baseline `max_tokens` value injected on outbound
/// Anthropic-shape requests when the caller omits the field AND no
/// per-model ceiling is known (no catalog prior, no learned ceiling). The
/// Anthropic Messages API requires `max_tokens` and 400s on omission;
/// 64000 is above the 64K ceiling of Sonnet 4.5/4.6 and Opus 4.5 and
/// within Opus 4.7's 128K window.
///
/// A model whose real ceiling is lower gets it from the catalog prior
/// (`routectl catalog set ... max_output_tokens`) or learns it from the
/// upstream's refusal. See docs/CONFIGURATION.md.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 64_000;

/// Resolve the `max_tokens` the request is sized to: caller's request
/// wins; absent that, the router-supplied per-model ceiling from
/// `req.routectl_internal.max_output_tokens` (when non-zero) wins;
/// absent that, the hardcoded `DEFAULT_MAX_OUTPUT_TOKENS` baseline
/// (64000).
///
/// Consumed by Anthropic-shape egresses (anthropic-api +
/// bedrock-invoke; bedrock-invoke delegates body construction to this
/// module's `normalize`) as the wire `max_tokens`, and by the legacy
/// thinking window on those egresses and on bedrock-converse, which
/// calls `build_thinking` itself. Converse, like openai-compat and
/// openai-responses, still forwards `req.max_tokens` cleanly when None
/// per the good-translator principle.
pub(super) const fn resolve_max_tokens(req: &ChatRequest) -> u32 {
    if let Some(v) = req.max_tokens {
        return v;
    }
    let from_internal = req.routectl_internal.max_output_tokens;
    if from_internal > 0 {
        return from_internal;
    }
    DEFAULT_MAX_OUTPUT_TOKENS
}

/// Anthropic Messages API minimum for `thinking.budget_tokens` on the
/// legacy `ThinkingConfig::Enabled` wire shape. Anthropic 400s any
/// value below this AND requires `max_tokens > budget_tokens`. The
/// smallest legal body that carries legacy thinking has
/// `max_tokens > 1024` (1024 budget plus at least 1 visible token).
const ANTHROPIC_MIN_THINKING_BUDGET: u32 = 1024;

/// True iff the caller's `max_tokens` can accommodate the Anthropic
/// minimum thinking budget plus at least one visible-output token.
/// Used by `build_thinking` to drop the legacy `Enabled` shape on
/// probe-sized requests (title generation, topic summaries) instead
/// of emitting a body that Anthropic would 400. The adaptive shape
/// has no equivalent floor and is unaffected.
const fn legacy_thinking_fits(req: &ChatRequest) -> bool {
    let max = resolve_max_tokens(req);
    max > ANTHROPIC_MIN_THINKING_BUDGET
}

/// Proportional budget_tokens as fraction of max_tokens per effort level.
/// Only consulted on the legacy `ThinkingConfig::Enabled` path -- the
/// adaptive-thinking path passes `effort` through verbatim into
/// `output_config.effort` and never calls this.
///
/// Must stay in sync with VALID_EFFORT_TOKENS in
/// routectl-providers/src/effort.rs. A new token added to that const
/// without a corresponding arm here returns the default ratio silently.
pub(super) fn effort_ratio(effort: &str) -> f64 {
    match effort {
        // `max` arrived with the Opus 4.7+ adaptive thinking shape,
        // but the legacy `Enabled { budget_tokens }` path may still
        // see it. 0.99 leaves 1% of max_tokens for the visible response
        // so the request is accepted.
        "max" => 0.99,
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

/// The `output_config.effort` routectl supplies beside adaptive thinking
/// that carries none: the canonical `req.reasoning.effort`, as sent.
fn adaptive_effort(req: &ChatRequest) -> Option<String> {
    req.reasoning.as_ref().and_then(|r| r.effort.clone())
}

/// Translate canonical reasoning into the `ThinkingConfig` to emit, if
/// any: `Disabled` for reasoning-off, else the legacy
/// `Enabled { budget_tokens }` shape when the caller asked for thinking and
/// the resolved `max_tokens` leaves room for Anthropic's minimum budget.
///
/// `thinking.display` comes from
/// `req.routectl_internal.anthropic_thinking_display` when the ingress
/// captured one, and from `reasoning.exclude` otherwise.
pub fn build_thinking(req: &ChatRequest) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    if r.is_off() {
        return Some(ThinkingConfig::Disabled);
    }

    // Absent means absent: no carrier and `reasoning.exclude == None`
    // must leave `thinking.display` off the wire entirely, because
    // Anthropic's default is model-dependent and an explicit value would
    // override whatever a newer model chooses for itself.
    //
    // The carrier wins over the derivation: it holds the string an
    // Anthropic client actually sent, including values the canonical
    // boolean cannot express. The derivation remains for library callers
    // and non-Anthropic ingresses, which set only `exclude`.
    let display = req
        .routectl_internal
        .anthropic_thinking_display
        .clone()
        .or_else(|| match r.exclude {
            Some(true) => Some("omitted".to_string()),
            Some(false) => Some("summarized".to_string()),
            None => None,
        });

    // Did the caller actually ask for thinking? Any of: explicit
    // enabled=true, a budget, an effort string ("none" returned above).
    let thinking_active = r.enabled == Some(true) || r.max_tokens.is_some() || r.effort.is_some();
    if !thinking_active {
        return None;
    }

    // Legacy wire shape constraint: Anthropic requires
    // `budget_tokens >= 1024` AND `max_tokens > budget_tokens`. Probe-
    // sized requests (claude-code title gen, topic summaries) routinely
    // send `max_tokens=64`; emitting any legacy `Enabled` body for
    // those would 400 upstream. Drop thinking for this one request
    // rather than reshape the caller's `max_tokens`.
    if !legacy_thinking_fits(req) {
        tracing::debug!(
            request_max_tokens = req.max_tokens,
            min_required = ANTHROPIC_MIN_THINKING_BUDGET + 1,
            reasoning_effort = ?r.effort,
            reasoning_max_tokens = ?r.max_tokens,
            "anthropic legacy thinking shape requires max_tokens > 1024; \
             dropping thinking for this probe-sized request. Send adaptive \
             thinking to avoid the budget-vs-max_tokens coupling, or send \
             max_tokens > 1024."
        );
        return None;
    }

    // Legacy wire shape: Enabled { budget_tokens }. Translate the
    // canonical signal (explicit budget > effort > enabled=true).
    // Every arm runs the budget through `clamp_budget_to_legacy_window`,
    // which enforces BOTH Anthropic invariants:
    //   - `budget_tokens >= 1024` (floor); a sub-1024 explicit budget
    //     gets raised with a DEBUG line so the silent promotion is
    //     visible. The effort/enabled arms can only land below the
    //     floor in the 1025-1279 (effort=high) band; same clamp.
    //   - `budget_tokens < max_tokens` (ceiling); an explicit budget
    //     that exceeds `req.max_tokens` would otherwise produce a
    //     wire body Anthropic 400s. The clamp caps at
    //     `max.saturating_sub(1)` to leave at least one visible-
    //     output token. The gate above guarantees `max > 1024`, so
    //     the ceiling (`max - 1`) is always at least 1024 and the
    //     floor never collides with the ceiling.
    let max = resolve_max_tokens(req);

    if let Some(budget) = r.max_tokens {
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Explicit),
            display,
        });
    }
    if let Some(effort) = r.effort.as_deref() {
        // `effort != "none"` here (the `is_off` return above excludes it),
        // so `wire_effort` always yields `Some`; fall back to the raw
        // effort defensively if that ever changes.
        let level = wire_effort(effort).unwrap_or(effort);
        // Prefer the exact effort->budget table; fall back to the
        // proportional estimate only for a level outside the table so
        // an unexpected string never regresses to a zero budget.
        let budget = budget_from_level(level)
            .unwrap_or_else(|| ((max as f64) * effort_ratio(level)).max(1.0) as u32);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
            display,
        });
    }
    // r.enabled == Some(true) without budget or effort.
    let budget = max / 2;
    Some(ThinkingConfig::Enabled {
        budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
        display,
    })
}

/// Origin of a `budget_tokens` value about to be clamped to
/// Anthropic's legal `[1024, max_tokens-1]` window. Used to gate
/// whether a silent floor promotion is logged: `Explicit` means
/// the caller asked for a specific number and we are about to ignore
/// it -- worth a log line; `Derived` means routectl computed the
/// number from `effort_ratio` or the `enabled=true` half-of-max
/// fallback.
#[derive(Copy, Clone)]
enum BudgetSource {
    Explicit,
    Derived,
}

/// Bring `budget` into Anthropic's `[1024, max_tokens-1]` window for
/// the legacy `Enabled` wire shape. The gate at the top of
/// `build_thinking` guarantees `max > 1024`, so `max - 1 >= 1024`
/// and the window is non-empty. On an explicit caller budget that
/// gets clamped UP from below the floor, log at DEBUG so the operator
/// can correlate "I asked for 500 tokens of thinking, why is the
/// model using 1024" with a single grep.
fn clamp_budget_to_legacy_window(budget: u32, max: u32, source: BudgetSource) -> u32 {
    let ceiling = max.saturating_sub(1);
    let clamped = budget.max(ANTHROPIC_MIN_THINKING_BUDGET).min(ceiling);
    if matches!(source, BudgetSource::Explicit) && budget < ANTHROPIC_MIN_THINKING_BUDGET {
        tracing::debug!(
            requested_budget = budget,
            clamped_to = clamped,
            "reasoning.max_tokens below Anthropic legacy minimum (1024); \
             clamping up. The model will use more thinking budget than \
             the caller asked for."
        );
    }
    clamped
}

/// The `budget_tokens` the legacy `enabled` shape composes from `req`'s
/// canonical reasoning, or `None` when that reasoning composes no enabled
/// thinking (thinking off or absent, or a `max_tokens` with no budget window).
#[must_use]
pub fn legacy_thinking_budget(req: &ChatRequest) -> Option<u32> {
    match build_thinking(req)? {
        ThinkingConfig::Enabled { budget_tokens, .. } => Some(budget_tokens),
        ThinkingConfig::Adaptive { .. } | ThinkingConfig::Disabled => None,
    }
}

/// Decide the `thinking` an Anthropic-shape body carries, from the
/// ingress carrier `req.routectl_internal.anthropic_thinking`:
///
/// - `Sent`: the client's object as sent, plus `display` from its own
///   carrier (see `forward_client_thinking`).
/// - `SentNone`: no thinking; an effort alone never adds any.
/// - `NotCaptured`: translated from canonical reasoning by
///   `build_thinking`.
pub fn compose_thinking(req: &ChatRequest) -> Option<WireThinking> {
    match &req.routectl_internal.anthropic_thinking {
        AnthropicThinking::Sent(object) => Some(forward_client_thinking(req, object)),
        AnthropicThinking::SentNone => None,
        AnthropicThinking::NotCaptured => build_thinking(req).map(WireThinking::Translated),
    }
}

/// Why [`shipped_thinking`] removed composed thinking from the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThinkingStrip {
    /// Active thinking beside a `tool_choice` of the named forcing type.
    ForcedToolUse(&'static str),
    /// A context-management cache miss left no thinking blocks to replay.
    ContextCacheMiss,
}

/// The `thinking` an Anthropic-shape body ships: [`compose_thinking`], then
/// the passes that remove it. Thinking is removed when the context-management
/// emulation missed its cache (`context_cache_missed`), or when it is active
/// (`is_active_thinking_type`) and the request's `tool_choice` forces tool
/// use: Anthropic 400s with "Thinking may not be enabled when tool_choice
/// forces tool use." `disabled`, `between_tools`, and an unknown type ride
/// beside a forced choice as sent, and the upstream judges them.
///
/// The forced-tool strip keeps `tool_choice` (the caller's intent to force
/// the named tool) and is a baked seed rather than a learned repair: the
/// rejection names no field path, so the rejection parser cannot attribute
/// it, and no envelope of it is captured to ground a learned row. Its
/// Claude Code trigger is the WebSearch sub-request, which forces
/// `web_search` beside adaptive thinking.
///
/// The second value names the pass that removed thinking, for the caller
/// to declare and log.
pub(super) fn shipped_thinking(
    req: &ChatRequest,
    context_cache_missed: bool,
) -> (Option<WireThinking>, Option<ThinkingStrip>) {
    let Some(composed) = compose_thinking(req) else {
        return (None, None);
    };
    if context_cache_missed {
        return (None, Some(ThinkingStrip::ContextCacheMiss));
    }
    let is_active = composed
        .thinking_type()
        .is_some_and(is_active_thinking_type);
    if is_active && let Some(forced) = forced_tool_choice_type(req.tool_choice.as_ref()) {
        return (None, Some(ThinkingStrip::ForcedToolUse(forced)));
    }
    (Some(composed), None)
}

/// The `thinking.type` an Anthropic-shape egress emits for `req`; `None`
/// when the body carries no `thinking`, or a forwarded object carries no
/// string `type`.
///
/// Reads `shipped_thinking`, the computation body assembly uses, so a
/// forcing `tool_choice` is reflected. One input is out of reach: whether
/// the context-management emulation misses its thinking cache is known only
/// while assembling the body, so this read assumes it does not. That pass
/// only ever removes `thinking`, never changes its type, so the read can
/// over-report a type the body then omits, and cannot report one the body
/// ships differently.
#[must_use]
pub fn emitted_thinking_type(req: &ChatRequest) -> Option<String> {
    shipped_thinking(req, false)
        .0
        .as_ref()
        .and_then(WireThinking::thinking_type)
        .map(str::to_string)
}

/// Whether `thinking_type` spends reasoning budget, and so pins sampling
/// to what Anthropic requires while thinking.
pub fn is_active_thinking_type(thinking_type: &str) -> bool {
    matches!(thinking_type, "enabled" | "adaptive")
}

/// Whether the client itself sent `output_config.effort`. The Anthropic
/// ingress keeps the client's `output_config` in `provider_extras`, so an
/// effort found there is the client's. Any other effort on the wire was
/// composed by routectl beside adaptive thinking (`adaptive_effort`).
#[cfg(feature = "bedrock")]
pub fn client_sent_output_config_effort(req: &ChatRequest) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|extras| extras.get("output_config"))
        .and_then(|config| config.get("effort"))
        .is_some()
}

/// The client's own `thinking` object for the wire. Nothing is clamped
/// into Anthropic's legacy budget window: the upstream judges what the
/// client sent. Only `display` is set, from its own carrier.
fn forward_client_thinking(req: &ChatRequest, object: &ThinkingObject) -> WireThinking {
    let mut wire = object.as_object().clone();
    if let Some(display) = &req.routectl_internal.anthropic_thinking_display {
        wire.insert("display".to_string(), Value::String(display.clone()));
    }
    WireThinking::Forwarded(wire)
}

/// Pair adaptive thinking with a top-level `output_config`. Returns
/// `Some(OutputConfig)` only when the emitted thinking type is `adaptive`
/// and `adaptive_effort` has an effort for it; otherwise `None`.
pub fn build_output_config(
    req: &ChatRequest,
    thinking: Option<&WireThinking>,
) -> Option<OutputConfig> {
    if thinking.and_then(WireThinking::thinking_type) != Some("adaptive") {
        return None;
    }
    adaptive_effort(req).map(|effort| OutputConfig { effort })
}

// ---------------------------------------------------------------------------
// Post-assembly body reconciliation
// ---------------------------------------------------------------------------

/// Merge `provider_extras` into the assembled body. Caller-supplied
/// keys win EXCEPT for routectl-managed top-level keys (see
/// `crate::managed_keys::is_managed_key`); those are dropped so a malicious or
/// careless `provider_extras = {"messages": [...]}` can't replace the
/// assembled messages array.
///
/// Source: `req.provider_extras` as `managed_keys::client_extras_for`
/// selects it for this request's provenance -- the Anthropic ingress's
/// forward-compat sweep, a library caller's own extras, or the operator's
/// `payload_extras` alone for another ingress dialect. Drops here
/// are by design (the swept key would conflict with a key routectl
/// builds itself, e.g. `thinking`) and were flooding
/// `routectl-warn.log` on every claude-code request. The drop log
/// fires at DEBUG with neutral phrasing. If a future caller wires a
/// new source (operator-config `default_extras` on this egress) the
/// log level should branch on the source the way the openai-compat
/// `merge_extras` does.
pub(super) fn merge_provider_extras(id: &str, body: &mut Value, extras: Option<&Value>) {
    let Some(extras) = extras else { return };
    let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) else {
        return;
    };
    for (k, v) in extra_obj {
        if is_managed_key(Dialect::AnthropicApi, k) {
            tracing::debug!(
                provider = id,
                key = %sanitize_for_log(k),
                "forward-compat extra would override routectl-managed key; dropped"
            );
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Late pass over `output_config.effort` on the assembled body, after
/// `merge_provider_extras` has laid the client's `output_config` over the
/// composed one. The client's effort is forwarded as sent beside whatever
/// thinking type ships, "none" included; the upstream judges the pairing.
/// An effort absent beside adaptive thinking is supplied by
/// `adaptive_effort` (a sent `output_config` holding only `format` replaces
/// the composed object wholesale). Nothing is supplied beside any other
/// type.
pub(super) fn reconcile_output_config_effort(req: &ChatRequest, body: &mut Value) {
    let sent = body
        .get("output_config")
        .and_then(|oc| oc.get("effort"))
        .and_then(Value::as_str);
    if sent.is_some() || !body_has_adaptive_thinking(body) {
        return;
    }
    let Some(effort) = adaptive_effort(req) else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let oc = obj
        .entry("output_config")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(oc) = oc.as_object_mut() {
        oc.insert("effort".to_string(), Value::String(effort));
    }
}

/// The assembled body's `thinking.type`.
fn body_thinking_type(body: &Value) -> Option<&str> {
    body.get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
}

/// True iff `body.thinking.type == "adaptive"`.
///
/// INVARIANT: this trusts `thinking` read straight from the assembled
/// body, safe only because `managed_keys::is_managed_key` keeps "thinking"
/// routectl-managed and so blocks `provider_extras` from injecting a
/// forged `thinking: {type: "adaptive"}`. If "thinking" is ever dropped
/// from that set, provider_extras could forge adaptive here and trigger
/// a spurious effort insert -- the two MUST stay in sync.
fn body_has_adaptive_thinking(body: &Value) -> bool {
    body_thinking_type(body) == Some("adaptive")
}

/// True iff the assembled body carries `output_config.effort`, the
/// adaptive-thinking directive Anthropic gates behind `EFFORT_BETA`.
///
/// Reads the ASSEMBLED body: `merge_provider_extras` and
/// `reconcile_output_config_effort` add `output_config.effort`
/// after translation, so `req.reasoning` is not a reliable signal for what
/// ships.
pub(super) fn body_has_output_config_effort(body: &Value) -> bool {
    body.get("output_config")
        .and_then(|oc| oc.get("effort"))
        .is_some()
}

/// Remove `REDACT_THINKING_BETA` from `betas` when `body` carries
/// `thinking.display`. Anthropic treats the two as mutually exclusive and
/// lets the redaction win, so shipping both silently discards the display
/// mode the caller asked for. The body-level directive is the more
/// specific request, so it is the one that ships.
///
/// Must run after every source that can contribute the flag (client
/// betas, operator betas, the pinned floor). Logs one DEBUG line when it
/// removes the flag; a body without `thinking.display` is untouched.
///
/// Baked seed verdict: the host accepts the pair and lets the redaction win,
/// so no rejection ever arrives for field repair to learn the drop from.
/// SEED-VERDICT: acceptance-seed -- the host silently prefers the redaction
/// over the display mode, so the drop is a baked prior no rejection can teach.
pub(super) fn drop_redact_beta_for_display(
    provider_id: &str,
    body: &Value,
    betas: &mut Vec<String>,
) {
    let has_display = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .is_some();
    if !has_display {
        return;
    }
    let flag = routectl_core::identity::anthropic::REDACT_THINKING_BETA;
    let before = betas.len();
    betas.retain(|b| b != flag);
    if betas.len() != before {
        tracing::debug!(
            provider = %provider_id,
            dropped_beta = flag,
            "anthropic-beta flag dropped: body carries thinking.display, which the flag would override"
        );
    }
}

/// Fixed lane label stamped on the sampling-strip event. Names the credential
/// lane (routectl's own OAuth seat egressing to api.anthropic.com), not the
/// cloak state -- the strip is lane-deterministic, so a single static value
/// correlates every strip event without leaking request-specific content.
const OAUTH_OWN_ANTHROPIC_LANE: &str = "oauth-own-anthropic";

/// Strip `temperature` and `top_p` from the FINAL outbound body on the
/// OAuth own-anthropic lane. Anthropic's OAuth seat on api.anthropic.com
/// 400s a `/v1/messages` body carrying either sampling param (both
/// confirmed), so routectl -- which owns the outbound fingerprint on this
/// lane -- must present a body the seat accepts regardless of what the
/// caller, the thinking clamp (a rewritten `temperature: 1.0`), or a `provider_extras`
/// smuggle put there.
///
/// The LAST word on the JSON: called after `cloak_body` and before the
/// outgoing-body trace at both dispatch sites (`complete`, `stream`), so no
/// later pass can re-introduce a stripped key. `stop_sequences` is left
/// untouched (the seat accepts it). No-op on a non-object body.
///
/// Emits at most ONE structured DEBUG event per affected request, listing only the
/// key names actually present in `dropped_params`. NEVER logs the removed
/// values or any body content -- the strip is lane-deterministic, so the
/// key names plus the lane label are the whole diagnostic. The count_tokens
/// path deliberately does NOT call this: `build_count_tokens_body` drops
/// sampling by allowlist already, so calling it there would only multiply
/// events on a path Claude Code polls heavily, breaking the one-event-per-
/// request contract.
///
/// Returns the top-level keys it removed, so the dispatch site can declare
/// them.
///
/// Baked seed verdict: the seat words its refusal as a deprecation of the
/// key, and the rejection parser has no template for that text, so the
/// refusal localizes no field and field repair never sees it.
/// SEED-VERDICT: acceptance-seed -- the rejection parser has no template for
/// the seat's deprecation text, so its refusal names no repairable field.
pub(super) fn normalize_claude_sampling(provider_id: &str, body: &mut Value) -> Vec<&'static str> {
    let Some(obj) = body.as_object_mut() else {
        return Vec::new();
    };
    let mut dropped: Vec<&'static str> = Vec::new();
    if obj.remove("temperature").is_some() {
        dropped.push("temperature");
    }
    if obj.remove("top_p").is_some() {
        dropped.push("top_p");
    }
    if dropped.is_empty() {
        return dropped;
    }
    tracing::debug!(
        provider = provider_id,
        lane = OAUTH_OWN_ANTHROPIC_LANE,
        dropped_params = dropped.join(","),
        "stripped sampling params the OAuth seat rejects on api.anthropic.com"
    );
    dropped
}

#[cfg(test)]
mod tests {
    use super::{
        ThinkingConfig, body_has_output_config_effort, build_thinking, normalize_claude_sampling,
    };
    use crate::beta_withhold::{BetaLane, union_body_implied_betas};
    use routectl_core::identity::anthropic::EFFORT_BETA;
    use routectl_core::{ChatRequest, ReasoningConfig};
    use serde_json::{Value, json};

    // Legacy-budget path with effort "high" must emit the exact table
    // budget (24576), not the old proportional estimate (max * 0.80).
    #[test]
    fn legacy_effort_high_emits_exact_table_budget() {
        // Arrange: non-adaptive request, max_tokens large enough that the
        // 24576 table value survives the [1024, max-1] window clamp and
        // is distinct from any proportional estimate.
        let req = ChatRequest {
            max_tokens: Some(100_000),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Act
        let thinking = build_thinking(&req);

        // Assert
        assert!(
            matches!(
                thinking,
                Some(ThinkingConfig::Enabled {
                    budget_tokens: 24576,
                    display: None
                })
            ),
            "expected exact table budget 24576, got {thinking:?}"
        );
    }

    // -- normalize_claude_sampling helper matrix -----------------------

    /// temperature only, top_p only, and both keys present (in an
    /// arbitrary body) are all stripped; `stop_sequences` and unrelated
    /// keys survive every case.
    #[test]
    fn sampling_strip_removes_temperature_and_top_p_keeps_the_rest() {
        for mut body in [
            json!({"model": "m", "temperature": 0.7, "stop_sequences": ["x"]}),
            json!({"model": "m", "top_p": 0.9, "stop_sequences": ["x"]}),
            json!({
                "top_p": 0.1,
                "model": "m",
                "temperature": 1.0,
                "stop_sequences": ["x"],
            }),
        ] {
            normalize_claude_sampling("test", &mut body);
            assert!(
                body.get("temperature").is_none(),
                "temperature must be stripped; got: {body}"
            );
            assert!(
                body.get("top_p").is_none(),
                "top_p must be stripped; got: {body}"
            );
            assert_eq!(
                body["stop_sequences"],
                json!(["x"]),
                "stop_sequences must be preserved; got: {body}"
            );
            assert_eq!(body["model"], "m", "unrelated keys must survive");
        }
    }

    /// The thinking clamp's rewritten `temperature: 1.0` is stripped like any
    /// other sampling value -- the strip reads the final body, not intent.
    #[test]
    fn sampling_strip_removes_thinking_clamp_temperature() {
        let mut body = json!({"temperature": 1.0});
        normalize_claude_sampling("test", &mut body);
        assert!(body.get("temperature").is_none());
    }

    /// No affected key present -> the body is byte-unchanged (and the
    /// caller emits no WARN, asserted in the mod-level logging tests).
    #[test]
    fn sampling_strip_is_a_noop_without_affected_keys() {
        let mut body = json!({"model": "m", "stop_sequences": ["x"]});
        let before = body.clone();
        normalize_claude_sampling("test", &mut body);
        assert_eq!(body, before, "no sampling key -> no mutation");
    }

    /// A non-object body (array, string, null) is a no-op, never a panic.
    #[test]
    fn sampling_strip_is_a_noop_on_non_object_body() {
        for mut body in [json!([1, 2, 3]), json!("scalar"), Value::Null] {
            let before = body.clone();
            normalize_claude_sampling("test", &mut body);
            assert_eq!(body, before, "non-object body must be untouched");
        }
    }

    // -- effort capability union ---------------------------------------

    /// The body predicate keys strictly on `output_config.effort`; a
    /// sibling `output_config.format` (structured outputs) is NOT effort.
    #[test]
    fn body_has_output_config_effort_keys_on_the_effort_subfield() {
        assert!(body_has_output_config_effort(
            &json!({"output_config": {"effort": "high"}})
        ));
        assert!(!body_has_output_config_effort(
            &json!({"output_config": {"format": {"type": "json_object"}}})
        ));
        assert!(!body_has_output_config_effort(&json!({"model": "m"})));
    }

    /// A body carrying `output_config.effort` gains `EFFORT_BETA` exactly
    /// once, appended after any pre-existing flags (no reorder).
    #[test]
    fn union_effort_beta_appends_flag_once() {
        let body = json!({"output_config": {"effort": "high"}});
        let mut betas = vec!["pre-existing".to_string()];
        union_body_implied_betas(
            body.as_object().expect("object body"),
            BetaLane::AnthropicApiCloak,
            &mut betas,
        );
        assert_eq!(
            betas,
            vec!["pre-existing".to_string(), EFFORT_BETA.to_string()],
            "the flag appends after existing entries without reordering"
        );
    }

    /// Idempotent: a second union pass neither duplicates nor reorders an
    /// already-present flag.
    #[test]
    fn union_effort_beta_is_idempotent() {
        let body = json!({"output_config": {"effort": "high"}});
        let mut betas = vec![EFFORT_BETA.to_string(), "other".to_string()];
        let before = betas.clone();
        union_body_implied_betas(
            body.as_object().expect("object body"),
            BetaLane::AnthropicApiCloak,
            &mut betas,
        );
        assert_eq!(betas, before, "an already-present flag stays put");
    }

    /// One-way invariant: with NO `output_config.effort` in the body, a
    /// caller-supplied effort beta is left untouched -- the union never
    /// manufactures the field, and never strips the flag.
    #[test]
    fn union_effort_beta_leaves_caller_beta_without_field_untouched() {
        let body = json!({"model": "m"});
        let mut betas = vec![EFFORT_BETA.to_string()];
        let before = betas.clone();
        union_body_implied_betas(
            body.as_object().expect("object body"),
            BetaLane::AnthropicApiCloak,
            &mut betas,
        );
        assert_eq!(
            betas, before,
            "no effort field -> the beta list is untouched (one-way invariant)"
        );
    }
}

#[cfg(test)]
#[path = "extras_thinking_display_tests.rs"]
mod thinking_display_tests;

// The spend coupling: which legacy-path thinking budgets move when the
// router injects a higher `max_output_tokens` ceiling, and which do not.
#[cfg(test)]
#[path = "extras_ceiling_budget_coupling_tests.rs"]
mod ceiling_budget_coupling_tests;
