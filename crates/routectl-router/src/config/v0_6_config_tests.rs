//! Tests for the v0.6.0+ config shapes: `[models]` table and the
//! untagged `AliasValue` enum.
//!
//! Breaking change: `thinking` and `effort` fields were removed from
//! `ModelEntry`. TOMLs carrying those keys must fail at parse time.

use super::{AliasValue, Config, HistoryReasoning, ModelEntry, ReasoningDialect};
use std::collections::BTreeMap;

/// A model entry with only the two required fields gets the correct
/// defaults.
#[test]
fn model_entry_required_fields_only() {
    let toml_text = r#"
[models.haiku]
provider = "anthropic"
upstream = "claude-haiku-4-5-20251001"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("haiku").expect("haiku entry");
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.upstream, "claude-haiku-4-5-20251001");
    assert!(m.selectable, "default selectable = true");
    assert!(m.reasoning_dialect.is_none());
    assert!(m.history_reasoning.is_none());
    assert!(m.header_extras.is_empty());
    assert!(m.payload_extras.is_none());
}

/// TOMLs carrying the old `thinking` key must fail at parse time.
/// `deny_unknown_fields` on `ModelEntry` surfaces the old key as a
/// parse error so misconfigurations are caught at startup.
#[test]
fn model_entry_rejects_old_thinking_field() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
thinking = "adaptive"
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("thinking"),
        "expected error to name the removed field 'thinking'; got: {msg}"
    );
}

/// TOMLs carrying the old `effort` key must fail at parse time.
#[test]
fn model_entry_rejects_old_effort_field() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
effort = "high"
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("effort"),
        "expected error to name the removed field 'effort'; got: {msg}"
    );
}

#[test]
fn model_entry_rejects_removed_adaptive_thinking_field() {
    // v0.6.0 dropped `adaptive_thinking`; deny_unknown_fields makes
    // the old key reject at startup so the upgrade isn't silent.
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
adaptive_thinking = true
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("adaptive_thinking"),
        "expected error to name the removed field; got: {msg}"
    );
}

#[test]
fn model_entry_rejects_removed_anthropic_beta_field() {
    // v0.6.0 dropped the per-model `anthropic_beta: Vec<String>`
    // field; operators set `anthropic-beta` via `header_extras`.
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
anthropic_beta = ["context-1m-2025-08-07"]
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("anthropic_beta"),
        "expected error to name the removed field; got: {msg}"
    );
}

#[test]
fn model_entry_rejects_removed_default_extras_field() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
default_extras = { foo = "bar" }
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("default_extras"),
        "expected error to name the removed field; got: {msg}"
    );
}

#[test]
fn model_entry_header_extras_round_trip() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
header_extras = { "anthropic-beta" = "context-1m-2025-08-07", "x-app" = "cli" }
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("opus").expect("entry");
    assert_eq!(
        m.header_extras.get("anthropic-beta"),
        Some(&"context-1m-2025-08-07".to_string())
    );
    assert_eq!(m.header_extras.get("x-app"), Some(&"cli".to_string()));
}

#[test]
fn model_entry_payload_extras_round_trip() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
payload_extras = { nested = { key = "value" }, scalar = 42 }
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("opus").expect("entry");
    let extras = m.payload_extras.as_ref().expect("payload_extras set");
    assert_eq!(
        extras.get("nested").and_then(|v| v.get("key")),
        Some(&serde_json::json!("value"))
    );
    assert_eq!(extras.get("scalar"), Some(&serde_json::json!(42)));
}

#[test]
fn model_entry_reasoning_dialect_round_trip() {
    let toml_text = r#"
[models.m]
provider = "p"
upstream = "u"
reasoning_dialect = "deepseek"
history_reasoning = "preserve"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("m").expect("entry");
    assert_eq!(m.reasoning_dialect, Some(ReasoningDialect::Deepseek));
    assert_eq!(m.history_reasoning, Some(HistoryReasoning::Preserve));
}

#[test]
fn provider_entry_rejects_removed_extra_headers_field() {
    // Provider-side `extra_headers` was renamed to `header_extras`;
    // deny_unknown_fields surfaces the old key as a parse error.
    let toml_text = r#"
[providers.bad]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
extra_headers = { "x-foo" = "bar" }
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("extra_headers"),
        "expected error to name the removed field; got: {msg}"
    );
}

