//! Thinking-budget composition and post-assembly body reconciliation.
//!
//! Two concerns live here. (1) The canonical `req.reasoning` ->
//! Anthropic `thinking` mapping: `build_thinking` selects the wire
//! shape (legacy `Enabled { budget_tokens }` vs Opus 4.7+ `Adaptive`),
//! clamps the budget into Anthropic's `[1024, max_tokens-1]` window,
//! applies the operator per-model cap, and pairs `Adaptive` with a
//! top-level `output_config` via `build_output_config`. `build_thinking`
//! is `pub(crate)` so the Bedrock Converse egress reuses it. (2) The
//! post-merge body reconciliation: `filter_anthropic_betas` applies the
//! operator allowlist, `merge_provider_extras` layers forward-compat
//! extras in while shielding routectl-managed keys
//! (`is_routectl_managed_key`), `reconcile_output_config_effort`
//! re-clamps or strips `output_config.effort` per model capability, and
//! `strip_thinking_when_tool_choice_forces_use` drops `thinking` when
//! the tool_choice forces tool use (Anthropic forbids the combo).

use std::borrow::Cow;

use serde_json::Value;

use routectl_core::{ChatRequest, is_canonical_request_key};

use crate::effort::{budget_from_level, clamp_effort_to_supported};

use super::tools::{PARALLEL_TOOL_CALLS_KEY, TOOL_CHOICE_TYPE_ANY, TOOL_CHOICE_TYPE_TOOL};
use super::types::{OutputConfig, ThinkingConfig};

/// Hardcoded baseline `max_tokens` value injected on outbound
/// Anthropic-shape requests when the caller omits the field AND the
/// per-model `[models.X].max_output_tokens` override is unset. The
/// Anthropic Messages API requires `max_tokens` and 400s on omission;
/// 64000 is above the 64K ceiling of Sonnet 4.5/4.6 and Opus 4.5 and
/// within Opus 4.7's 128K window.
///
/// Operators with known-low-cap models (Anthropic Opus 4 / 4.1 at
/// 32000, Sonnet 3.5 / DeepSeek V3 at 8000) should set
/// `[models.X].max_output_tokens` to avoid an upstream 400 on the
/// per-model ceiling check. See docs/CONFIGURATION.md.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 64_000;

/// Resolve the outbound `max_tokens` value: caller's request wins;
/// absent that, the router-supplied per-model override from
/// `req.routectl_internal.max_output_tokens` (when non-zero) wins;
/// absent that, the hardcoded `DEFAULT_MAX_OUTPUT_TOKENS` baseline
/// (64000).
///
/// Consumed by Anthropic-shape egresses (anthropic-api +
/// bedrock-invoke; bedrock-invoke delegates body construction to this
/// module's `normalize`). Other egresses (openai-compat,
/// openai-responses, bedrock-converse) forward `req.max_tokens` cleanly
/// when None per the good-translator principle.
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
        // see it on a non-adaptive provider. 0.99 leaves 1% of
        // max_tokens for the visible response so the request is
        // accepted; in practice operators who want `max` should set
        // `supports_adaptive_thinking = true` on the model config.
        "max" => 0.99,
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

/// Effort string to use for top-level `output_config.effort`. Returns
/// `req.reasoning.effort` clamped against `req.routectl_internal.effort_levels`
/// when that slice is non-empty (operator cost cap); falls back to the
/// raw effort string when effort_levels is empty (Anthropic pass-through
/// default). Falls back to "medium" when no effort is set (Anthropic
/// requires the field when adaptive thinking is active and validates
/// the string).
fn derive_effort(req: &ChatRequest) -> String {
    let raw = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .unwrap_or_else(|| "medium".to_string());
    clamp_effort_to_supported(&raw, &req.routectl_internal.effort_levels).into_owned()
}

