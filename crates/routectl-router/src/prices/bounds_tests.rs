//! Bounds enforced while a table is parsed: the id byte set, the row and
//! provider counts on both input formats, and the schema version probe.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::prices_test_support::{bytes, one_provider, priced_model, table};
use super::validate::MAX_ROWS;
use super::*;

fn date() -> TableDate {
    TableDate::parse("2026-11-01").expect("date")
}

const ACCEPTED_IDS: &[(&str, &str)] = &[
    ("fireworks path", "accounts/fireworks/models/glm-5"),
    ("vertex version tag", "claude-opus-4-1@20250805"),
    ("colon tag", "qwen3-coder:480b-cloud"),
    ("every punctuation edge", "!~a.b_c-d+e=f"),
];

const REFUSED_IDS: &[(&str, &str)] = &[
    ("zero-width space", "glm\u{200B}-5"),
    ("bidi override", "glm-5\u{202E}"),
    ("c1 control", "glm\u{0085}5"),
    ("space", "glm 5"),
    ("tab", "glm\t5"),
    ("delete", "glm\u{7F}5"),
    ("latin letter with accent", "caf\u{E9}"),
];

#[test]
fn provider_ids_outside_printable_ascii_are_refused() {
    for (name, id) in ACCEPTED_IDS {
        let doc = table(json!([{ "id": id, "models": [priced_model("m")] }]));
        assert!(parse_and_validate(&bytes(&doc)).is_ok(), "{name}");
    }
    for (name, id) in REFUSED_IDS {
        let doc = table(json!([{ "id": id, "models": [priced_model("m")] }]));
        assert_eq!(
            parse_and_validate(&bytes(&doc)).err(),
            Some(PriceTableError::ProviderIdCharset),
            "{name}"
        );
    }
}

#[test]
fn model_ids_outside_printable_ascii_are_refused() {
    for (name, id) in ACCEPTED_IDS {
        let doc = one_provider(json!([priced_model(id)]));
        assert!(parse_and_validate(&bytes(&doc)).is_ok(), "{name}");
    }
    for (name, id) in REFUSED_IDS {
        let doc = one_provider(json!([priced_model("anchor"), priced_model(id)]));
        assert_eq!(
            parse_and_validate(&bytes(&doc)).err(),
            Some(PriceTableError::ModelIdCharset),
            "{name}"
        );
    }
}

#[test]
fn models_dev_ids_obey_the_same_byte_set() {
    let cost = json!({ "cost": { "input": 1 } });
    for (name, id) in ACCEPTED_IDS {
        let source = json!({ *id: { "models": { *id: cost.clone() } } });
        let bytes = serde_json::to_vec(&source).expect("json");
        assert!(from_models_dev(&bytes, date()).is_ok(), "{name}");
    }
    for (name, id) in REFUSED_IDS {
        let provider = json!({ *id: { "models": { "m": cost.clone() } } });
        let model = json!({ "p": { "models": { "m": cost.clone(), *id: cost.clone() } } });
        let refused = |source: &Value| {
            from_models_dev(&serde_json::to_vec(source).expect("json"), date()).err()
        };
        assert_eq!(
            refused(&provider),
            Some(PriceTableError::ProviderIdCharset),
            "provider {name}"
        );
        assert_eq!(
            refused(&model),
            Some(PriceTableError::ModelIdCharset),
            "model {name}"
        );
    }
}

#[test]
fn charset_errors_carry_no_input_text() {
    for err in [
        PriceTableError::ProviderIdCharset,
        PriceTableError::ModelIdCharset,
    ] {
        let text = format!("{err} {err:?}");
        assert!(text.is_ascii() && !text.contains("glm"), "{text}");
    }
}

