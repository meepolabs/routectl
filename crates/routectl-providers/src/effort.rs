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
//! Anthropic-shape egresses (anthropic_api, bedrock) accept "xhigh"
//! and "max" verbatim and MUST NOT call this helper.

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
const RANK_ORDER: &[&str] = &VALID_EFFORT_TOKENS;

/// Clamp `requested` to the nearest supported level on the standard
/// rank order:
///
///   minimal < low < medium < high < xhigh < max
///
/// Rules:
///   - If `supported` is empty, return `requested` unchanged (passthrough).
///   - If `supported` contains `requested`, return `requested` unchanged.
///   - Otherwise, pick the highest supported level that is <= requested
///     on the rank order above.
///   - If no supported level is <= requested (e.g., requested="minimal"
///     but supported only contains ["high"]), pick the lowest supported
///     level.
///   - Emits `tracing::debug!` whenever a clamp actually changes the value,
///     with fields `requested`, `applied`, and `supported`.
///   - Emits `tracing::warn!` when `supported` is non-empty AND `requested`
///     is not in the standard rank order (unknown string). In that case the
///     function still picks the lowest supported level as a safe default.
pub(crate) fn clamp_effort_to_supported<'a>(
    requested: &'a str,
    supported: &[String],
) -> Cow<'a, str> {
    // Empty supported list -> passthrough semantics.
    if supported.is_empty() {
        return Cow::Borrowed(requested);
    }

    // Fast path: the requested level is already in the supported set.
    if supported.iter().any(|s| s == requested) {
        return Cow::Borrowed(requested);
    }

    // Locate the requested level in the canonical rank order.
    let requested_rank = RANK_ORDER.iter().position(|&r| r == requested);

    let Some(req_rank) = requested_rank else {
        // Unknown effort string: warn and fall back to the lowest supported.
        let lowest = lowest_supported(supported);
        tracing::warn!(
            requested = requested,
            applied = %lowest,
            supported = ?supported,
            "effort string is not in the standard rank order; clamping to lowest supported"
        );
        return Cow::Owned(lowest.to_owned());
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
    Cow::Owned(applied.to_owned())
}

/// Return the lowest-ranked string in `supported` by the standard rank
/// order. Falls back to the first element for strings not in the order.
///
/// Precondition: `supported` is non-empty. Callers must ensure this
/// before invoking (every call site checks `supported.is_empty()`
/// earlier and returns early).
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
        .map(|s| s.as_str())
        .expect("supported is non-empty -- precondition violated")
}

#[cfg(test)]
mod tests {
    use super::{clamp_effort_to_supported, RANK_ORDER, VALID_EFFORT_TOKENS};

    // VALID_EFFORT_TOKENS and RANK_ORDER must stay in sync: same elements,
    // same order. If either is updated, the other must follow.
    #[test]
    fn effort_tokens_and_rank_order_in_sync() {
        assert_eq!(
            VALID_EFFORT_TOKENS.as_slice(),
            RANK_ORDER,
            "VALID_EFFORT_TOKENS and RANK_ORDER must be identical slices"
        );
    }

    // Helper to build a Vec<String> from a slice of &str.
    fn levels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    // Empty supported -> requested returned verbatim (passthrough).
    #[test]
    fn empty_supported_returns_requested() {
        assert_eq!(clamp_effort_to_supported("max", &[]), "max");
        assert_eq!(clamp_effort_to_supported("xhigh", &[]), "xhigh");
        assert_eq!(clamp_effort_to_supported("anything", &[]), "anything");
    }

    // Requested level is in supported -> verbatim, no allocation.
    #[test]
    fn requested_in_supported_returns_verbatim() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(clamp_effort_to_supported("high", &sup), "high");
        assert_eq!(clamp_effort_to_supported("low", &sup), "low");
    }

    // requested="max", supported=["low","medium","high"] -> "high".
    #[test]
    fn max_clamps_to_high() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(clamp_effort_to_supported("max", &sup), "high");
    }

    // requested="xhigh", supported=["low","medium","high"] -> "high".
    #[test]
    fn xhigh_clamps_to_high() {
        let sup = levels(&["low", "medium", "high"]);
        assert_eq!(clamp_effort_to_supported("xhigh", &sup), "high");
    }

    // requested="minimal", supported=["medium","high"] -> "medium"
    // (nothing <= minimal in supported, so pick lowest = "medium").
    #[test]
    fn minimal_below_all_supported_picks_lowest() {
        let sup = levels(&["medium", "high"]);
        assert_eq!(clamp_effort_to_supported("minimal", &sup), "medium");
    }

    // requested="low", supported=["medium","high"] -> "medium"
    // (nothing <= low in supported, so pick lowest = "medium").
    #[test]
    fn low_below_all_supported_picks_lowest() {
        let sup = levels(&["medium", "high"]);
        assert_eq!(clamp_effort_to_supported("low", &sup), "medium");
    }

    // Unknown effort string with non-empty supported -> warn + lowest.
    // We cannot easily test the warn emission in unit tests without
    // tracing-test, but we can verify the returned value is the lowest
    // supported level.
    #[test]
    fn unknown_effort_returns_lowest_supported() {
        let sup = levels(&["low", "medium"]);
        let result = clamp_effort_to_supported("unknown_str", &sup);
        assert_eq!(result, "low");
    }

    // Clamp from a mid-level works correctly.
    #[test]
    fn high_clamps_to_medium_when_only_low_medium_supported() {
        let sup = levels(&["low", "medium"]);
        assert_eq!(clamp_effort_to_supported("high", &sup), "medium");
    }

    // Single-element supported list: everything maps to it.
    #[test]
    fn single_element_supported_always_returns_it() {
        let sup = levels(&["medium"]);
        assert_eq!(clamp_effort_to_supported("max", &sup), "medium");
        assert_eq!(clamp_effort_to_supported("minimal", &sup), "medium");
        assert_eq!(clamp_effort_to_supported("medium", &sup), "medium");
    }

    // minimal < low: low clamps down to minimal when minimal is sole option.
    #[test]
    fn low_clamps_down_when_only_minimal_supported() {
        let sup = levels(&["minimal"]);
        // low > minimal; highest <= low in supported is minimal.
        assert_eq!(clamp_effort_to_supported("low", &sup), "minimal");
    }
}