/// Decide which `ThinkingConfig` variant (if any) to emit. The
/// `adaptive` flag selects the wire shape: when `true` AND thinking
/// would otherwise be `Enabled`, returns `Adaptive` instead (the
/// caller pairs that with a top-level `output_config`); when `false`,
/// returns the legacy `Enabled { budget_tokens }` shape; `Disabled`
/// is always returned verbatim regardless of the flag.
///
/// The `adaptive` flag comes from
/// `req.routectl_internal.supports_adaptive_thinking`, set by the
/// router from the operator-declared `[models.X]
/// supports_adaptive_thinking` capability.
///
/// On the legacy `Enabled` path, `req.routectl_internal.max_thinking_budget`
/// is applied as an operator-declared ceiling BEFORE Anthropic's own
/// `[1024, max_tokens-1]` window clamp. Zero means no operator cap.
///
/// Note on `max_tokens` + adaptive: Anthropic's adaptive thinking wire
/// shape has no field for an explicit budget -- the model picks its
/// own from the effort string. If a caller sets both
/// `reasoning.max_tokens` AND the model is adaptive, the budget is
/// dropped (with a tracing::warn at the call site). The caller's
/// effort string still travels to `output_config.effort`.
pub fn build_thinking(req: &ChatRequest, adaptive: bool) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    if r.enabled == Some(false) {
        return Some(ThinkingConfig::Disabled);
    }
    if r.effort.as_deref() == Some("none") {
        return Some(ThinkingConfig::Disabled);
    }

    // Did the caller actually ask for thinking? Any of: explicit
    // enabled=true, a budget, an effort string (other than "none").
    let thinking_active = r.enabled == Some(true)
        || r.max_tokens.is_some()
        || r.effort.as_deref().is_some_and(|e| e != "none");
    if !thinking_active {
        return None;
    }

    if adaptive {
        // Opus 4.7+ wire shape. budget_tokens is gone; effort moves
        // to top-level output_config (handled by build_output_config).
        // If the caller set both an explicit budget AND the model is
        // adaptive, the budget gets dropped because there's no wire
        // field for it. Warn so an operator who set both fields
        // routinely (e.g. a client library that always sends
        // `reasoning.max_tokens`) can see the discard in logs and
        // adjust to using `effort` instead.
        if r.max_tokens.is_some() {
            tracing::warn!(
                budget_tokens = r.max_tokens,
                "reasoning.max_tokens dropped on adaptive thinking path; \
                 Anthropic's adaptive shape has no budget field -- \
                 the model picks its own budget from output_config.effort. \
                 Set reasoning.effort to steer instead."
            );
        }
        return Some(ThinkingConfig::Adaptive);
    }

    // Legacy wire shape constraint: Anthropic requires
    // `budget_tokens >= 1024` AND `max_tokens > budget_tokens`. Probe-
    // sized requests (claude-code title gen, topic summaries) routinely
    // send `max_tokens=64`; emitting any legacy `Enabled` body for
    // those would 400 upstream. Drop thinking for this one request
    // rather than reshape the caller's `max_tokens`.
    if !legacy_thinking_fits(req) {
        tracing::warn!(
            request_max_tokens = req.max_tokens,
            min_required = ANTHROPIC_MIN_THINKING_BUDGET + 1,
            reasoning_effort = ?r.effort,
            reasoning_max_tokens = ?r.max_tokens,
            "anthropic legacy thinking shape requires max_tokens > 1024; \
             dropping thinking for this probe-sized request. Set \
             supports_adaptive_thinking=true on the model config for Opus 4.7+ \
             to avoid the budget-vs-max_tokens coupling, or send max_tokens > 1024."
        );
        return None;
    }

    // Legacy wire shape: Enabled { budget_tokens }. Translate the
    // canonical signal (explicit budget > effort > enabled=true).
    // Every arm runs the budget through `clamp_budget_to_legacy_window`,
    // which enforces BOTH Anthropic invariants:
    //   - `budget_tokens >= 1024` (floor); a sub-1024 explicit budget
    //     gets raised with a WARN so an operator can see the silent
    //     promotion. The effort/enabled arms can only land below the
    //     floor in the 1025-1279 (effort=high) band; same clamp.
    //   - `budget_tokens < max_tokens` (ceiling); an explicit budget
    //     that exceeds `req.max_tokens` would otherwise produce a
    //     wire body Anthropic 400s. The clamp caps at
    //     `max.saturating_sub(1)` to leave at least one visible-
    //     output token. The gate above guarantees `max > 1024`, so
    //     the ceiling (`max - 1`) is always at least 1024 and the
    //     floor never collides with the ceiling.
    let max = resolve_max_tokens(req);
    // Operator-declared per-model ceiling. Non-zero values cap the
    // budget DOWN before Anthropic's own `[1024, max_tokens-1]` window
    // clamp runs. Zero means no operator cap -- Anthropic's clamp is
    // the only guard.
    let operator_cap = req.routectl_internal.max_thinking_budget;

    if let Some(budget) = r.max_tokens {
        let budget = apply_operator_cap(budget, operator_cap);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Explicit),
        });
    }
    if let Some(effort) = r.effort.as_deref() {
        let clamped = clamp_effort_to_supported(effort, &req.routectl_internal.effort_levels);
        // Prefer the exact effort->budget table; fall back to the
        // proportional estimate only for a level outside the table so
        // an unexpected string never regresses to a zero budget.
        let budget = budget_from_level(clamped.as_ref())
            .unwrap_or_else(|| ((max as f64) * effort_ratio(clamped.as_ref())).max(1.0) as u32);
        let budget = apply_operator_cap(budget, operator_cap);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
        });
    }
    // r.enabled == Some(true) without budget or effort.
    let budget = apply_operator_cap(max / 2, operator_cap);
    Some(ThinkingConfig::Enabled {
        budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
    })
}

