//! The canonical parser: schema version, every bound, price admission, the
//! date and what a table renders.

use serde_json::{Value, json};

use super::prices_test_support::{bytes, one_provider, priced_model, table};
use super::validate::{MAX_MODEL_ID_BYTES, MAX_PROVIDER_ID_BYTES, MAX_ROWS};
use super::*;

fn parse(value: &Value) -> Result<PriceTable, PriceTableError> {
    parse_and_validate(&bytes(value))
}

fn acme_prices(table: &PriceTable, model: &str) -> Option<PricingConfig> {
    table
        .provider("acme")
        .and_then(|p| p.models.get(model).cloned())
}

#[test]
fn a_well_formed_table_parses_and_keeps_its_prices() {
    let parsed = parse(&one_provider(json!([{
        "id": "m", "input": 1.5, "output": 3.0, "cache_read": 0.15,
        "cache_write_5m": 1.875, "cache_write_1h": 3.0
    }])))
    .expect("valid table");

    assert_eq!(parsed.generated_at().to_string(), "2026-11-01");
    assert_eq!(
        acme_prices(&parsed, "m"),
        Some(PricingConfig {
            input_per_mtok: Some(1.5),
            output_per_mtok: Some(3.0),
            cache_read_per_mtok: Some(0.15),
            cache_write_5m_per_mtok: Some(1.875),
            cache_write_1h_per_mtok: Some(3.0),
            cache_write_per_mtok: None,
        })
    );
}

#[test]
fn schema_version_other_than_one_fails_closed() {
    let rows: [(&str, Value); 5] = [
        ("two", json!(2)),
        ("zero", json!(0)),
        ("string", json!("1")),
        ("float", json!(1.5)),
        ("null", Value::Null),
    ];
    for (name, version) in rows {
        let mut doc = one_provider(json!([priced_model("m")]));
        doc["schema_version"] = version;
        assert_eq!(
            parse(&doc).err(),
            Some(PriceTableError::UnsupportedSchemaVersion),
            "{name}"
        );
    }
    let mut missing = one_provider(json!([priced_model("m")]));
    missing
        .as_object_mut()
        .expect("object")
        .remove("schema_version");
    assert_eq!(
        parse(&missing).err(),
        Some(PriceTableError::UnsupportedSchemaVersion),
        "missing"
    );
}

#[test]
fn unknown_fields_are_tolerated_at_every_level() {
    let doc = json!({
        "schema_version": 1,
        "generated_at": "2026-11-01",
        "future_top": {"x": 1},
        "providers": [{
            "id": "acme",
            "future_provider": [1, 2],
            "models": [{ "id": "m", "input": 1.0, "future_model": "y" }]
        }]
    });

    let parsed = parse(&doc).expect("unknown fields tolerated");

    assert_eq!(
        acme_prices(&parsed, "m").and_then(|p| p.input_per_mtok),
        Some(1.0)
    );
}

#[test]
fn input_over_the_byte_bound_is_refused_before_parsing() {
    let at_limit = vec![b' '; MAX_INPUT_BYTES];
    let over_limit = vec![b' '; MAX_INPUT_BYTES + 1];

    assert!(matches!(
        parse_and_validate(&at_limit),
        Err(PriceTableError::Syntax { .. })
    ));
    assert_eq!(
        parse_and_validate(&over_limit).err(),
        Some(PriceTableError::TooLarge)
    );
    assert_eq!(
        from_models_dev(&over_limit, TableDate::parse("2026-11-01").expect("date")).err(),
        Some(PriceTableError::TooLarge)
    );
}

fn models(count: usize) -> Value {
    Value::Array((0..count).map(|i| priced_model(&format!("m{i}"))).collect())
}

#[test]
fn row_count_bound_admits_the_limit_and_refuses_one_more() {
    assert!(parse(&one_provider(models(MAX_ROWS))).is_ok(), "at limit");
    let split = table(json!([
        { "id": "a", "models": models(MAX_ROWS / 2) },
        { "id": "b", "models": models(MAX_ROWS / 2 + 1) },
    ]));
    assert_eq!(
        parse(&split).err(),
        Some(PriceTableError::TooManyRows { limit: MAX_ROWS }),
        "summed across providers"
    );
}

