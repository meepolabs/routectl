//! The models.dev converter: field mapping, api host handling, duplicate
//! keys, and the vendored source shape end to end.

use serde_json::{Value, json};

use super::*;

fn date() -> TableDate {
    TableDate::parse("2026-11-01").expect("date")
}

fn convert(value: &Value) -> Result<PriceTable, PriceTableError> {
    from_models_dev(&serde_json::to_vec(value).expect("json"), date())
}

fn canonical(table: &PriceTable) -> Value {
    serde_json::from_slice(&table.to_canonical_json()).expect("canonical json")
}

#[test]
fn cost_fields_map_onto_the_canonical_row_with_cache_write_as_the_5m_tier() {
    let source = json!({
        "anthropic": {
            "id": "anthropic", "name": "Anthropic", "npm": "@ai-sdk/anthropic",
            "models": {
                "claude-x": {
                    "id": "claude-x", "limit": { "context": 200000 },
                    "cost": { "input": 3, "output": 15, "cache_read": 0.3,
                              "cache_write": 3.75, "reasoning": 15,
                              "context_over_200k": { "input": 6 } }
                }
            }
        }
    });

    let doc = canonical(&convert(&source).expect("converts"));

    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["generated_at"], "2026-11-01");
    assert_eq!(
        doc["providers"][0],
        json!({ "id": "anthropic", "models": [{
            "id": "claude-x", "input": 3.0, "output": 15.0, "cache_read": 0.3,
            "cache_write_5m": 3.75
        }]})
    );
}

#[test]
fn api_base_urls_are_normalized_and_unusable_ones_dropped() {
    let source = json!({
        "go": { "api": "https://OpenCode.ai/zen/go/v1/", "models": { "m": { "cost": { "input": 1 } } } },
        "tmpl": { "api": "${GATEWAY_BASE_URL}/v1", "models": { "m": { "cost": { "input": 1 } } } },
        "ftp": { "api": "ftp://files.example/v1", "models": { "m": { "cost": { "input": 1 } } } },
        "first": { "models": { "m": { "cost": { "input": 1 } } } },
    });

    let doc = canonical(&convert(&source).expect("converts"));
    let providers = doc["providers"].as_array().expect("array");

    let host = |id: &str| {
        providers
            .iter()
            .find(|p| p["id"] == id)
            .map(|p| p["api_host"].clone())
    };
    assert_eq!(host("go"), Some(json!("https://opencode.ai:443/zen/go/v1")));
    assert_eq!(host("tmpl"), Some(Value::Null));
    assert_eq!(host("ftp"), Some(Value::Null));
    assert_eq!(host("first"), Some(Value::Null));
}

#[test]
fn models_without_cost_are_omitted_and_bad_prices_drop_to_unknown() {
    let source = json!({
        "p": { "models": {
            "free": { "cost": { "input": 0, "output": 0 } },
            "nocost": { "name": "x" },
            "bad": { "cost": { "input": -1, "output": 20000, "cache_read": 0.5 } },
        }}
    });

    let doc = canonical(&convert(&source).expect("converts"));

    assert_eq!(
        doc["providers"][0]["models"],
        json!([
            { "id": "bad", "cache_read": 0.5 },
            { "id": "free", "input": 0.0, "output": 0.0 },
        ])
    );
}

#[test]
fn source_shape_errors_fail_closed() {
    let rows = [
        (
            "duplicate model key",
            r#"{"p":{"models":{"m":{"cost":{"input":1}},"m":{"cost":{"input":2}}}}}"#,
            Some(PriceTableError::DuplicateModel),
        ),
        (
            "duplicate provider key",
            r#"{"p":{"models":{"m":{"cost":{"input":1}}}},"p":{"models":{"n":{"cost":{"input":1}}}}}"#,
            Some(PriceTableError::DuplicateProvider),
        ),
        (
            "no priced model",
            r#"{"p":{"models":{"m":{}}}}"#,
            Some(PriceTableError::NoPricedModels),
        ),
        (
            "empty document",
            "{}",
            Some(PriceTableError::NoPricedModels),
        ),
        ("not an object", "[]", None),
        (
            "price as a string",
            r#"{"p":{"models":{"m":{"cost":{"input":"1"}}}}}"#,
            None,
        ),
    ];
    for (name, text, expected) in rows {
        let result = from_models_dev(text.as_bytes(), date());
        match expected {
            Some(err) => assert_eq!(result.err(), Some(err), "{name}"),
            None => assert!(
                matches!(result, Err(PriceTableError::Syntax { .. })),
                "{name}"
            ),
        }
    }
}

const VENDORED_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/catalog_data/models_dev.json");

#[test]
fn the_vendored_models_dev_snapshot_converts_and_selects_as_designed() {
    let source = std::fs::read(VENDORED_SOURCE).expect("vendored models.dev snapshot is readable");

    let converted = from_models_dev(&source, date()).expect("snapshot converts");
    let reparsed =
        parse_and_validate(&converted.to_canonical_json()).expect("canonical output reparses");

    assert_eq!(reparsed, converted);
    let price = |kind: &str, base: Option<&str>, model: &str| {
        converted
            .price_for_target(kind, base, model)
            .and_then(|p| p.input_per_mtok)
    };
    assert_eq!(
        price(
            "openai-compat",
            Some("https://opencode.ai/zen/go/v1"),
            "deepseek-v4-pro"
        ),
        Some(0.435),
        "opencode-go, not opencode"
    );
    assert_eq!(
        price(
            "openai-compat",
            Some("https://opencode.ai/zen/v1"),
            "deepseek-v4-pro"
        ),
        Some(1.74),
        "opencode"
    );
    assert_eq!(
        price(
            "anthropic-api",
            Some("https://api.anthropic.com"),
            "claude-sonnet-4-6"
        ),
        Some(3.0)
    );
    assert_eq!(
        converted
            .price_for_target("anthropic-api", None, "claude-sonnet-4-6")
            .and_then(|p| p.cache_write_5m_per_mtok),
        Some(3.75)
    );
    assert_eq!(
        price(
            "openai-compat",
            Some("http://127.0.0.1:1234/v1"),
            "qwen/qwen3-coder-30b"
        ),
        None,
        "a loopback listener never selects lmstudio"
    );
}