#[test]
fn provider_entry_rejects_reasoning_dialect_on_provider() {
    // Moved to [models.X]; provider-side key must reject.
    let toml_text = r#"
[providers.bad]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
reasoning_dialect = "deepseek"
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("reasoning_dialect"),
        "expected error to name the removed field; got: {msg}"
    );
}

#[test]
fn alias_value_parses_single_string() {
    let toml_text = r#"
"claude-opus-4-7" = "heavy"
"#;
    let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
    let entry = v.get("claude-opus-4-7").expect("entry");
    match entry {
        AliasValue::Single(s) => assert_eq!(s, "heavy"),
        other => panic!("expected Single, got {other:?}"),
    }
}

#[test]
fn alias_value_parses_chain_list() {
    let toml_text = r#"
"fast" = ["nano", "mini"]
"#;
    let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
    let entry = v.get("fast").expect("entry");
    match entry {
        AliasValue::Chain(c) => assert_eq!(c, &vec!["nano".to_string(), "mini".to_string()]),
        other => panic!("expected Chain, got {other:?}"),
    }
}

#[test]
fn alias_value_default_special_key() {
    let toml_text = r#"
default = "small"
"claude-opus-4-7" = "heavy"
"#;
    let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
    let default_entry = v.get("default").expect("default entry");
    match default_entry {
        AliasValue::Single(s) => assert_eq!(s, "small"),
        other => panic!("expected Single, got {other:?}"),
    }
    assert!(v.contains_key("claude-opus-4-7"));
}

#[test]
fn alias_value_suffix_glob_parses() {
    let toml_text = r#"
"claude-opus-*" = "opus"
"claude-*" = "fallback"
"#;
    let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
    assert!(v.contains_key("claude-opus-*"));
    assert!(v.contains_key("claude-*"));
}

#[test]
fn provider_kind_field_is_kind_not_type() {
    let toml_text = r#"
[providers.deepseek]
kind = "openai-compat"
base_url = "https://api.deepseek.com/v1"
api_key_ref = "literal:k"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert!(cfg.providers.contains_key("deepseek"));
}

#[test]
fn model_entry_disabled_field() {
    let toml_text = r#"
[models.shelved]
provider = "p"
upstream = "u"
selectable = false
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("shelved").expect("entry");
    assert!(!m.selectable);
}

/// `ModelEntry::new` defaults match the TOML defaults: selectable=true.
#[test]
fn model_entry_builder_defaults_match_toml_defaults() {
    let m = ModelEntry::new("p", "u");
    assert_eq!(m.provider, "p");
    assert_eq!(m.upstream, "u");
    assert!(m.selectable);
}

