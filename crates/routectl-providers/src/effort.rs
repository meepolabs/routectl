//! Shared effort-clamping helper for OpenAI-shape egresses.
//!
//! OpenAI-compatible upstreams (openai, codex, deepseek, vllm) each
//! declare a finite set of valid `reasoning_effort` strings. The
//! canonical `ChatRequest.reasoning.effort` field may carry values
//! beyond that set (e.g. "xhigh" or "max" from claude-code). Without
//! clamping, the upstream rejects with 400.
//!
//! This module provides a single helper, `clamp_effort_to_supported`,
//! that maps an arbitrary effort string to the nearest supported level.
//! It returns `Option`: `None` means the caller asked for reasoning-OFF
//! (`effort: "none"`) and the egress must OMIT the effort field rather
//! than substitute a positive level. Anthropic-shape egresses
//! (anthropic_api, bedrock) accept "xhigh" and "max" verbatim and MUST
//! NOT call this helper for that purpose.

#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
use std::borrow::Cow;

/// The six canonical effort tokens, in rank order from lowest to
/// highest. This is the single source of truth for the valid effort
/// vocabulary; `validate_reasoning_defaults` in `routectl-router`
/// imports this constant instead of maintaining a separate copy.
pub const VALID_EFFORT_TOKENS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Standard effort rank order. Identical to VALID_EFFORT_TOKENS --
/// maintained as a separate slice reference so `clamp_effort_to_supported`
/// can continue to work with a `&[&str]` reference and carry its
/// rank-ordering semantics explicitly. Both must stay in sync; the
/// unit test `effort_tokens_and_rank_order_in_sync` enforces that.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
const RANK_ORDER: &[&str] = &VALID_EFFORT_TOKENS;

/// Resolve `requested` against the model's declared supported levels.
///
/// Returns `None` when the caller's intent is reasoning-OFF (`requested ==
/// "none"`): every egress interprets `None` as "omit the effort field" (or
/// emit its own disable form), so a client that explicitly declined thinking
/// is never billed for it. Returns `Some(level)` otherwise.
///
/// Rank order for clamping:
///
///   minimal < low < medium < high < xhigh < max
///
/// Rules:
///   - `requested == "none"` -> `None` (reasoning-OFF; omit, never substitute
///     a positive level). This is the single place the "none" -> omit
///     decision lives so all egresses stay consistent.
///   - If `supported` is empty, return `Some(requested)` unchanged (passthrough).
///   - If `supported` contains `requested`, return `Some(requested)` unchanged.
///   - Otherwise, pick the highest supported level that is <= requested
///     on the rank order above.
///   - If no supported level is <= requested (e.g., requested="minimal"
///     but supported only contains ["high"]), pick the lowest supported
///     level.
///   - A genuinely unknown, intent-bearing token (not "none", not on the
///     rank order -- e.g. "turbo") is PASSED THROUGH verbatim with a
///     `tracing::warn!`, never clamped down to the lowest supported level.
///     Clamping an unknown DOWN would silently invert caller intent; the
///     upstream may reject it, but routectl does not fabricate a level.
///   - Emits `tracing::debug!` whenever a clamp actually changes the value,
///     with fields `requested`, `applied`, and `supported`.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
pub(crate) fn clamp_effort_to_supported<'a>(
    requested: &'a str,
    supported: &[String],
) -> Option<Cow<'a, str>> {
    // Reasoning-OFF: never substitute a positive level. Signal "omit" so the
    // egress drops the effort field (or emits its own disable form). "none" is
    // deliberately absent from RANK_ORDER -- it is not a positive level.
    if requested == "none" {
        return None;
    }

    // Empty supported list -> passthrough semantics.
    if supported.is_empty() {
        return Some(Cow::Borrowed(requested));
    }

    // Fast path: the requested level is already in the supported set.
    if supported.iter().any(|s| s == requested) {
        return Some(Cow::Borrowed(requested));
    }

    // Locate the requested level in the canonical rank order.
    let requested_rank = RANK_ORDER.iter().position(|&r| r == requested);

    let Some(req_rank) = requested_rank else {
        // Unknown, intent-bearing effort string. Pass it through verbatim
        // rather than clamping to the lowest supported level: clamping an
        // unknown DOWN silently inverts caller intent. The upstream may
        // reject it, but routectl never fabricates a level.
        tracing::warn!(
            requested = requested,
            supported = ?supported,
            "effort string is not in the standard rank order; passing through verbatim"
        );
        return Some(Cow::Borrowed(requested));
    };

    // Find the highest supported level whose rank is <= requested rank.
    // Iterate RANK_ORDER in reverse (high to low) and pick the first
    // supported entry that fits.
    let clamped = RANK_ORDER[..=req_rank]
        .iter()
        .rev()
        .find(|&&r| supported.iter().any(|s| s == r))
        .copied();

    let applied = match clamped {
        Some(c) => c,
        // Nothing <= requested is supported: pick the lowest supported level.
        None => lowest_supported(supported),
    };

    tracing::debug!(
        requested = requested,
        applied = %applied,
        supported = ?supported,
        "effort clamped to model's declared supported levels"
    );
    Some(Cow::Owned(applied.to_owned()))
}