/// Origin of a `budget_tokens` value about to be clamped to
/// Anthropic's legal `[1024, max_tokens-1]` window. Used to gate
/// whether a silent floor promotion should WARN: `Explicit` means
/// the caller asked for a specific number and we are about to ignore
/// it -- worth a log line; `Derived` means routectl computed the
/// number from `effort_ratio` or the `enabled=true` half-of-max
/// fallback, and the operator's per-model config implicitly opted in.
#[derive(Copy, Clone)]
enum BudgetSource {
    Explicit,
    Derived,
}

/// Apply the operator-declared per-model thinking-budget cap. When
/// `operator_cap` is non-zero, the budget is clamped DOWN to that
/// ceiling before Anthropic's own `[1024, max_tokens-1]` window clamp
/// runs. Zero is the sentinel for "no operator cap" and returns the
/// budget unchanged.
fn apply_operator_cap(budget: u32, operator_cap: u32) -> u32 {
    if operator_cap > 0 {
        budget.min(operator_cap)
    } else {
        budget
    }
}

/// Bring `budget` into Anthropic's `[1024, max_tokens-1]` window for
/// the legacy `Enabled` wire shape. The gate at the top of
/// `build_thinking` guarantees `max > 1024`, so `max - 1 >= 1024`
/// and the window is non-empty. On an explicit caller budget that
/// gets clamped UP from below the floor, fire a WARN so the operator
/// can correlate "I asked for 500 tokens of thinking, why is the
/// model using 1024" with a single grep.
fn clamp_budget_to_legacy_window(budget: u32, max: u32, source: BudgetSource) -> u32 {
    let ceiling = max.saturating_sub(1);
    let clamped = budget.max(ANTHROPIC_MIN_THINKING_BUDGET).min(ceiling);
    if matches!(source, BudgetSource::Explicit) && budget < ANTHROPIC_MIN_THINKING_BUDGET {
        tracing::warn!(
            requested_budget = budget,
            clamped_to = clamped,
            "reasoning.max_tokens below Anthropic legacy minimum (1024); \
             clamping up. The model will use more thinking budget than \
             the caller asked for."
        );
    }
    clamped
}

