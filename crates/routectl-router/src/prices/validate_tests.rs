//! The canonical parser: schema version, every bound, price admission, the
//! date and what a table renders.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::prices_test_support::{bytes, one_provider, priced_model, table};
use super::validate::{
    MAX_CACHE_WRITE_LIFETIMES, MAX_MODEL_ID_BYTES, MAX_PROVIDER_ID_BYTES, MAX_ROWS,
};
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

/// One `acme` row `m` beside a priced anchor, so a row with no admitted
/// price never empties the table.
fn row_table(row: Value) -> Value {
    one_provider(json!([priced_model("anchor"), row]))
}

type Rates = Result<BTreeMap<u32, f64>, PriceTableError>;

fn cache_write_rates(doc: &Value) -> Rates {
    let table = parse(doc)?;
    Ok(acme_prices(&table, "m")
        .expect("row kept")
        .cache_write_rates())
}

#[test]
fn a_cache_write_object_prices_its_lifetimes_beside_the_legacy_keys() {
    let rows: [(&str, Value, Rates); 7] = [
        (
            "a 1800 second lifetime alone",
            json!({ "id": "m", "cache_write": { "1800": 2.0 } }),
            Ok(BTreeMap::from([(1800, 2.0)])),
        ),
        (
            "a 300 second lifetime in the object alone",
            json!({ "id": "m", "cache_write": { "300": 1.25 } }),
            Ok(BTreeMap::from([(300, 1.25)])),
        ),
        (
            "legacy keys and the object agree",
            json!({ "id": "m", "cache_write_5m": 1.25, "cache_write_1h": 2.0,
                    "cache_write": { "300": 1.25, "3600": 2.0, "1800": 1.5 } }),
            Ok(BTreeMap::from([(300, 1.25), (1800, 1.5), (3600, 2.0)])),
        ),
        (
            "the 5-minute legacy key disagrees with the object",
            json!({ "id": "m", "cache_write_5m": 1.25, "cache_write": { "300": 1.5 } }),
            Err(PriceTableError::CacheWriteAliasConflict),
        ),
        (
            "the 1-hour legacy key disagrees with the object",
            json!({ "id": "m", "cache_write_1h": 2.0, "cache_write": { "3600": 2.5 } }),
            Err(PriceTableError::CacheWriteAliasConflict),
        ),
        (
            "an out-of-range object price is unknown",
            json!({ "id": "m", "input": 1.0,
                    "cache_write": { "1800": -1.0, "7200": 20000.0, "60": 0.0 } }),
            Ok(BTreeMap::from([(60, 0.0)])),
        ),
        (
            "an empty object prices nothing",
            json!({ "id": "m", "input": 1.0, "cache_write": {} }),
            Ok(BTreeMap::new()),
        ),
    ];
    for (name, row, expected) in rows {
        assert_eq!(cache_write_rates(&row_table(row)), expected, "{name}");
    }
}

/// The 300 and 3600 second lifetimes land in their legacy fields whichever
/// spelling priced them, so a table compares equal across spellings.
#[test]
fn an_object_price_for_a_legacy_lifetime_fills_the_legacy_field() {
    let parsed = parse(&row_table(
        json!({ "id": "m", "cache_write": { "300": 1.25, "3600": 2.0 } }),
    ))
    .expect("valid");

    let prices = acme_prices(&parsed, "m").expect("row");

    assert_eq!(prices.cache_write_5m_per_mtok, Some(1.25));
    assert_eq!(prices.cache_write_1h_per_mtok, Some(2.0));
    assert_eq!(prices.cache_write_per_mtok, None);
}

#[test]
fn a_cache_write_key_that_is_not_canonical_decimal_seconds_is_unknown() {
    let rows = [
        ("zero", "0"),
        ("negative", "-5"),
        ("fraction", "1.5"),
        ("duration grammar", "5m"),
        ("above u32", "4294967296"),
        ("leading zero", "0300"),
        ("plus sign", "+30"),
        ("empty", ""),
        ("padded", " 30"),
    ];
    let control = cache_write_rates(&row_table(
        json!({ "id": "m", "input": 1.0, "cache_write": { "4294967295": 3.0 } }),
    ));
    assert_eq!(
        control,
        Ok(BTreeMap::from([(u32::MAX, 3.0)])),
        "control: the largest u32 key prices its lifetime"
    );
    for (name, key) in rows {
        let doc = row_table(json!({ "id": "m", "input": 1.0, "cache_write": { key: 3.0 } }));
        assert_eq!(cache_write_rates(&doc), Ok(BTreeMap::new()), "{name}");
    }
}

fn cache_write_entries(count: usize, key: impl Fn(usize) -> String) -> Value {
    Value::Object((1..=count).map(|i| (key(i), json!(1.0))).collect())
}

