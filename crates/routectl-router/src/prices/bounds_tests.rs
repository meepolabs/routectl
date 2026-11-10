//! Bounds enforced while a table is parsed: the id byte set (a row skipped,
//! the table kept), the row and provider counts on both input formats, and
//! the schema version probe.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::prices_test_support::{bytes, one_provider, priced_model, table};
use super::validate::{MAX_PROVIDERS, MAX_ROWS};
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

/// A table holding `anchor` (a priced provider every row keeps) plus
/// `extra`, so a skipped row leaves a table that still parses.
fn with_anchor(extra: Value) -> Value {
    table(json!([
        { "id": "anchor", "models": [priced_model("kept")] },
        extra,
    ]))
}

/// Whether the anchor provider's row survived validation.
fn kept(t: &PriceTable) -> bool {
    t.provider("anchor")
        .is_some_and(|p| p.models.contains_key("kept"))
}

#[test]
fn a_provider_id_outside_printable_ascii_skips_its_rows_and_keeps_the_table() {
    for (name, id) in ACCEPTED_IDS {
        let doc = with_anchor(json!({ "id": id, "models": [priced_model("m")] }));
        let t = parse_and_validate(&bytes(&doc)).expect(name);
        assert_eq!(
            (t.provider_count(), t.skipped_row_count()),
            (2, 0),
            "{name}"
        );
    }
    for (name, id) in REFUSED_IDS {
        let doc =
            with_anchor(json!({ "id": id, "models": [priced_model("m"), priced_model("n")] }));
        let t = parse_and_validate(&bytes(&doc)).expect(name);
        assert!(kept(&t), "{name}: the anchor row stays");
        assert_eq!(t.provider_count(), 1, "{name}: the provider is left out");
        assert_eq!(t.skipped_row_count(), 2, "{name}: both its rows count");
    }
}

#[test]
fn a_model_id_outside_printable_ascii_skips_the_row_and_keeps_the_table() {
    for (name, id) in ACCEPTED_IDS {
        let doc = one_provider(json!([priced_model(id)]));
        let t = parse_and_validate(&bytes(&doc)).expect(name);
        assert_eq!((t.model_count(), t.skipped_row_count()), (1, 0), "{name}");
    }
    for (name, id) in REFUSED_IDS {
        let doc = one_provider(json!([priced_model("anchor"), priced_model(id)]));
        let t = parse_and_validate(&bytes(&doc)).expect(name);
        assert_eq!(t.model_count(), 1, "{name}: only the anchor row stays");
        assert_eq!(t.skipped_row_count(), 1, "{name}");
        assert!(
            !String::from_utf8(t.to_canonical_json().expect("serializes"))
                .expect("utf8")
                .contains(id),
            "{name}: the skipped id is not written back"
        );
    }
}

#[test]
fn a_table_whose_only_priced_row_is_skipped_has_no_priced_model() {
    for (name, id) in REFUSED_IDS {
        let doc = one_provider(json!([priced_model(id)]));
        assert_eq!(
            parse_and_validate(&bytes(&doc)).err(),
            Some(PriceTableError::NoPricedModels),
            "{name}"
        );
    }
}