/// A canonical body of `rows` priced models, optionally followed by one row
/// whose price is a string: well-formed JSON the schema probe skips, but a
/// type error once the rows are deserialized.
fn canonical_rows(rows: usize, bad_tail: bool) -> Vec<u8> {
    let mut out = String::from(
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","models":["#,
    );
    for i in 0..rows {
        if i > 0 {
            out.push(',');
        }
        write!(out, r#"{{"id":"m{i}","input":1}}"#).expect("write");
    }
    if bad_tail {
        out.push_str(r#",{"id":"tail","input":"x"}"#);
    }
    out.push_str("]}]}");
    out.into_bytes()
}

/// A models.dev body of `rows` cost-less models split over two providers,
/// optionally cut short so the document never closes.
fn models_dev_costless(rows: usize, truncated: bool) -> Vec<u8> {
    let half = rows / 2;
    let mut out = String::from(r#"{"a":{"models":{"priced":{"cost":{"input":1}}"#);
    for i in 1..half {
        write!(out, r#","a{i}":{{"name":"x"}}"#).expect("write");
    }
    out.push_str(r#"}},"b":{"models":{"#);
    for i in half..rows {
        if i > half {
            out.push(',');
        }
        write!(out, r#""b{i}":{{"name":"x"}}"#).expect("write");
    }
    if !truncated {
        out.push_str("}}}");
    }
    out.into_bytes()
}

#[test]
fn canonical_rows_are_bounded_while_parsing() {
    let too_many = Some(PriceTableError::TooManyRows { limit: MAX_ROWS });
    assert!(
        parse_and_validate(&canonical_rows(MAX_ROWS, false)).is_ok(),
        "at limit"
    );
    assert_eq!(
        parse_and_validate(&canonical_rows(MAX_ROWS + 1, false)).err(),
        too_many,
        "one over"
    );
    assert_eq!(
        parse_and_validate(&canonical_rows(MAX_ROWS + 1, true)).err(),
        too_many,
        "refused at the row that crosses the bound, before a later row is read"
    );
    assert!(
        matches!(
            parse_and_validate(&canonical_rows(MAX_ROWS, true)),
            Err(PriceTableError::Syntax { .. })
        ),
        "control: under the bound the bad tail row is reached"
    );
}

#[test]
fn models_dev_entries_count_toward_the_bound_with_or_without_a_cost() {
    let too_many = Some(PriceTableError::TooManyRows { limit: MAX_ROWS });
    assert!(
        from_models_dev(&models_dev_costless(MAX_ROWS, false), date()).is_ok(),
        "at limit"
    );
    assert_eq!(
        from_models_dev(&models_dev_costless(MAX_ROWS + 1, false), date()).err(),
        too_many,
        "one over, summed across providers"
    );
    assert_eq!(
        from_models_dev(&models_dev_costless(MAX_ROWS + 1, true), date()).err(),
        too_many,
        "refused before the document closes"
    );
}

#[test]
fn provider_count_is_bounded_on_both_formats() {
    let mut canonical = vec![json!({ "id": "anchor", "models": [priced_model("m")] })];
    canonical.extend((0..MAX_ROWS).map(|i| json!({ "id": format!("p{i}"), "models": [] })));
    let mut dev = serde_json::Map::new();
    dev.insert(
        "anchor".into(),
        json!({ "models": { "m": { "cost": { "input": 1 } } } }),
    );
    for i in 0..MAX_ROWS {
        dev.insert(format!("p{i}"), json!({ "models": {} }));
    }
    let over = Some(PriceTableError::TooManyProviders { limit: MAX_ROWS });

    assert_eq!(
        parse_and_validate(&bytes(&table(Value::Array(canonical)))).err(),
        over,
        "canonical"
    );
    assert_eq!(
        from_models_dev(&bytes(&Value::Object(dev)), date()).err(),
        over,
        "models.dev"
    );
    let mut at_limit = vec![json!({ "id": "anchor", "models": [priced_model("m")] })];
    at_limit.extend((1..MAX_ROWS).map(|i| json!({ "id": format!("p{i}"), "models": [] })));
    assert!(
        parse_and_validate(&bytes(&table(Value::Array(at_limit)))).is_ok(),
        "at limit"
    );
}

#[test]
fn a_structured_schema_version_is_malformed() {
    let rows: [(&str, Value); 3] = [
        ("object", json!({ "major": 1 })),
        ("array", json!([1])),
        ("nested object", json!({ "v": [1, { "x": 1 }] })),
    ];
    for (name, version) in rows {
        let mut doc = one_provider(json!([priced_model("m")]));
        doc["schema_version"] = version;
        assert!(
            matches!(
                parse_and_validate(&bytes(&doc)),
                Err(PriceTableError::Syntax { .. })
            ),
            "{name}"
        );
    }
    let mut negative = one_provider(json!([priced_model("m")]));
    negative["schema_version"] = json!(-1);
    assert_eq!(
        parse_and_validate(&bytes(&negative)).err(),
        Some(PriceTableError::UnsupportedSchemaVersion),
        "a negative scalar is unsupported"
    );
}

#[test]
fn a_field_repeated_in_one_object_is_malformed() {
    let rows = [
        (
            "models twice",
            r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","models":[{"id":"m","input":1}],"models":[]}]}"#,
        ),
        (
            "providers twice",
            r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[],"providers":[{"id":"p","models":[{"id":"m","input":1}]}]}"#,
        ),
        (
            "models.dev models twice",
            r#"{"p":{"models":{"m":{"cost":{"input":1}}},"models":{}}}"#,
        ),
    ];
    for (name, text) in rows {
        let result = if name.starts_with("models.dev") {
            from_models_dev(text.as_bytes(), date())
        } else {
            parse_and_validate(text.as_bytes())
        };
        assert!(
            matches!(result, Err(PriceTableError::Syntax { .. })),
            "{name}"
        );
    }
}
