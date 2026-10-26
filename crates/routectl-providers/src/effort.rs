//! Shared effort helpers for the egresses.
//!
//! `wire_effort` is the one place the reasoning-OFF decision lives for the
//! egresses that translate the canonical effort: `effort: "none"` omits the
//! effort field rather than substituting a positive level, and every other
//! token rides as sent. The budget tables map effort levels to and from
//! Anthropic-style thinking budgets.

/// The six canonical effort tokens, in rank order from lowest to
/// highest. This is the single source of truth for the valid effort
/// vocabulary; the router's effort repair rows import this constant
/// instead of maintaining a separate copy.
pub const VALID_EFFORT_TOKENS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The effort a translating egress emits for the caller's `requested`.
///
/// Returns `None` when the caller's intent is reasoning-OFF (`requested ==
/// "none"`): every egress that TRANSLATES the canonical effort interprets
/// `None` as "omit the effort field" (or emit its own disable form), so a
/// client that explicitly declined thinking is never billed for it. Any other
/// token, including one outside [`VALID_EFFORT_TOKENS`], is returned as sent:
/// the upstream judges it, and a refused level is repaired per lane. An
/// effort the client sent on the anthropic-api `output_config` carrier is not
/// translated: that egress forwards it as sent, "none" included.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
pub(crate) fn wire_effort(requested: &str) -> Option<&str> {
    if routectl_core::ReasoningConfig::is_off_effort(requested) {
        return None;
    }
    Some(requested)
}

/// Exact effort-level -> `budget_tokens` lookup. Returns `None` for any
/// level outside the table so callers can fall back to their own
/// estimate rather than guess. This is the forward direction of the
/// effort<->budget bijection; `level_from_budget` is the reverse.
///
/// The table is independent of `VALID_EFFORT_TOKENS`:
/// it carries a "none" level (budget 0) and exact per-level budgets.
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
#[cfg(any(
    feature = "gemini",
    feature = "openai-responses",
    feature = "openai-compat"
))]
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

/// Pure JSON utility: remove `output_config.effort` from `obj`, and
/// drop the now-empty `output_config` object only if removing `effort`
/// actually removed something. Any orthogonal sibling (e.g.
/// structured-output `format`) is preserved, and a caller-supplied empty
/// `output_config {}` that carried no `effort` is left untouched.
///
/// This encodes NO business rule about WHEN orphaned effort should be
/// dropped -- each call site decides that and calls this only when the
/// strip is warranted.
#[cfg(feature = "bedrock")]
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
    use super::wire_effort;

    // A positive token is returned verbatim.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn positive_effort_returns_verbatim() {
        assert_eq!(wire_effort("max"), Some("max"));
        assert_eq!(wire_effort("xhigh"), Some("xhigh"));
    }

    // "none" is reasoning-OFF: the helper returns None so every egress omits
    // the effort field. It must NEVER resolve to a positive level.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn none_maps_to_omit() {
        assert_eq!(wire_effort("none"), None);
    }

    // A token outside the known vocabulary rides as sent.
    #[cfg(any(
        feature = "openai-compat",
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-responses"
    ))]
    #[test]
    fn unknown_effort_passes_through_verbatim() {
        assert_eq!(
            wire_effort("turbo"),
            Some("turbo"),
            "an unknown token must pass through"
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
#[cfg(feature = "bedrock")]
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