#[test]
fn an_empty_or_overlong_id_skips_its_rows_and_keeps_the_table() {
    let provider_rows = [
        ("provider at limit", "p".repeat(MAX_PROVIDER_ID_BYTES), 0),
        (
            "provider over limit",
            "p".repeat(MAX_PROVIDER_ID_BYTES + 1),
            1,
        ),
        ("provider empty", String::new(), 1),
    ];
    for (name, id, skipped) in provider_rows {
        let doc = table(json!([
            { "id": "acme", "models": [priced_model("kept")] },
            { "id": id, "models": [priced_model("m")] },
        ]));
        let t = parse(&doc).expect(name);
        assert_eq!(t.skipped_row_count(), skipped, "{name}");
        assert_eq!(t.provider_count(), 2 - skipped, "{name}");
        assert!(
            acme_prices(&t, "kept").is_some(),
            "{name}: the table is kept"
        );
    }
    let model_rows = [
        ("model at limit", "m".repeat(MAX_MODEL_ID_BYTES), 0),
        ("model over limit", "m".repeat(MAX_MODEL_ID_BYTES + 1), 1),
        ("model empty", String::new(), 1),
    ];
    for (name, id, skipped) in model_rows {
        let t = parse(&one_provider(json!([
            priced_model("kept"),
            priced_model(&id)
        ])))
        .expect(name);
        assert_eq!(t.skipped_row_count(), skipped, "{name}");
        assert_eq!(
            acme_prices(&t, &id).is_some(),
            skipped == 0,
            "{name}: a skipped row stays unknown"
        );
        assert!(
            acme_prices(&t, "kept").is_some(),
            "{name}: the table is kept"
        );
    }
}

#[test]
fn duplicate_rows_are_refused() {
    let rows = [
        (
            "same model twice in one provider",
            one_provider(json!([priced_model("m"), priced_model("m")])),
            PriceTableError::DuplicateModel,
        ),
        (
            "same provider twice",
            table(json!([
                { "id": "acme", "models": [priced_model("m")] },
                { "id": "acme", "models": [priced_model("n")] },
            ])),
            PriceTableError::DuplicateProvider,
        ),
        (
            "duplicate unusable provider id",
            table(json!([
                { "id": "acme", "models": [priced_model("m")] },
                { "id": "", "models": [priced_model("m")] },
                { "id": "", "models": [priced_model("n")] },
            ])),
            PriceTableError::DuplicateProvider,
        ),
        (
            "duplicate unusable model id",
            one_provider(json!([
                priced_model("kept"),
                priced_model(""),
                priced_model("")
            ])),
            PriceTableError::DuplicateModel,
        ),
        (
            "duplicate model under an unusable provider",
            table(json!([
                { "id": "acme", "models": [priced_model("m")] },
                { "id": "", "models": [priced_model("n"), priced_model("n")] },
            ])),
            PriceTableError::DuplicateModel,
        ),
    ];
    for (name, doc, expected) in rows {
        assert_eq!(parse(&doc).err(), Some(expected), "{name}");
    }
    let same_model_two_providers = table(json!([
        { "id": "a", "models": [priced_model("m")] },
        { "id": "b", "models": [priced_model("m")] },
    ]));
    assert!(
        parse(&same_model_two_providers).is_ok(),
        "one model id under two providers is two rows"
    );
}

#[test]
fn a_table_with_no_priced_model_is_malformed() {
    let rows = [
        ("no providers", table(json!([]))),
        ("provider with no models", one_provider(json!([]))),
        (
            "models with no prices",
            one_provider(json!([{ "id": "m" }])),
        ),
        (
            "every price inadmissible",
            one_provider(json!([{ "id": "m", "input": -1.0, "output": 20000.0 }])),
        ),
    ];
    for (name, doc) in rows {
        assert_eq!(
            parse(&doc).err(),
            Some(PriceTableError::NoPricedModels),
            "{name}"
        );
    }
}

#[test]
fn a_single_known_zero_price_makes_a_table_priced() {
    let parsed = parse(&one_provider(json!([{ "id": "m", "cache_read": 0.0 }])))
        .expect("a known zero is a price");
    let prices = acme_prices(&parsed, "m").expect("row");
    assert_eq!(prices.cache_read_per_mtok, Some(0.0));
    assert_eq!(prices.input_per_mtok, None);
}

