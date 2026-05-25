//! Feature-key derivation for the alias-chain pre-filter.
//!
//! `unsupported_features` on `ProviderRuntimePolicy` is an
//! operator-supplied list of feature keys (e.g. `web_search`,
//! `computer_use`) the operator has declared the provider does not
//! support. Before walking the alias chain, the router maps the
//! request's tool definitions to feature keys and filters out any
//! chain entry whose provider lists ANY of those keys. The result is
//! the original chain minus skipped providers; an empty chain after
//! filter surfaces as `Error::NotImplemented` rather than walking and
//! getting per-target 400s from each upstream that can't handle the
//! feature.
//!
//! Pattern-matches on `ToolDef::Other(v)["type"]`. `ToolDef::Custom`
//! tools are user-defined and do not contribute feature keys.

use routectl_core::ToolDef;

/// Derive feature keys from the request's `tools` array. Strips a
/// trailing `-YYYYMMDD` or `_YYYYMMDD` suffix so
/// `web_search_20250305` and `web_search_20251102` both reduce to
/// `web_search`. Returns a deduped list in first-seen order.
///
/// Walks `ToolDef::Other(v)["type"]` strings; skips `ToolDef::Custom`
/// (user-defined tools have no version-stamped type) and `Other`
/// entries without a string `type` field.
pub(crate) fn derive_feature_keys(tools: &[ToolDef]) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut keys: Vec<String> = Vec::new();
    for tool in tools {
        match tool {
            ToolDef::Custom(_) => continue,
            ToolDef::Other(v) => {
                let Some(type_str) = v.get("type").and_then(|t| t.as_str()) else {
                    continue;
                };
                let key = strip_date_suffix(type_str);
                if seen.insert(key.to_string()) {
                    keys.push(key.to_string());
                }
            }
        }
    }
    keys
}

/// Strip a trailing `-YYYYMMDD` or `_YYYYMMDD` suffix if present.
/// Returns the input unchanged when the trailing 9 chars don't match
/// the date pattern (separator byte + 8 ASCII digits).
fn strip_date_suffix(s: &str) -> &str {
    if s.len() < 9 {
        return s;
    }
    let bytes = s.as_bytes();
    let suffix_start = s.len() - 9;
    let sep = bytes[suffix_start];
    if sep != b'-' && sep != b'_' {
        return s;
    }
    let date_bytes = &bytes[suffix_start + 1..];
    if date_bytes.iter().all(|b| b.is_ascii_digit()) {
        &s[..suffix_start]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::CustomTool;
    use serde_json::json;

    fn custom_tool(name: &str) -> ToolDef {
        ToolDef::Custom(CustomTool {
            name: name.into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })
    }

    #[test]
    fn strip_date_suffix_handles_dash() {
        assert_eq!(strip_date_suffix("web-search-20250305"), "web-search");
    }

    #[test]
    fn strip_date_suffix_handles_underscore() {
        assert_eq!(strip_date_suffix("web_search_20250305"), "web_search");
        assert_eq!(strip_date_suffix("computer_use_20250124"), "computer_use");
    }

    #[test]
    fn strip_date_suffix_no_match_passes_through() {
        assert_eq!(strip_date_suffix("web_search"), "web_search");
        assert_eq!(strip_date_suffix("custom_tool"), "custom_tool");
    }

    #[test]
    fn strip_date_suffix_short_string_passes_through() {
        // Less than 9 chars total -- no trailing date pattern fits.
        assert_eq!(strip_date_suffix("abc"), "abc");
        assert_eq!(strip_date_suffix("12345678"), "12345678");
    }

    #[test]
    fn strip_date_suffix_non_digit_after_separator_passes_through() {
        // Has 9-char tail but tail isn't separator-then-8-digits.
        assert_eq!(strip_date_suffix("foo-bar20250305"), "foo-bar20250305");
        assert_eq!(
            strip_date_suffix("web_search_abcdef12"),
            "web_search_abcdef12"
        );
    }

    #[test]
    fn strip_date_suffix_separator_alone_passes_through() {
        // Exactly 9 chars but not separator + 8 digits.
        assert_eq!(strip_date_suffix("a_bcdefgh"), "a_bcdefgh");
    }

    #[test]
    fn derive_returns_empty_for_no_tools() {
        assert!(derive_feature_keys(&[]).is_empty());
    }

    #[test]
    fn derive_returns_empty_for_only_custom_tools() {
        let tools = vec![custom_tool("calc"), custom_tool("send_email")];
        assert!(derive_feature_keys(&tools).is_empty());
    }

    #[test]
    fn derive_extracts_web_search_from_anthropic_builtin() {
        let tools = vec![ToolDef::Other(json!({
            "type": "web_search_20250305",
            "name": "search"
        }))];
        assert_eq!(derive_feature_keys(&tools), vec!["web_search"]);
    }

    #[test]
    fn derive_extracts_computer_use() {
        let tools = vec![ToolDef::Other(json!({
            "type": "computer_use_20250124",
            "name": "computer"
        }))];
        assert_eq!(derive_feature_keys(&tools), vec!["computer_use"]);
    }

    #[test]
    fn derive_dedups_multiple_versions_of_same_feature() {
        // `web_search_20250305` and `web_search_20251102` both reduce
        // to `web_search`; the deduper keeps only one entry.
        let tools = vec![
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "search"})),
            ToolDef::Other(json!({"type": "web_search_20251102", "name": "search"})),
        ];
        assert_eq!(derive_feature_keys(&tools), vec!["web_search"]);
    }

    #[test]
    fn derive_keeps_distinct_features_in_order() {
        // Multiple distinct features keep first-seen order so the log
        // / error message lists them deterministically.
        let tools = vec![
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "search"})),
            ToolDef::Other(json!({"type": "computer_use_20250124", "name": "computer"})),
        ];
        assert_eq!(
            derive_feature_keys(&tools),
            vec!["web_search".to_string(), "computer_use".to_string()]
        );
    }

    #[test]
    fn derive_skips_other_with_non_string_type() {
        let tools = vec![ToolDef::Other(json!({
            "type": 42,
            "name": "weird"
        }))];
        assert!(derive_feature_keys(&tools).is_empty());
    }

    #[test]
    fn derive_skips_other_without_type_field() {
        let tools = vec![ToolDef::Other(json!({
            "name": "no_type"
        }))];
        assert!(derive_feature_keys(&tools).is_empty());
    }

    #[test]
    fn derive_keeps_custom_and_other_in_mixed_list() {
        // Custom tools get skipped; Other tools contribute their key.
        let tools = vec![
            custom_tool("calc"),
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "search"})),
        ];
        assert_eq!(derive_feature_keys(&tools), vec!["web_search"]);
    }

    #[test]
    fn derive_handles_unversioned_other_type() {
        // Some upstreams ship unversioned built-in tool types; those
        // pass through verbatim as the feature key.
        let tools = vec![ToolDef::Other(json!({"type": "bash", "name": "bash"}))];
        assert_eq!(derive_feature_keys(&tools), vec!["bash"]);
    }
}
