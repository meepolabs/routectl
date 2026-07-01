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
//! Pattern-matches on `ToolDef::Other(v)["type"]` for tool-type keys
//! (e.g. `web_search`, `computer_use`). The `structured_output` key is
//! NOT tool-type-derived -- it is request-derived from two sources:
//!
//! - `provider_extras["output_config"]["format"]` set to a non-null
//!   value (Anthropic structured outputs), OR
//! - any tool requesting strict / constrained decoding: a
//!   `ToolDef::Custom` with `strict == Some(true)`, or a
//!   `ToolDef::Other(v)` carrying `v["strict"] == true`.
//!
//! Both rely on constrained decoding, which some upstreams (e.g. a
//! Bedrock Invoke leg on certain Claude models) do not enforce, yielding
//! malformed `tool_use` JSON the client cannot parse. Declaring
//! `unsupported_features = ["structured_output"]` on such a provider
//! steers these requests to a leg that DOES enforce it.

use routectl_core::ToolDef;
use serde_json::Value;

/// Feature key for requests that need constrained decoding -- either an
/// `output_config.format` structured output or a strict tool.
const STRUCTURED_OUTPUT_KEY: &str = "structured_output";

/// Derive feature keys from the request's `tools` array and
/// `provider_extras`. Tool-type keys come from `ToolDef::Other(v)
/// ["type"]` strings with a trailing `-YYYYMMDD` / `_YYYYMMDD` suffix
/// stripped so `web_search_20250305` and `web_search_20251102` both
/// reduce to `web_search`. Returns a deduped list in first-seen order.
///
/// Skips `ToolDef::Custom` and `Other` entries without a string `type`
/// for tool-type derivation. Additionally appends the request-derived
/// `structured_output` key (after any tool-type keys) when the request
/// needs constrained decoding -- see the module docs for the two
/// trigger sources. Pure: takes only what it reads, holds no router
/// state.
pub fn derive_feature_keys(tools: &[ToolDef], provider_extras: Option<&Value>) -> Vec<String> {
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
    if needs_structured_output(tools, provider_extras)
        && seen.insert(STRUCTURED_OUTPUT_KEY.to_string())
    {
        keys.push(STRUCTURED_OUTPUT_KEY.to_string());
    }
    keys
}