/// Exact effort-level -> `budget_tokens` lookup. Returns `None` for any
/// level outside the table so callers can fall back to their own
/// estimate rather than guess. This is the forward direction of the
/// effort<->budget bijection; `level_from_budget` is the reverse.
///
/// The table is independent of `VALID_EFFORT_TOKENS` / `RANK_ORDER`:
/// it carries a "none" level (budget 0) and exact per-level budgets
/// that the clamp path deliberately does not model.
#[cfg(any(feature = "gemini", feature = "anthropic-api"))]
pub(crate) fn budget_from_level(level: &str) -> Option<u32> {
    match level {
        "none" => Some(0),
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" => Some(32768),
        "max" => Some(128_000),
        _ => None,
    }
}

/// Reverse of `budget_from_level`: map a `budget_tokens` value back to
/// the effort level whose threshold band contains it. Bands:
///
///   0           -> none
///   1..=512     -> minimal
///   513..=1024  -> low
///   1025..=8192 -> medium
///   8193..=24576 -> high
///   24577..=32768 -> xhigh
///   32769..     -> max
#[cfg(any(feature = "gemini", feature = "openai-responses"))]
pub(crate) const fn level_from_budget(budget: u32) -> &'static str {
    match budget {
        0 => "none",
        1..=512 => "minimal",
        513..=1024 => "low",
        1025..=8192 => "medium",
        8193..=24576 => "high",
        24577..=32768 => "xhigh",
        _ => "max",
    }
}

/// Return the lowest-ranked string in `supported` by the standard rank
/// order. Falls back to the first element for strings not in the order.
///
/// Precondition: `supported` is non-empty. Callers must ensure this
/// before invoking (every call site checks `supported.is_empty()`
/// earlier and returns early).
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
fn lowest_supported(supported: &[String]) -> &str {
    debug_assert!(
        !supported.is_empty(),
        "lowest_supported called with empty slice -- precondition violated"
    );
    supported
        .iter()
        .min_by_key(|s| {
            RANK_ORDER
                .iter()
                .position(|&r| r == s.as_str())
                .unwrap_or(usize::MAX)
        })
        .map(std::string::String::as_str)
        .expect("supported is non-empty -- precondition violated")
}