#[test]
fn an_inadmissible_price_drops_to_unknown_and_a_known_zero_is_kept() {
    let rows: [(&str, Value, Option<f64>); 7] = [
        ("negative", json!(-0.01), None),
        ("over the ceiling", json!(10_000.01), None),
        ("huge", json!(1e300), None),
        ("ceiling itself", json!(10_000.0), Some(10_000.0)),
        ("zero", json!(0.0), Some(0.0)),
        ("integer zero", json!(0), Some(0.0)),
        ("ordinary", json!(3.75), Some(3.75)),
    ];
    for (name, price, expected) in rows {
        let doc = one_provider(json!([
            priced_model("anchor"),
            { "id": "m", "input": price.clone(), "output": price.clone(),
              "cache_read": price.clone(), "cache_write_5m": price.clone(),
              "cache_write_1h": price },
        ]));
        let prices = acme_prices(&parse(&doc).expect(name), "m").expect("row kept");
        for (field, value) in [
            ("input", prices.input_per_mtok),
            ("output", prices.output_per_mtok),
            ("cache_read", prices.cache_read_per_mtok),
            ("cache_write_5m", prices.cache_write_5m_per_mtok),
            ("cache_write_1h", prices.cache_write_1h_per_mtok),
        ] {
            assert_eq!(value, expected, "{name}: {field}");
        }
    }
}

#[test]
fn non_finite_prices_never_admit() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(validate::admit_price(value), None, "{value}");
    }
    let overflow = br#"{"schema_version":1,"generated_at":"2026-11-01",
        "providers":[{"id":"acme","models":[{"id":"m","input":1e400}]}]}"#;
    assert!(
        matches!(
            parse_and_validate(overflow),
            Err(PriceTableError::Syntax { .. })
        ),
        "an out-of-range literal fails closed"
    );
}

#[test]
fn generated_at_parses_only_as_a_calendar_date() {
    let rows = [
        ("plain", "2026-11-01", true),
        ("leap day", "2024-02-29", true),
        ("timestamp", "2026-11-01T00:00:00Z", false),
        ("non leap day", "2026-02-29", false),
        ("century non leap", "1900-02-29", false),
        ("month 13", "2026-13-01", false),
        ("day 31 in a 30-day month", "2026-04-31", false),
        ("day zero", "2026-11-00", false),
        ("year zero", "0000-11-01", false),
        ("short", "2026-1-01", false),
        ("signed", "+026-11-01", false),
        ("slashes", "2026/11/01", false),
        ("non-ascii digits", "٢٠-11-01", false),
        ("free text", "yesterday", false),
        ("empty", "", false),
    ];
    for (name, date, ok) in rows {
        let mut doc = one_provider(json!([priced_model("m")]));
        doc["generated_at"] = json!(date);
        let result = parse(&doc);
        if ok {
            assert_eq!(
                result.expect(name).generated_at().to_string(),
                date,
                "{name}"
            );
        } else {
            assert_eq!(result.err(), Some(PriceTableError::InvalidDate), "{name}");
        }
    }
}

/// An `api_host` the shared origin normalizer withholds leaves the provider
/// in the table with no origin, so it is reachable only through the
/// first-party fallback; the table is kept. The control is the same provider
/// with a usable host, selected by origin.
#[test]
fn an_api_host_the_origin_normalizer_withholds_drops_only_the_origin() {
    let rows = [
        ("whole-url placeholder", "${GATEWAY_BASE_URL}/v1"),
        (
            "at sign past the authority",
            "https://key/x@host.example/v1",
        ),
        ("non-http scheme", "ftp://files.example/v1"),
        ("double userinfo", "https://a@b@host.example/v1"),
        ("not a url", "opencode.ai/zen"),
    ];
    let lookup = |host: &str| {
        let doc = table(json!([
            { "id": "anthropic", "api_host": host, "models": [priced_model("m")] },
        ]));
        let t = parse(&doc).expect("the table is kept");
        (
            t.price_for_target("openai-compat", Some("https://host.example/v1"), "m")
                .is_some(),
            t.price_for_target("anthropic-api", None, "m").is_some(),
        )
    };
    assert_eq!(
        lookup("https://host.example/v1"),
        (true, true),
        "control: a usable host matches by origin"
    );
    for (name, host) in rows {
        assert_eq!(lookup(host), (false, true), "{name}");
    }
}