#[test]
fn models_dev_ids_obey_the_same_byte_set() {
    let cost = json!({ "cost": { "input": 1 } });
    for (name, id) in ACCEPTED_IDS {
        let source = json!({ *id: { "models": { *id: cost.clone() } } });
        let t = from_models_dev(&serde_json::to_vec(&source).expect("json"), date()).expect(name);
        assert_eq!(t.skipped_row_count(), 0, "{name}");
    }
    for (name, id) in REFUSED_IDS {
        let provider = json!({
            "anchor": { "models": { "kept": cost.clone() } },
            *id: { "models": { "m": cost.clone() } },
        });
        let model = json!({ "anchor": { "models": { "kept": cost.clone(), *id: cost.clone() } } });
        let convert = |source: &Value| {
            from_models_dev(&serde_json::to_vec(source).expect("json"), date()).expect(name)
        };
        for (what, source) in [("provider", &provider), ("model", &model)] {
            let t = convert(source);
            assert!(kept(&t), "{what} {name}: the anchor row stays");
            assert_eq!(t.model_count(), 1, "{what} {name}");
            assert_eq!(t.skipped_row_count(), 1, "{what} {name}");
        }
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
    canonical.extend((0..MAX_PROVIDERS).map(|i| json!({ "id": format!("p{i}"), "models": [] })));
    let mut dev = serde_json::Map::new();
    dev.insert(
        "anchor".into(),
        json!({ "models": { "m": { "cost": { "input": 1 } } } }),
    );
    for i in 0..MAX_PROVIDERS {
        dev.insert(format!("p{i}"), json!({ "models": {} }));
    }
    let over = Some(PriceTableError::TooManyProviders {
        limit: MAX_PROVIDERS,
    });

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
    at_limit.extend((1..MAX_PROVIDERS).map(|i| json!({ "id": format!("p{i}"), "models": [] })));
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

/// Which parser a shape row goes through.
#[derive(Clone, Copy)]
enum Source {
    Canonical,
    ModelsDev,
}

const SHAPE_ROWS: &[(&str, Source, &str)] = &[
    (
        "missing provider id",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "missing models on a second provider",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","models":[{"id":"m","input":1}]},{"id":"q"}]}"#,
    ),
    (
        "missing providers",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01"}"#,
    ),
    (
        "missing generated_at",
        Source::Canonical,
        r#"{"schema_version":1,"providers":[{"id":"p","models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "provider id twice",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","id":"q","models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "api_host twice",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","api_host":"https://a.example","api_host":"https://b.example","models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "generated_at twice",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","generated_at":"2026-11-02","providers":[{"id":"p","models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "models twice",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","models":[{"id":"m","input":1}],"models":[]}]}"#,
    ),
    (
        "providers twice",
        Source::Canonical,
        r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[],"providers":[{"id":"p","models":[{"id":"m","input":1}]}]}"#,
    ),
    (
        "models.dev api twice",
        Source::ModelsDev,
        r#"{"p":{"api":"https://a.example","api":"https://b.example","models":{"m":{"cost":{"input":1}}}}}"#,
    ),
    (
        "models.dev models twice",
        Source::ModelsDev,
        r#"{"p":{"models":{"m":{"cost":{"input":1}}},"models":{}}}"#,
    ),
    (
        "models.dev trailing data",
        Source::ModelsDev,
        r#"{"p":{"models":{"m":{"cost":{"input":1}}}}} {}"#,
    ),
];

fn parse_row(source: Source, text: &str) -> Result<PriceTable, PriceTableError> {
    match source {
        Source::Canonical => parse_and_validate(text.as_bytes()),
        Source::ModelsDev => from_models_dev(text.as_bytes(), date()),
    }
}

#[test]
fn a_missing_required_or_repeated_field_or_trailing_data_is_malformed() {
    for (name, source, text) in SHAPE_ROWS {
        assert!(
            matches!(
                parse_row(*source, text),
                Err(PriceTableError::Syntax { .. })
            ),
            "{name}"
        );
    }
}

#[test]
fn shape_rows_parse_once_their_defect_is_removed() {
    let controls: &[(&str, Source, &str)] = &[
        (
            "canonical, every field once",
            Source::Canonical,
            r#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"p","api_host":"https://a.example","models":[{"id":"m","input":1}]},{"id":"q","models":[]}]}"#,
        ),
        (
            "models.dev, every field once",
            Source::ModelsDev,
            r#"{"p":{"api":"https://a.example","models":{"m":{"cost":{"input":1}}}}} "#,
        ),
    ];
    for (name, source, text) in controls {
        assert!(parse_row(*source, text).is_ok(), "{name}");
    }
}