/// Pair `ThinkingConfig::Adaptive` with a top-level `output_config`.
/// Returns `Some(OutputConfig)` only when the thinking variant is
/// `Adaptive`; otherwise `None`.
pub(super) fn build_output_config(
    req: &ChatRequest,
    thinking: &Option<ThinkingConfig>,
) -> Option<OutputConfig> {
    if matches!(thinking, Some(ThinkingConfig::Adaptive)) {
        Some(OutputConfig {
            effort: derive_effort(req),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Beta allowlist + post-assembly body reconciliation
// ---------------------------------------------------------------------------

/// Filter `req.anthropic_beta` against the operator-supplied
/// `allowed_betas` list. Empty allowlist = pass-through (default).
/// Otherwise, drop entries not in the list at DEBUG so operators
/// triaging unexpected behavior can see WHICH flags got removed.
/// Mirrors the Bedrock-egress `filter_bedrock_betas` shape.
pub fn filter_anthropic_betas<'a>(
    provider_id: &str,
    requested: &'a [String],
    allowed: &[String],
) -> Cow<'a, [String]> {
    if allowed.is_empty() {
        return Cow::Borrowed(requested);
    }
    let mut kept = Vec::with_capacity(requested.len());
    for flag in requested {
        if allowed.iter().any(|a| a == flag) {
            kept.push(flag.clone());
        } else {
            tracing::debug!(
                provider = provider_id,
                flag = %routectl_core::sanitize_for_log(flag),
                "dropping beta flag not in operator-supplied [providers.X] allowed_betas"
            );
        }
    }
    Cow::Owned(kept)
}

/// Merge `provider_extras` into the assembled body. Caller-supplied
/// keys win EXCEPT for routectl-managed top-level keys (see
/// `is_routectl_managed_key`); those are dropped so a malicious or
/// careless `provider_extras = {"messages": [...]}` can't replace the
/// assembled messages array. This was an architecture-review
/// finding (MEDIUM-1).
///
/// Source: this helper only ever sees `req.provider_extras` -- the
/// Anthropic ingress's forward-compat sweep destination. Drops here
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
        if is_routectl_managed_key(k) {
            tracing::debug!(
                provider = id,
                key = %k,
                "forward-compat extra would override routectl-managed key; dropped"
            );
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Top-level Anthropic body keys that routectl owns. Delegates to the
/// shared canonical list and adds Anthropic-API-specific keys that are
/// not on `ChatRequest` but are written by this egress from canonical
/// fields:
/// - `thinking`  -- translated from `req.reasoning` by `build_thinking`;
///   not a raw ChatRequest field but routectl writes it.
///
/// `output_config` is intentionally NOT here -- the full object is
/// allowed through `provider_extras` for legitimate sub-fields like
/// `output_config.format` (structured-output). The `output_config.effort`
/// sub-field is reconciled post-merge in `reconcile_output_config_effort`
/// when the model does not have `supports_adaptive_thinking=true` (Haiku,
/// Sonnet -- Anthropic 400s on `effort` for them).
///
/// `parallel_tool_calls` is Anthropic-egress-local: the OpenAI-dialect
/// toggle is consumed into `disable_parallel_tool_use` on `tool_choice`
/// (see `tools::apply_parallel_tool_use`) and must not also reach the
/// wire (invalid top-level Anthropic field). It is NOT on shared
/// `reserved.rs` so the openai-compat egress keeps forwarding it verbatim.
pub(super) fn is_routectl_managed_key(key: &str) -> bool {
    is_canonical_request_key(key)
        || matches!(
            key,
            // Anthropic-API-specific managed keys not on ChatRequest:
            // `thinking` is built from req.reasoning by this egress.
            "thinking" | PARALLEL_TOOL_CALLS_KEY
        )
}

/// Late enforcer of the output_config.effort invariant:
/// `output_config.effort` is present IFF the assembled body carries
/// `thinking` with `type == "adaptive"`. Reads ground truth from the
/// BODY, not from a (possibly stale) `adaptive` flag -- earlier passes
/// (cache-miss soft-fail, tool_choice strip) may have removed `thinking`
/// after `build_output_config` ran, so the only reliable signal is the
/// final assembled body. This must run LAST among the body mutations
/// touching thinking/output_config.
///
/// No adaptive thinking in the body: drop any orphan
/// `output_config.effort` (preserving a sibling `output_config.format`,
/// and removing a now-empty `output_config`). Delegates to
/// `remove_output_config_effort`.
///
/// Adaptive thinking present: guarantee `output_config.effort` exists.
/// Re-inject from `derive_effort(req)` when absent (e.g. provider_extras
/// supplied an `output_config` with only `format`); when present, re-
/// clamp against the operator's `effort_levels` exactly as before
/// (`merge_provider_extras` may have overwritten the pre-merge clamped
/// value with a raw caller-supplied `effort`). Empty `effort_levels`
/// skips the re-clamp but still guarantees presence (pass-through).
pub(super) fn reconcile_output_config_effort(req: &ChatRequest, body: &mut Value) {
    if !body_has_adaptive_thinking(body) {
        remove_output_config_effort(body);
        return;
    }
    let effort_levels = &req.routectl_internal.effort_levels;
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let oc = obj
        .entry("output_config")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(oc) = oc.as_object_mut() else {
        return;
    };
    match oc
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    {
        None => {
            oc.insert("effort".to_string(), Value::String(derive_effort(req)));
        }
        Some(current) => {
            if !effort_levels.is_empty() {
                let clamped = clamp_effort_to_supported(&current, effort_levels);
                if clamped.as_ref() != current {
                    oc.insert("effort".to_string(), Value::String(clamped.into_owned()));
                }
            }
        }
    }
}

/// True iff the assembled body carries a `thinking` key at all (legacy
/// `enabled` OR `adaptive`). Shared final-body predicate: the sampling-params
/// reconciler restores caller sampling when this is false, and
/// `body_has_adaptive_thinking` is layered on it, so the two late passes
/// cannot disagree about whether thinking survived the strip passes.
fn body_has_thinking(body: &Value) -> bool {
    body.get("thinking").is_some()
}

/// True iff `body.thinking.type == "adaptive"`.
///
/// INVARIANT: this trusts `thinking` read straight from the assembled
/// body, safe only because `is_routectl_managed_key` keeps "thinking"
/// routectl-managed and so blocks `provider_extras` from injecting a
/// forged `thinking: {type: "adaptive"}`. If "thinking" is ever dropped
/// from that set, provider_extras could forge adaptive here and trigger
/// spurious effort re-injection -- the two MUST stay in sync.
fn body_has_adaptive_thinking(body: &Value) -> bool {
    body_has_thinking(body)
        && body
            .get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            == Some("adaptive")
}

/// Late enforcer of the temperature/top_p invariant, the sampling analogue
/// of `reconcile_output_config_effort`. Assembly forces `temperature = 1.0`
/// and drops `top_p` whenever thinking is composed (Anthropic forbids
/// alternative-continuation sampling while spending reasoning budget). A
/// later strip pass (cache-miss soft-fail, tool_choice-forces-use) can then
/// remove `thinking` from the body, leaving the forced sampling behind and
/// discarding the caller's original values.
///
/// This recomputes from the FINAL body: when no `thinking` survives, re-apply
/// the caller's sampling from the SOURCE request -- `temperature =
/// req.temperature`, and `top_p` only when temperature is absent (mirrors the
/// assembly else-branch; Claude 4.x rejects `temperature`+`top_p` together).
/// Computing from the final body rather than restoring saved pre-strip values
/// means a future strip pass cannot invalidate it. Must run after ALL strip
/// passes.
pub(super) fn reconcile_sampling_params(provider_id: &str, req: &ChatRequest, body: &mut Value) {
    if body_has_thinking(body) {
        return;
    }
    let temperature = req.temperature;
    let top_p = if temperature.is_some() {
        None
    } else {
        req.top_p
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let temp_changed = set_optional_f64(obj, "temperature", temperature);
    let top_p_changed = set_optional_f64(obj, "top_p", top_p);
    if temp_changed || top_p_changed {
        tracing::debug!(
            provider = provider_id,
            "recomputed temperature/top_p after a strip pass removed thinking; \
             restored caller sampling params"
        );
    }
}

/// Set `key` to `value` on `obj`, or remove it when `value` is None
/// (mirroring the wire's `skip_serializing_if = "Option::is_none"`).
/// Returns true iff the emitted field actually changed, so the caller only
/// logs a real correction.
fn set_optional_f64(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<f64>,
) -> bool {
    match value {
        Some(v) => {
            let next = Value::from(v);
            if obj.get(key) == Some(&next) {
                false
            } else {
                obj.insert(key.to_string(), next);
                true
            }
        }
        None => obj.remove(key).is_some(),
    }
}

/// Remove `output_config.effort` from `body`, preserving any orthogonal
/// sibling (e.g. structured-output `format`). When `effort` was the only
/// sub-key, the now-empty `output_config` object is removed entirely so
/// the wire body stays clean. A no-op when neither key is present.
fn remove_output_config_effort(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        crate::effort::drop_orphaned_output_config_effort(obj);
    }
}

/// Anthropic's extended-thinking docs forbid `thinking` paired with
/// `tool_choice` values that force tool use. The Messages API 400s with
/// "Thinking may not be enabled when tool_choice forces tool use." when
/// the body carries `thinking` AND `tool_choice.type` is `"any"` or
/// `"tool"`. The constraint is identical for adaptive thinking
/// (`{"type":"adaptive"}`) per the adaptive-thinking docs page.
///
/// Real-world trigger: Claude Code's WebSearch tool sub-request fires
/// `tool_choice: {type:"tool", name:"web_search"}` in tandem with
/// `thinking: {type:"adaptive"}` (when the operator config sets
/// `effort: "max"`). routectl emits both because each was set
/// independently by separate concerns -- the tool_choice translator and
/// the thinking composer don't talk to each other.
///
/// Strip `thinking` (not `tool_choice`) so the caller's intent to force
/// the named tool is preserved; the request still completes
/// successfully, just without thinking. `auto`, `none`, and absent
/// `tool_choice` do not trigger the strip.
///
/// This function touches only `thinking`. A now-orphan
/// `output_config.effort` (valid only alongside adaptive thinking) is
/// dropped by `reconcile_output_config_effort`, the late enforcer that
/// runs after this and reads the final body shape.
///
/// Runs after `merge_provider_extras` so the check operates on the
/// final wire body, regardless of whether `thinking` was composed by
/// `build_thinking` or layered in by some future provider-extras path
/// that bypasses `is_routectl_managed_key`.
pub(super) fn strip_thinking_when_tool_choice_forces_use(provider_id: &str, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if !obj.contains_key("thinking") {
        return;
    }
    // The `.filter()` short-circuits on non-forcing values (`auto` /
    // `none` / unknown), so the owned `String` is allocated only on the
    // strip path. The Option also lets the immutable borrow on `obj` end
    // at the semicolon -- `obj.remove` below takes the mutable borrow
    // without conflict.
    let ttype = obj
        .get("tool_choice")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str())
        .filter(|&t| t == TOOL_CHOICE_TYPE_ANY || t == TOOL_CHOICE_TYPE_TOOL)
        .map(str::to_string);
    let Some(ttype) = ttype else {
        return;
    };
    obj.remove("thinking");
    tracing::debug!(
        provider = provider_id,
        tool_choice_type = %ttype,
        "stripped thinking from outgoing body: tool_choice forces tool use; \
         Anthropic forbids the combo"
    );
}

#[cfg(test)]
mod tests {
    use super::{ThinkingConfig, build_thinking};
    use routectl_core::{ChatRequest, ReasoningConfig};

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
        let thinking = build_thinking(&req, false);

        // Assert
        assert!(
            matches!(
                thinking,
                Some(ThinkingConfig::Enabled {
                    budget_tokens: 24576
                })
            ),
            "expected exact table budget 24576, got {thinking:?}"
        );
    }
}