#[test]
fn alias_value_chain_iter_yields_in_order() {
    let v = AliasValue::Chain(vec!["a".into(), "b".into(), "c".into()]);
    let names: Vec<&str> = v.nicknames().collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn alias_value_single_iter_yields_one() {
    let v = AliasValue::Single("solo".into());
    let names: Vec<&str> = v.nicknames().collect();
    assert_eq!(names, vec!["solo"]);
}

#[test]
fn model_entry_stream_first_byte_timeout_ms_round_trip() {
    let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
stream_first_byte_timeout_ms = 300000
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("opus").expect("opus entry");
    assert_eq!(m.stream_first_byte_timeout_ms, Some(300_000));
}

/// `max_thinking_entry_bytes` round-trips through TOML and defaults
/// to `None` when omitted (the runtime falls back to the default
/// 1 MiB cap).
#[test]
fn anthropic_api_max_thinking_entry_bytes_round_trip() {
    use crate::config::{Config, ProviderEntry};

    // Default: omitted -> None.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse default");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    match entry {
        ProviderEntry::AnthropicApi {
            max_thinking_entry_bytes,
            ..
        } => assert!(
            max_thinking_entry_bytes.is_none(),
            "default must be None; got: {max_thinking_entry_bytes:?}"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }

    // Explicit value round-trips through serde.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
max_thinking_entry_bytes = 2097152
"#;
    let cfg_in: Config = toml::from_str(toml_text).expect("parse explicit");
    match cfg_in.providers.get("anthropic").expect("anthropic") {
        ProviderEntry::AnthropicApi {
            max_thinking_entry_bytes,
            ..
        } => assert_eq!(*max_thinking_entry_bytes, Some(2_097_152)),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
    let serialized = toml::to_string(&cfg_in).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    match cfg_out.providers.get("anthropic").expect("anthropic") {
        ProviderEntry::AnthropicApi {
            max_thinking_entry_bytes,
            ..
        } => assert_eq!(*max_thinking_entry_bytes, Some(2_097_152)),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// When `max_thinking_entry_bytes` is unset on the TOML, the
/// runtime resolution lands on the
/// `AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES` baseline (1 MiB).
#[test]
fn anthropic_api_max_thinking_entry_bytes_unset_resolves_to_default() {
    use crate::config::ProviderEntry;
    use crate::factory::resolve_max_thinking_entry_bytes_for_test;
    use routectl_providers::anthropic_api::AnthropicApiConfig;

    let entry = ProviderEntry::anthropic_api("literal:sk-ant-test");
    let configured = match &entry {
        ProviderEntry::AnthropicApi {
            max_thinking_entry_bytes,
            ..
        } => *max_thinking_entry_bytes,
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    };
    assert!(configured.is_none(), "constructor must default to None");
    let resolved = resolve_max_thinking_entry_bytes_for_test("test", configured);
    assert_eq!(
        resolved,
        AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        "None must resolve to the 1 MiB default"
    );
    assert_eq!(resolved, 1024 * 1024, "default must be 1 MiB");
}

#[test]
fn max_thinking_entry_bytes_zero_resolves_to_default() {
    let resolved = crate::factory::resolve_max_thinking_entry_bytes_for_test("test", Some(0));
    assert_eq!(
        resolved,
        routectl_providers::anthropic_api::AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        "Some(0) must fall back to the default cap, not zero"
    );
}

/// A `[providers.X.cloak]` block with mode + strict_mode + tool_rename
/// (array of tables) + sensitive_words parses into the entry's
/// `CloakConfig`, and round-trips through serialize + re-parse.
#[test]
fn anthropic_api_cloak_block_parses_and_round_trips() {
    use crate::config::ProviderEntry;
    use routectl_providers::anthropic_api::CloakMode;
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.cloak]
mode = "always"
strict_mode = true
sensitive_words = ["secret", "token"]

[[providers.anthropic.cloak.tool_rename]]
from = "foo"
to = "bar"

[[providers.anthropic.cloak.tool_rename]]
from = "baz"
to = "qux"
"#;
    let cfg_in: Config = toml::from_str(toml_text).expect("parse cloak block");
    let assert_cloak = |entry: &ProviderEntry| match entry {
        ProviderEntry::AnthropicApi { cloak, .. } => {
            assert_eq!(cloak.mode, CloakMode::Always);
            assert!(cloak.strict_mode);
            assert_eq!(cloak.sensitive_words, vec!["secret", "token"]);
            assert_eq!(cloak.tool_rename.len(), 2);
            assert_eq!(cloak.tool_rename[0].from, "foo");
            assert_eq!(cloak.tool_rename[0].to, "bar");
            assert_eq!(cloak.tool_rename[1].from, "baz");
            assert_eq!(cloak.tool_rename[1].to, "qux");
        }
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    };
    assert_cloak(cfg_in.providers.get("anthropic").expect("anthropic"));

    // Serialize + re-parse: the cloak surface must survive the round-trip.
    let serialized = toml::to_string(&cfg_in).expect("serialize");
    let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
    assert_cloak(cfg_out.providers.get("anthropic").expect("anthropic"));
}

/// Omitting the `[cloak]` block yields `CloakConfig::default()` (mode
/// auto, no strict mode, empty tool_rename + sensitive_words).
#[test]
fn anthropic_api_cloak_omitted_yields_default() {
    use crate::config::ProviderEntry;
    use routectl_providers::anthropic_api::CloakMode;

    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    let cfg_in: Config = toml::from_str(toml_text).expect("parse without cloak");
    match cfg_in.providers.get("anthropic").expect("anthropic") {
        ProviderEntry::AnthropicApi { cloak, .. } => {
            assert_eq!(cloak.mode, CloakMode::Auto);
            assert!(!cloak.strict_mode);
            assert!(cloak.tool_rename.is_empty());
            assert!(cloak.sensitive_words.is_empty());
        }
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}