#[test]
fn a_cache_write_object_over_the_entry_cap_refuses_the_table() {
    let at_cap = cache_write_entries(MAX_CACHE_WRITE_LIFETIMES, |i| i.to_string());
    let rates = cache_write_rates(&row_table(json!({ "id": "m", "cache_write": at_cap })))
        .expect("at the cap");
    assert_eq!(rates.len(), MAX_CACHE_WRITE_LIFETIMES);

    let rows = [
        (
            "valid keys",
            cache_write_entries(MAX_CACHE_WRITE_LIFETIMES + 1, |i| i.to_string()),
        ),
        (
            "keys that read as unknown",
            cache_write_entries(MAX_CACHE_WRITE_LIFETIMES + 1, |i| format!("{i}m")),
        ),
        (
            "far over the cap",
            cache_write_entries(MAX_CACHE_WRITE_LIFETIMES * 64, |i| i.to_string()),
        ),
    ];
    for (name, entries) in rows {
        let doc = row_table(json!({ "id": "m", "input": 1.0, "cache_write": entries }));
        assert_eq!(
            parse(&doc).err(),
            Some(PriceTableError::TooManyCacheWriteLifetimes {
                limit: MAX_CACHE_WRITE_LIFETIMES
            }),
            "{name}"
        );
    }
}

#[test]
fn a_repeated_cache_write_key_fails_closed() {
    let doc = br#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"acme",
        "models":[{"id":"m","cache_write":{"1800":1.0,"1800":2.0}}]}]}"#;

    assert!(matches!(
        parse_and_validate(doc),
        Err(PriceTableError::Syntax { .. })
    ));
}

#[test]
fn canonical_json_writes_legacy_keys_and_only_other_lifetimes_in_the_object() {
    let parsed = parse(&row_table(json!({
        "id": "m", "input": 1.0,
        "cache_write": { "300": 1.25, "3600": 2.0, "1800": 1.5 },
    })))
    .expect("valid");

    let written = parsed.to_canonical_json().expect("serializes");
    let doc: Value = serde_json::from_slice(&written).expect("json");
    let row = &doc["providers"][0]["models"][1];

    assert_eq!(row["id"], "m");
    assert_eq!(row["cache_write_5m"], 1.25);
    assert_eq!(row["cache_write_1h"], 2.0);
    assert_eq!(row["cache_write"], json!({ "1800": 1.5 }));
    assert!(
        doc["providers"][0]["models"][0]
            .get("cache_write")
            .is_none(),
        "a row with no other lifetime writes no object"
    );
    assert_eq!(parse_and_validate(&written).expect("reparses"), parsed);
}

/// The table as the base release reads it: derived `Deserialize` on the
/// model row, unknown fields ignored at every level. Kept frozen so a newer
/// writer's output is checked against what an installed older binary parses.
#[derive(serde::Deserialize)]
struct FrozenTable {
    schema_version: u64,
    providers: Vec<FrozenProvider>,
}

#[derive(serde::Deserialize)]
struct FrozenProvider {
    models: Vec<FrozenRawModel>,
}

#[derive(serde::Deserialize)]
struct FrozenRawModel {
    id: String,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write_5m: Option<f64>,
    #[serde(default)]
    cache_write_1h: Option<f64>,
}

#[test]
fn written_bytes_parse_under_the_frozen_base_model_row() {
    let parsed = parse(&row_table(json!({
        "id": "m", "input": 1.0, "output": 2.0, "cache_read": 0.1,
        "cache_write_5m": 1.25, "cache_write_1h": 2.0,
        "cache_write": { "1800": 1.5 },
    })))
    .expect("valid");
    let written = parsed.to_canonical_json().expect("serializes");
    assert!(
        String::from_utf8_lossy(&written).contains("\"cache_write\": {"),
        "positive control: the written file carries the new object"
    );

    let old: FrozenTable = serde_json::from_slice(&written).expect("an older reader parses it");

    assert_eq!(old.schema_version, 1);
    let row = old.providers[0]
        .models
        .iter()
        .find(|row| row.id == "m")
        .expect("row");
    assert_eq!(
        (row.input, row.output, row.cache_read),
        (Some(1.0), Some(2.0), Some(0.1))
    );
    assert_eq!(
        (row.cache_write_5m, row.cache_write_1h),
        (Some(1.25), Some(2.0))
    );
}

#[test]
fn an_old_format_file_parses_and_writes_back_unchanged() {
    let old = parse(&one_provider(json!([{
        "id": "m", "input": 1.5, "output": 3.0, "cache_read": 0.15,
        "cache_write_5m": 1.875, "cache_write_1h": 3.0
    }])))
    .expect("valid")
    .to_canonical_json()
    .expect("serializes");
    assert!(
        !String::from_utf8_lossy(&old).contains("\"cache_write\""),
        "the fixture is in the old format"
    );

    let reparsed = parse_and_validate(&old).expect("old format parses");

    assert_eq!(reparsed.to_canonical_json().expect("serializes"), old);
    let prices = acme_prices(&reparsed, "m").expect("row");
    assert_eq!(
        prices.cache_write_rates(),
        BTreeMap::from([(300, 1.875), (3600, 3.0)])
    );
}