/// Pure JSON utility: remove `output_config.effort` from `obj`, and
/// drop the now-empty `output_config` object only if removing `effort`
/// actually removed something. Any orthogonal sibling (e.g.
/// structured-output `format`) is preserved, and a caller-supplied empty
/// `output_config {}` that carried no `effort` is left untouched.
///
/// This encodes NO business rule about WHEN orphaned effort should be
/// dropped -- each call site decides that and calls this only when the
/// strip is warranted.
#[cfg(any(feature = "anthropic-api", feature = "bedrock"))]
pub(crate) fn drop_orphaned_output_config_effort(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if oc.remove("effort").is_some() && oc.is_empty() {
        obj.remove("output_config");
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    use super::VALID_EFFORT_TOKENS;
    #[cfg(any(feature = "gemini", feature = "anthropic-api"))]
    use super::budget_from_level;
    #[cfg(feature = "openai-responses")]
    use super::level_from_budget;
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    use super::{RANK_ORDER, clamp_effort_to_supported};

    // VALID_EFFORT_TOKENS and RANK_ORDER must stay in sync: same elements,
    // same order. If either is updated, the other must follow.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn effort_tokens_and_rank_order_in_sync() {
        assert_eq!(
            VALID_EFFORT_TOKENS.as_slice(),
            RANK_ORDER,
            "VALID_EFFORT_TOKENS and RANK_ORDER must be identical slices"
        );
    }

    // Helper to build a Vec<String> from a slice of &str.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    fn levels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| (*s).to_string()).collect()
    }

    // Empty supported -> requested returned verbatim (passthrough).
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn empty_supported_returns_requested() {
        assert_eq!(
            clamp_effort_to_supported("max", &[]).as_deref(),
            Some("max")
        );
        assert_eq!(
            clamp_effort_to_supported("xhigh", &[]).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            clamp_effort_to_supported("anything", &[]).as_deref(),
            Some("anything")
        );
    }

    // "none" is reasoning-OFF: the helper returns None so every egress omits
    // the effort field. It must NEVER resolve to a positive level, with or
    // without a supported cap.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn none_maps_to_omit_regardless_of_supported() {
        assert_eq!(clamp_effort_to_supported("none", &[]), None);
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(clamp_effort_to_supported("none", &sup), None);
        // Even when the only supported level is the lowest, "none" still omits
        // rather than clamping up to it (the original inversion bug).
        let only_low = levels(&["low"]);
        assert_eq!(clamp_effort_to_supported("none", &only_low), None);
    }

    // Requested level is in supported -> verbatim, no allocation.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn requested_in_supported_returns_verbatim() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(
            clamp_effort_to_supported("high", &sup).as_deref(),
            Some("high")
        );
        assert_eq!(
            clamp_effort_to_supported("low", &sup).as_deref(),
            Some("low")
        );
    }

    // requested="max", supported=["low","medium","high"] -> "high".
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn max_clamps_to_high() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(
            clamp_effort_to_supported("max", &sup).as_deref(),
            Some("high")
        );
    }

    // requested="xhigh", supported=["low","medium","high"] -> "high".
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn xhigh_clamps_to_high() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(
            clamp_effort_to_supported("xhigh", &sup).as_deref(),
            Some("high")
        );
    }

    // requested="minimal", supported=["medium","high"] -> "medium"
    // (nothing <= minimal in supported, so pick lowest = "medium").
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn minimal_below_all_supported_picks_lowest() {
        let sup = levels(&["medium", "high"]);
        assert_eq!(
            clamp_effort_to_supported("minimal", &sup).as_deref(),
            Some("medium")
        );
    }

    // requested="low", supported=["medium","high"] -> "medium"
    // (nothing <= low in supported, so pick lowest = "medium").
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn low_below_all_supported_picks_lowest() {
        let sup = levels(&["medium", "high"]);
        assert_eq!(
            clamp_effort_to_supported("low", &sup).as_deref(),
            Some("medium")
        );
    }

    // Unknown, intent-bearing effort string with non-empty supported ->
    // passthrough verbatim (NOT clamped down to lowest, which would invert
    // intent). The upstream may reject it, but routectl never fabricates a
    // level.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn unknown_effort_passes_through_verbatim() {
        let sup = levels(&["low", "medium"]);
        assert_eq!(
            clamp_effort_to_supported("turbo", &sup).as_deref(),
            Some("turbo"),
            "an unknown token must pass through, not clamp down to lowest"
        );
    }

    // Clamp from a mid-level works correctly.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn high_clamps_to_medium_when_only_low_medium_supported() {
        let sup = levels(&["low", "medium"]);
        assert_eq!(
            clamp_effort_to_supported("high", &sup).as_deref(),
            Some("medium")
        );
    }

    // Single-element supported list: everything (except "none") maps to it.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn single_element_supported_always_returns_it() {
        let sup = levels(&["medium"]);
        assert_eq!(
            clamp_effort_to_supported("max", &sup).as_deref(),
            Some("medium")
        );
        assert_eq!(
            clamp_effort_to_supported("minimal", &sup).as_deref(),
            Some("medium")
        );
        assert_eq!(
            clamp_effort_to_supported("medium", &sup).as_deref(),
            Some("medium")
        );
    }

    // minimal < low: low clamps down to minimal when minimal is sole option.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn low_clamps_down_when_only_minimal_supported() {
        let sup = levels(&["minimal"]);
        // low > minimal; highest <= low in supported is minimal.
        assert_eq!(
            clamp_effort_to_supported("low", &sup).as_deref(),
            Some("minimal")
        );
    }

    // Forward table: every defined level maps to its exact budget.
    #[cfg(any(feature = "gemini", feature = "anthropic-api"))]
    #[test]
    fn budget_from_level_returns_exact_table_value_for_each_level() {
        assert_eq!(budget_from_level("none"), Some(0));
        assert_eq!(budget_from_level("minimal"), Some(512));
        assert_eq!(budget_from_level("low"), Some(1024));
        assert_eq!(budget_from_level("medium"), Some(8192));
        assert_eq!(budget_from_level("high"), Some(24576));
        assert_eq!(budget_from_level("xhigh"), Some(32768));
        assert_eq!(budget_from_level("max"), Some(128_000));
    }

    // Forward table: an unknown level yields None so callers can fall back.
    #[cfg(any(feature = "gemini", feature = "anthropic-api"))]
    #[test]
    fn budget_from_level_returns_none_for_unknown_level() {
        assert_eq!(budget_from_level("ludicrous"), None);
    }

    // Reverse table: each threshold boundary maps to the correct band.
    #[cfg(feature = "openai-responses")]
    #[test]
    fn level_from_budget_maps_thresholds_to_bands() {
        assert_eq!(level_from_budget(0), "none");
        assert_eq!(level_from_budget(512), "minimal");
        assert_eq!(level_from_budget(513), "low");
        assert_eq!(level_from_budget(1024), "low");
        assert_eq!(level_from_budget(1025), "medium");
        assert_eq!(level_from_budget(8192), "medium");
        assert_eq!(level_from_budget(8193), "high");
        assert_eq!(level_from_budget(24576), "high");
        assert_eq!(level_from_budget(24577), "xhigh");
        assert_eq!(level_from_budget(32768), "xhigh");
        assert_eq!(level_from_budget(32769), "max");
        assert_eq!(level_from_budget(128000), "max");
    }
}