const PLANTED: &str = "planted-download-string";

#[test]
fn no_download_string_renders_through_debug_display_or_errors() {
    let host = format!("https://{PLANTED}.example/{PLANTED}");
    let parsed = parse(&table(json!([{
        "id": PLANTED,
        "api_host": host,
        "models": [priced_model(PLANTED)],
    }])))
    .expect("valid table");
    assert!(
        String::from_utf8(parsed.to_canonical_json().expect("serializes"))
            .expect("utf8")
            .contains(PLANTED),
        "positive control: the fixture carries the planted string"
    );

    let rendered = format!("{parsed:?} {}", parsed.generated_at());

    assert!(!rendered.contains(PLANTED), "{rendered}");
    let failures = [
        one_provider(json!([priced_model(PLANTED), priced_model(PLANTED)])),
        table(json!([{ "id": PLANTED, "api_host": format!("ftp://{PLANTED}"), "models": [] }])),
        one_provider(json!([priced_model(&format!("{PLANTED}\u{7F}"))])),
        json!({ "schema_version": 1, "generated_at": PLANTED, "providers": [] }),
        json!({ "schema_version": 1, "generated_at": "2026-11-01", "providers": PLANTED }),
    ];
    for doc in failures {
        let err = parse(&doc).expect_err("refused");
        let text = format!("{err} {err:?}");
        assert!(!text.contains(PLANTED), "{text}");
    }
}

#[test]
fn canonical_json_round_trips_sorted_and_normalized() {
    let parsed = parse(&table(json!([
        { "id": "zeta", "api_host": "https://API.Zeta.example/v1/", "models": [
            priced_model("b"), priced_model("a")
        ]},
        { "id": "alpha", "models": [{ "id": "m", "cache_read": 0.0 }] },
    ])))
    .expect("valid");

    let written = parsed.to_canonical_json().expect("serializes");
    let reparsed = parse_and_validate(&written).expect("canonical output reparses");
    let doc: Value = serde_json::from_slice(&written).expect("json");

    assert_eq!(reparsed, parsed);
    assert_eq!(doc["providers"][0]["id"], "alpha");
    assert_eq!(
        doc["providers"][1]["api_host"],
        "https://api.zeta.example/v1"
    );
    assert_eq!(doc["providers"][1]["models"][0]["id"], "a");
    assert_eq!(doc["providers"][0]["models"][0]["cache_read"], 0.0);
    assert!(doc["providers"][0]["models"][0].get("input").is_none());
    assert!(written.ends_with(b"}\n"));
}

#[test]
fn canonical_api_host_drops_only_the_scheme_default_port() {
    let rows: [(&str, &str, &str); 5] = [
        (
            "https explicit 443",
            "https://h.example:443/v1",
            "https://h.example/v1",
        ),
        (
            "http explicit 80",
            "http://h.example:80/v1",
            "http://h.example/v1",
        ),
        (
            "https on 80 kept",
            "https://h.example:80/v1",
            "https://h.example:80/v1",
        ),
        (
            "http on 443 kept",
            "http://h.example:443/v1",
            "http://h.example:443/v1",
        ),
        (
            "other port kept",
            "http://127.0.0.1:1234/v1",
            "http://127.0.0.1:1234/v1",
        ),
    ];
    for (name, host, expected) in rows {
        let parsed = parse(&table(json!([
            { "id": "acme", "api_host": host, "models": [priced_model("m")] },
        ])))
        .expect(name);

        let written = parsed.to_canonical_json().expect("serializes");
        let doc: Value = serde_json::from_slice(&written).expect("json");

        assert_eq!(doc["providers"][0]["api_host"], expected, "{name}");
        assert_eq!(
            parse_and_validate(&written).expect("reparses"),
            parsed,
            "{name}: the written form selects the same provider"
        );
    }
}