/// True when the request needs constrained decoding: an
/// `output_config.format` set to a non-null value, or any strict tool
/// (`ToolDef::Custom` with `strict == Some(true)`, or `ToolDef::Other`
/// carrying `"strict": true`).
fn needs_structured_output(tools: &[ToolDef], provider_extras: Option<&Value>) -> bool {
    let has_format = provider_extras
        .and_then(|v| v.get("output_config"))
        .and_then(|oc| oc.get("format"))
        .is_some_and(|fmt| !fmt.is_null());
    if has_format {
        return true;
    }
    tools.iter().any(|tool| match tool {
        ToolDef::Custom(c) => c.strict == Some(true),
        ToolDef::Other(v) => v.get("strict").and_then(serde_json::Value::as_bool) == Some(true),
    })
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
    if date_bytes.iter().all(u8::is_ascii_digit) {
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

    fn strict_custom_tool(name: &str, strict: Option<bool>) -> ToolDef {
        ToolDef::Custom(CustomTool {
            name: name.into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict,
            type_tag: None,
        })
    }

    #[test]
    fn derive_returns_empty_for_no_tools() {
        assert!(derive_feature_keys(&[], None).is_empty());
    }

    #[test]
    fn derive_returns_empty_for_only_custom_tools() {
        let tools = vec![custom_tool("calc"), custom_tool("send_email")];
        assert!(derive_feature_keys(&tools, None).is_empty());
    }

    #[test]
    fn derive_extracts_web_search_from_anthropic_builtin() {
        let tools = vec![ToolDef::Other(json!({
            "type": "web_search_20250305",
            "name": "search"
        }))];
        assert_eq!(derive_feature_keys(&tools, None), vec!["web_search"]);
    }

    #[test]
    fn derive_extracts_computer_use() {
        let tools = vec![ToolDef::Other(json!({
            "type": "computer_use_20250124",
            "name": "computer"
        }))];
        assert_eq!(derive_feature_keys(&tools, None), vec!["computer_use"]);
    }

    #[test]
    fn derive_emits_structured_output_when_output_config_format_present() {
        // Anthropic structured outputs land at
        // provider_extras["output_config"]["format"]; a non-null format
        // object means the request needs constrained decoding.
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object"}
                }
            }
        });
        let keys = derive_feature_keys(&[], Some(&extras));
        assert_eq!(keys, vec!["structured_output"]);
    }

    #[test]
    fn derive_no_structured_output_when_output_config_without_format() {
        // output_config present but carrying no `format` -> not a
        // structured-output request, no key emitted.
        let extras = json!({
            "output_config": {
                "effort": "high"
            }
        });
        assert!(derive_feature_keys(&[], Some(&extras)).is_empty());
    }

    #[test]
    fn derive_no_structured_output_when_format_is_null() {
        // An explicit null format is the same as absent -- no constraint.
        let extras = json!({
            "output_config": {
                "format": null
            }
        });
        assert!(derive_feature_keys(&[], Some(&extras)).is_empty());
    }

    #[test]
    fn derive_no_structured_output_when_extras_not_object() {
        // provider_extras that is not an object (e.g. a bare array)
        // cannot carry output_config; no key.
        let extras = json!([1, 2, 3]);
        assert!(derive_feature_keys(&[], Some(&extras)).is_empty());
    }

    #[test]
    fn derive_emits_structured_output_for_strict_custom_tool() {
        // A strict custom tool relies on constrained decoding too.
        let tools = vec![strict_custom_tool("lookup", Some(true))];
        assert_eq!(derive_feature_keys(&tools, None), vec!["structured_output"]);
    }

    #[test]
    fn derive_no_structured_output_for_non_strict_custom_tool() {
        // strict: Some(false) and strict: None both mean no constraint.
        let off = vec![strict_custom_tool("lookup", Some(false))];
        assert!(derive_feature_keys(&off, None).is_empty());
        let unset = vec![strict_custom_tool("lookup", None)];
        assert!(derive_feature_keys(&unset, None).is_empty());
    }

    #[test]
    fn derive_emits_structured_output_for_strict_other_tool() {
        // A forward-compat Other tool carrying "strict": true also
        // demands constrained decoding.
        let tools = vec![ToolDef::Other(json!({
            "type": "some_builtin",
            "name": "thing",
            "strict": true
        }))];
        let keys = derive_feature_keys(&tools, None);
        // The Other tool also carries a `type`, so its tool-type key must
        // co-occur with structured_output (tool-type first, SO appended).
        assert_eq!(
            keys,
            vec!["some_builtin".to_string(), "structured_output".to_string()]
        );
    }

    #[test]
    fn derive_appends_structured_output_after_tool_type_keys() {
        // structured_output co-occurring with a web_search tool: both
        // keys present, tool-type key first, structured_output appended.
        let tools = vec![ToolDef::Other(json!({
            "type": "web_search_20250305",
            "name": "search"
        }))];
        let extras = json!({
            "output_config": { "format": {"type": "json_schema"} }
        });
        assert_eq!(
            derive_feature_keys(&tools, Some(&extras)),
            vec!["web_search".to_string(), "structured_output".to_string()]
        );
    }

    #[test]
    fn derive_dedups_structured_output_from_both_sources() {
        // A strict custom tool AND an output_config.format both fire;
        // the key appears exactly once.
        let tools = vec![strict_custom_tool("lookup", Some(true))];
        let extras = json!({
            "output_config": { "format": {"type": "json_schema"} }
        });
        assert_eq!(
            derive_feature_keys(&tools, Some(&extras)),
            vec!["structured_output".to_string()]
        );
    }

    #[test]
    fn derive_dedups_multiple_versions_of_same_feature() {
        // `web_search_20250305` and `web_search_20251102` both reduce
        // to `web_search`; the deduper keeps only one entry.
        let tools = vec![
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "search"})),
            ToolDef::Other(json!({"type": "web_search_20251102", "name": "search"})),
        ];
        assert_eq!(derive_feature_keys(&tools, None), vec!["web_search"]);
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
            derive_feature_keys(&tools, None),
            vec!["web_search".to_string(), "computer_use".to_string()]
        );
    }

    #[test]
    fn derive_skips_other_with_non_string_type() {
        let tools = vec![ToolDef::Other(json!({
            "type": 42,
            "name": "weird"
        }))];
        assert!(derive_feature_keys(&tools, None).is_empty());
    }

    #[test]
    fn derive_skips_other_without_type_field() {
        let tools = vec![ToolDef::Other(json!({
            "name": "no_type"
        }))];
        assert!(derive_feature_keys(&tools, None).is_empty());
    }

    #[test]
    fn derive_keeps_custom_and_other_in_mixed_list() {
        // Custom tools get skipped; Other tools contribute their key.
        let tools = vec![
            custom_tool("calc"),
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "search"})),
        ];
        assert_eq!(derive_feature_keys(&tools, None), vec!["web_search"]);
    }

    #[test]
    fn derive_handles_unversioned_other_type() {
        // Some upstreams ship unversioned built-in tool types; those
        // pass through verbatim as the feature key.
        let tools = vec![ToolDef::Other(json!({"type": "bash", "name": "bash"}))];
        assert_eq!(derive_feature_keys(&tools, None), vec!["bash"]);
    }
}