#[cfg(test)]
#[cfg(any(feature = "anthropic-api", feature = "bedrock"))]
mod orphan_effort_tests {
    use super::drop_orphaned_output_config_effort;
    use serde_json::{Value, json};

    fn as_map(v: &mut Value) -> &mut serde_json::Map<String, Value> {
        v.as_object_mut().expect("object")
    }

    // effort is the sole sub-key -> the empty output_config is removed.
    #[test]
    fn effort_only_removes_output_config() {
        let mut body = json!({"model": "m", "output_config": {"effort": "high"}});
        drop_orphaned_output_config_effort(as_map(&mut body));
        assert!(body.get("output_config").is_none(), "got: {body}");
        assert_eq!(body["model"], "m", "siblings outside output_config survive");
    }

    // effort plus an orthogonal sibling -> effort goes, format + the
    // output_config object survive.
    #[test]
    fn effort_with_format_sibling_keeps_format() {
        let mut body =
            json!({"output_config": {"effort": "max", "format": {"type": "json_schema"}}});
        drop_orphaned_output_config_effort(as_map(&mut body));
        assert!(body["output_config"].get("effort").is_none(), "got: {body}");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    // No effort present -> a caller-supplied empty output_config{} is NOT
    // removed (the gate keys on `remove("effort").is_some()`).
    #[test]
    fn no_effort_leaves_empty_output_config_untouched() {
        let mut body = json!({"output_config": {}});
        drop_orphaned_output_config_effort(as_map(&mut body));
        assert!(
            body.get("output_config").is_some(),
            "empty output_config without effort must survive: {body}"
        );
    }

    // No output_config at all -> total no-op.
    #[test]
    fn absent_output_config_is_a_noop() {
        let mut body = json!({"model": "m"});
        drop_orphaned_output_config_effort(as_map(&mut body));
        assert_eq!(body, json!({"model": "m"}));
    }
}
