//! Effective pricing: one positive control per source token, the precedence
//! between layers, per-field fill, the registry's whole-row rule, the
//! derived 1-hour write, subscription providers and persisted-kind pricing.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::prices_test_support::{bytes, table};
use super::*;
use crate::config::Config;

const UPSTREAM: &str = "claude-x";

const CONFIG: &str = r#"
[providers.claude]
kind = "anthropic-api"
api_key_ref = "env://A"

[providers.seat]
kind = "anthropic-api"
api_key_ref = "oauth://claude"

[providers.gateway]
kind = "openai-compat"
api_key_ref = "env://B"
base_url = "https://gateway.example/v1"

[models.m]
provider = "claude"
upstream = "claude-x"
"#;

/// Every field set to `value`.
fn all_fields(value: f64) -> PricingConfig {
    PricingConfig {
        input_per_mtok: Some(value),
        output_per_mtok: Some(value),
        cache_read_per_mtok: Some(value),
        cache_write_5m_per_mtok: Some(value),
        cache_write_1h_per_mtok: Some(value),
        cache_write_per_mtok: None,
    }
}

/// Only the input field set to `value`.
fn input_only(value: f64) -> PricingConfig {
    PricingConfig {
        input_per_mtok: Some(value),
        ..PricingConfig::default()
    }
}

/// A canonical model row for `pricing` under id `model`.
fn model_row(model: &str, pricing: &PricingConfig) -> Value {
    let mut row = json!({ "id": model });
    let fields = [
        ("input", pricing.input_per_mtok),
        ("output", pricing.output_per_mtok),
        ("cache_read", pricing.cache_read_per_mtok),
        ("cache_write_5m", pricing.cache_write_5m_per_mtok),
        ("cache_write_1h", pricing.cache_write_1h_per_mtok),
    ];
    for (key, value) in fields {
        if let Some(value) = value {
            row[key] = json!(value);
        }
    }
    row
}

/// A table pricing `upstream` at `pricing` for the first-party Anthropic,
/// Bedrock and the gateway's host, plus a filler row so it is never empty.
fn table_for(upstream: &str, pricing: &PricingConfig) -> PriceTable {
    let row = model_row(upstream, pricing);
    let filler = json!({ "id": "filler", "input": 1.0 });
    parse_and_validate(&bytes(&table(json!([
        { "id": "anthropic", "models": [row.clone(), filler.clone()] },
        { "id": "amazon-bedrock", "models": [row.clone(), filler.clone()] },
        { "id": "gateway", "api_host": "https://gateway.example/v1",
          "models": [row, filler] },
    ]))))
    .expect("fixture table parses")
}

/// The config the test rows mutate.
fn config() -> Config {
    toml::from_str(CONFIG).expect("config parses")
}

fn with_model_pricing(mut config: Config, pricing: PricingConfig) -> Config {
    let entry = config.models.get_mut("m").expect("model m");
    entry.pricing = Some(pricing);
    config
}

fn with_registry(mut config: Config, pricing: PricingConfig) -> Config {
    config.registry.insert(
        UPSTREAM.to_string(),
        crate::config::RegistryEntry {
            pricing: Some(pricing),
            provider: None,
        },
    );
    config
}

fn subject<'a>(provider: &'a str, upstream: &'a str) -> PriceSubject<'a> {
    PriceSubject {
        model: Some("m"),
        provider,
        upstream,
        persisted_kind: None,
    }
}

/// The effective price for the `claude` provider serving `UPSTREAM`.
fn resolve(
    config: &Config,
    refreshed: Option<&PriceTable>,
    shipped: Option<&PriceTable>,
) -> Option<(PricingConfig, PriceSources)> {
    effective_pricing(config, &subject("claude", UPSTREAM), refreshed, shipped)
}

#[test]
fn every_source_token_renders_its_fixed_text() {
    let rows = [
        (PriceSource::ModelConfig, "model-config"),
        (PriceSource::Registry, "registry"),
        (PriceSource::Refreshed, "refreshed"),
        (PriceSource::Shipped, "shipped"),
        (PriceSource::Derived, "derived"),
        (PriceSource::Unknown, "unknown"),
    ];
    for (source, token) in rows {
        assert_eq!(source.as_str(), token, "{source:?}");
    }
}

/// One positive control per source token: in each fixture only that source
/// carries the input price (the `derived` and `unknown` rows read the
/// 1-hour write and the output field, which nothing else carries).
#[test]
fn each_source_alone_supplies_the_field_it_carries() {
    let refreshed = table_for(UPSTREAM, &input_only(3.0));
    let shipped = table_for(UPSTREAM, &input_only(4.0));
    let rows: [(&str, Config, Option<&PriceTable>, Option<&PriceTable>); 4] = [
        (
            "model-config",
            with_model_pricing(config(), input_only(1.0)),
            None,
            None,
        ),
        (
            "registry",
            with_registry(config(), input_only(2.0)),
            None,
            None,
        ),
        ("refreshed", config(), Some(&refreshed), None),
        ("shipped", config(), None, Some(&shipped)),
    ];
    let expected = [
        (PriceSource::ModelConfig, 1.0),
        (PriceSource::Registry, 2.0),
        (PriceSource::Refreshed, 3.0),
        (PriceSource::Shipped, 4.0),
    ];
    for ((name, config, refreshed, shipped), (source, input)) in rows.into_iter().zip(expected) {
        let (pricing, sources) = resolve(&config, refreshed, shipped).expect(name);
        assert_eq!(pricing.input_per_mtok, Some(input), "{name}");
        assert_eq!(sources.input, source, "{name}");
    }

    let (pricing, sources) = resolve(&config(), None, Some(&shipped)).expect("derived");
    assert_eq!(pricing.cache_write_1h_per_mtok, Some(8.0), "derived");
    assert_eq!(
        sources.cache_write_at(3600),
        PriceSource::Derived,
        "derived"
    );
    assert_eq!(pricing.output_per_mtok, None, "unknown");
    assert_eq!(sources.output, PriceSource::Unknown, "unknown");
}

/// Each layer wins over every layer below it when all of them carry a
/// value for every field. Swapping any two layers turns a row red.
#[test]
fn each_layer_wins_over_every_layer_below_it() {
    let refreshed = table_for(UPSTREAM, &all_fields(3.0));
    let shipped = table_for(UPSTREAM, &all_fields(4.0));
    let stacked = |model: bool, registry: bool| {
        let mut config = config();
        if model {
            config = with_model_pricing(config, all_fields(1.0));
        }
        if registry {
            config = with_registry(config, all_fields(2.0));
        }
        config
    };
    let rows: [(&str, Config, Option<&PriceTable>, PriceSource, f64); 4] = [
        (
            "model-config over registry and tables",
            stacked(true, true),
            Some(&refreshed),
            PriceSource::ModelConfig,
            1.0,
        ),
        (
            "registry over both tables",
            stacked(false, true),
            Some(&refreshed),
            PriceSource::Registry,
            2.0,
        ),
        (
            "refreshed over shipped",
            stacked(false, false),
            Some(&refreshed),
            PriceSource::Refreshed,
            3.0,
        ),
        (
            "shipped with no refreshed table",
            stacked(false, false),
            None,
            PriceSource::Shipped,
            4.0,
        ),
    ];
    for (name, config, refreshed, source, value) in rows {
        let (pricing, sources) = resolve(&config, refreshed, Some(&shipped)).expect(name);
        assert_eq!(pricing, all_fields(value), "{name}");
        assert_eq!(
            sources,
            PriceSources {
                input: source,
                output: source,
                cache_read: source,
                cache_write: [(300, source), (3600, source)].into(),
            },
            "{name}"
        );
    }
}

#[test]
fn a_partial_model_pricing_fills_field_by_field_over_the_table() {
    let shipped = table_for(UPSTREAM, &all_fields(4.0));
    let config = with_model_pricing(
        config(),
        PricingConfig {
            output_per_mtok: Some(1.5),
            ..PricingConfig::default()
        },
    );

    let (pricing, sources) = resolve(&config, None, Some(&shipped)).expect("priced");

    assert_eq!(pricing.output_per_mtok, Some(1.5));
    assert_eq!(sources.output, PriceSource::ModelConfig);
    assert_eq!(pricing.input_per_mtok, Some(4.0));
    assert_eq!(sources.input, PriceSource::Shipped);
    assert_eq!(pricing.cache_write_1h_per_mtok, Some(4.0));
    assert_eq!(sources.cache_write_at(3600), PriceSource::Shipped);
}

/// A registry row is taken whole: its unset fields stay unknown although
/// both tables price them and the target is Anthropic-family, while a
/// model-config field still wins over it.
#[test]
fn a_registry_row_wins_whole_below_model_config() {
    let refreshed = table_for(UPSTREAM, &all_fields(3.0));
    let shipped = table_for(UPSTREAM, &all_fields(4.0));
    let config = with_model_pricing(
        with_registry(config(), input_only(2.0)),
        PricingConfig {
            cache_read_per_mtok: Some(0.1),
            ..PricingConfig::default()
        },
    );

    let (pricing, sources) = resolve(&config, Some(&refreshed), Some(&shipped)).expect("priced");

    assert_eq!(
        pricing,
        PricingConfig {
            input_per_mtok: Some(2.0),
            cache_read_per_mtok: Some(0.1),
            ..PricingConfig::default()
        }
    );
    assert_eq!(
        sources,
        PriceSources {
            input: PriceSource::Registry,
            output: PriceSource::Unknown,
            cache_read: PriceSource::ModelConfig,
            cache_write: BTreeMap::new(),
        }
    );
}

#[test]
fn a_registry_row_that_prices_nothing_is_returned_as_written() {
    let shipped = table_for(UPSTREAM, &all_fields(4.0));
    let config = with_registry(config(), PricingConfig::default());

    let (pricing, sources) = resolve(&config, None, Some(&shipped)).expect("registry row");

    assert_eq!(pricing, PricingConfig::default());
    assert_eq!(sources.input, PriceSource::Unknown);
}

#[test]
fn no_source_is_unknown() {
    let shipped = table_for("another-model", &all_fields(4.0));

    assert_eq!(resolve(&config(), None, Some(&shipped)), None);
    assert_eq!(resolve(&config(), None, None), None);
}

#[test]
fn model_pricing_applies_only_while_the_entry_names_the_served_upstream() {
    let config = with_model_pricing(config(), input_only(1.0));

    let served_elsewhere = effective_pricing(&config, &subject("claude", "other-id"), None, None);
    let no_model = effective_pricing(
        &config,
        &PriceSubject {
            model: None,
            ..subject("claude", UPSTREAM)
        },
        None,
        None,
    );

    assert_eq!(served_elsewhere, None);
    assert_eq!(no_model, None);
}

/// A resolved price paired with the source that supplied it.
#[cfg(feature = "bedrock")]
type SourcedPrice = (f64, PriceSource);

#[cfg(feature = "bedrock")]
const BEDROCK_CONFIG: &str = r#"
[providers.br]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }
"#;

/// The derived 1-hour write: which targets get it, and when a source
/// already supplies the field.
#[cfg(feature = "bedrock")]
#[test]
fn the_1h_write_is_derived_for_the_anthropic_family_only() {
    let bedrock: Config = toml::from_str(BEDROCK_CONFIG).expect("bedrock config parses");
    let rows: [(&str, &Config, &str, &str, Option<SourcedPrice>); 4] = [
        (
            "anthropic-api",
            &config(),
            "claude",
            UPSTREAM,
            Some((10.0, PriceSource::Derived)),
        ),
        (
            "bedrock anthropic model",
            &bedrock,
            "br",
            "us.anthropic.claude-x-v1:0",
            Some((10.0, PriceSource::Derived)),
        ),
        (
            "bedrock non-anthropic model",
            &bedrock,
            "br",
            "meta.llama-x-v1:0",
            None,
        ),
        ("openai-compat", &config(), "gateway", UPSTREAM, None),
    ];
    for (name, config, provider, upstream, expected) in rows {
        let shipped = table_for(upstream, &input_only(5.0));
        let (pricing, sources) =
            effective_pricing(config, &subject(provider, upstream), None, Some(&shipped))
                .expect(name);
        assert_eq!(pricing.input_per_mtok, Some(5.0), "{name}");
        let got = pricing
            .cache_write_1h_per_mtok
            .map(|value| (value, sources.cache_write_at(3600)));
        assert_eq!(got, expected, "{name}");
    }
}

#[test]
fn a_supplied_1h_write_is_never_replaced_by_the_derived_figure() {
    let shipped_with_1h = table_for(
        UPSTREAM,
        &PricingConfig {
            input_per_mtok: Some(5.0),
            cache_write_1h_per_mtok: Some(7.0),
            ..PricingConfig::default()
        },
    );
    let shipped = table_for(UPSTREAM, &input_only(5.0));
    let model_1h = with_model_pricing(
        config(),
        PricingConfig {
            cache_write_1h_per_mtok: Some(6.0),
            ..PricingConfig::default()
        },
    );
    let rows: [(&str, Config, &PriceTable, (f64, PriceSource)); 2] = [
        (
            "table row carries it",
            config(),
            &shipped_with_1h,
            (7.0, PriceSource::Shipped),
        ),
        (
            "model-config overrides",
            model_1h,
            &shipped,
            (6.0, PriceSource::ModelConfig),
        ),
    ];
    for (name, config, shipped, expected) in rows {
        let (pricing, sources) = resolve(&config, None, Some(shipped)).expect(name);
        assert_eq!(
            pricing
                .cache_write_1h_per_mtok
                .map(|value| (value, sources.cache_write_at(3600))),
            Some(expected),
            "{name}"
        );
    }
}

/// An `anthropic-api` gateway is matched by its host, so its row may come
/// from a table provider that is not Anthropic (OpenRouter's host here, as
/// the shipped table lists it). Only a row under the `anthropic` table
/// provider carries the derived 1-hour write; the control is the same
/// gateway host listed under that id.
#[test]
fn an_anthropic_api_gateway_priced_from_another_table_provider_gets_no_derived_write() {
    const GATEWAY: &str = r#"
[providers.router]
kind = "anthropic-api"
api_key_ref = "env://R"
base_url = "https://openrouter.ai/api/v1"
"#;
    let config: Config = toml::from_str(GATEWAY).expect("config parses");
    let rows = [
        ("openrouter table provider", "openrouter", None),
        (
            "anthropic table provider on the same host",
            "anthropic",
            Some((10.0, PriceSource::Derived)),
        ),
    ];
    for (name, table_provider, expected) in rows {
        let shipped = parse_and_validate(&bytes(&table(json!([{
            "id": table_provider,
            "api_host": "https://openrouter.ai/api/v1",
            "models": [model_row(UPSTREAM, &input_only(5.0))],
        }]))))
        .expect("fixture table parses");

        let (pricing, sources) =
            effective_pricing(&config, &subject("router", UPSTREAM), None, Some(&shipped))
                .expect(name);

        assert_eq!(
            pricing.input_per_mtok,
            Some(5.0),
            "{name}: the row is selected"
        );
        assert_eq!(
            pricing
                .cache_write_1h_per_mtok
                .map(|value| (value, sources.cache_write_at(3600))),
            expected,
            "{name}"
        );
    }
}

/// The derived figure needs a refreshed or shipped basis: a model-config
/// input alone, or a registry row, derives nothing.
#[test]
fn the_1h_write_is_derived_only_on_a_table_basis() {
    let shipped = table_for(UPSTREAM, &input_only(5.0));
    let rows: [(&str, Config, Option<&PriceTable>, Option<f64>); 4] = [
        (
            "model-config input, no table row",
            with_model_pricing(config(), input_only(1.0)),
            None,
            None,
        ),
        (
            "registry input over a table row",
            with_registry(config(), input_only(2.0)),
            Some(&shipped),
            None,
        ),
        (
            "model-config input over a table row",
            with_model_pricing(config(), input_only(1.0)),
            Some(&shipped),
            Some(2.0),
        ),
        ("table input", config(), Some(&shipped), Some(10.0)),
    ];
    for (name, config, shipped, expected) in rows {
        let (pricing, _) = resolve(&config, None, shipped).expect(name);
        assert_eq!(pricing.cache_write_1h_per_mtok, expected, "{name}");
    }
}

#[test]
fn a_subscription_provider_resolves_the_same_rates_as_an_api_key_provider() {
    let shipped = table_for(UPSTREAM, &all_fields(4.0));

    let seat = effective_pricing(&config(), &subject("seat", UPSTREAM), None, Some(&shipped));
    let keyed = effective_pricing(
        &config(),
        &subject("claude", UPSTREAM),
        None,
        Some(&shipped),
    );

    assert!(seat.is_some());
    assert_eq!(seat, keyed);
}

/// A usage row whose provider entry was renamed away prices a first-party
/// upstream through the kind it recorded; with the entry present the same
/// row prices through the entry.
#[test]
fn a_renamed_away_provider_prices_through_its_persisted_kind() {
    let shipped = table_for(UPSTREAM, &input_only(4.0));
    let row = |provider, persisted_kind| PriceSubject {
        model: None,
        provider,
        upstream: UPSTREAM,
        persisted_kind,
    };
    let rows = [
        (
            "entry present",
            row("claude", Some("anthropic-api")),
            Some(4.0),
        ),
        (
            "entry renamed away",
            row("renamed", Some("anthropic-api")),
            Some(4.0),
        ),
        (
            "entry renamed away, no persisted kind",
            row("renamed", None),
            None,
        ),
        (
            "entry renamed away, gateway kind has no first-party table",
            row("renamed", Some("openai-compat")),
            None,
        ),
    ];
    for (name, subject, expected) in rows {
        let got = effective_pricing(&config(), &subject, None, Some(&shipped))
            .and_then(|(pricing, _)| pricing.input_per_mtok);
        assert_eq!(got, expected, "{name}");
    }
}

/// `table` with `rate` for the cache-write lifetime `duration` added to
/// every row pricing `upstream`. The canonical file format has no spelling
/// for an arbitrary lifetime, so the row is edited after parsing.
fn with_table_cache_write(
    mut table: PriceTable,
    upstream: &str,
    duration: &str,
    rate: f64,
) -> PriceTable {
    for provider in &mut table.providers {
        if let Some(prices) = provider.models.get_mut(upstream) {
            prices
                .cache_write_per_mtok
                .get_or_insert_with(BTreeMap::new)
                .insert(duration.to_string(), rate);
        }
    }
    table
}

/// `CONFIG` plus `extra` TOML.
fn config_with(extra: &str) -> Config {
    toml::from_str(&format!("{CONFIG}{extra}")).expect("config parses")
}

/// The generic `cache_write_per_mtok` map prices any lifetime, and the
/// legacy 5m / 1h keys win their own lifetime over it, in a
/// `[models.X.pricing]` block and in a `[registry]` row alike.
#[test]
fn a_cache_write_map_prices_its_lifetimes_and_the_legacy_keys_win_their_own() {
    let rows: [(&str, &str, u32, (f64, PriceSource)); 4] = [
        (
            "model-config 30m",
            "[models.m.pricing]\ncache_write_per_mtok = { \"30m\" = 2.0 }\n",
            1800,
            (2.0, PriceSource::ModelConfig),
        ),
        (
            "model-config legacy 5m beside a generic 5m",
            "[models.m.pricing]\ncache_write_5m_per_mtok = 3.75\n\
             cache_write_per_mtok = { \"5m\" = 9.0 }\n",
            300,
            (3.75, PriceSource::ModelConfig),
        ),
        (
            "registry 30m",
            "[registry.\"claude-*\".pricing]\ncache_write_per_mtok = { \"30m\" = 2.0 }\n",
            1800,
            (2.0, PriceSource::Registry),
        ),
        (
            "registry legacy 5m beside a generic 5m",
            "[registry.\"claude-*\".pricing]\ncache_write_5m_per_mtok = 3.75\n\
             cache_write_per_mtok = { \"5m\" = 9.0 }\n",
            300,
            (3.75, PriceSource::Registry),
        ),
    ];
    for (name, extra, seconds, expected) in rows {
        let (pricing, sources) = resolve(&config_with(extra), None, None).expect(name);

        let got = pricing
            .cache_write_rates()
            .get(&seconds)
            .map(|rate| (*rate, sources.cache_write_at(seconds)));

        assert_eq!(got, Some(expected), "{name}");
    }
}

/// Each lifetime resolves on its own: a model-config price for one lifetime
/// wins that lifetime only, and the tables keep supplying the others.
#[test]
fn a_model_config_lifetime_beats_the_tables_for_that_lifetime_only() {
    let shipped =
        with_table_cache_write(table_for(UPSTREAM, &all_fields(4.0)), UPSTREAM, "30m", 4.0);
    let config = config_with("[models.m.pricing]\ncache_write_per_mtok = { \"30m\" = 2.0 }\n");

    let (pricing, sources) = resolve(&config, None, Some(&shipped)).expect("priced");

    assert_eq!(
        pricing.cache_write_rates(),
        BTreeMap::from([(300, 4.0), (1800, 2.0), (3600, 4.0)])
    );
    assert_eq!(
        sources.cache_write,
        BTreeMap::from([
            (300, PriceSource::Shipped),
            (1800, PriceSource::ModelConfig),
            (3600, PriceSource::Shipped),
        ])
    );
    assert_eq!(sources.input, PriceSource::Shipped);
}

/// A table lifetime with no higher source keeps the table's price; the
/// control for the row above.
#[test]
fn a_table_lifetime_fills_when_no_higher_source_prices_it() {
    let shipped =
        with_table_cache_write(table_for(UPSTREAM, &input_only(4.0)), UPSTREAM, "30m", 4.5);

    let (pricing, sources) = resolve(&config(), None, Some(&shipped)).expect("priced");

    assert_eq!(pricing.cache_write_rates().get(&1800), Some(&4.5));
    assert_eq!(sources.cache_write_at(1800), PriceSource::Shipped);
}

/// The derived Anthropic 1-hour write lands at 3600 beside other
/// lifetimes, and a generic `1h` key from a higher source replaces it.
#[test]
fn the_derived_1h_write_lands_at_3600_unless_a_generic_1h_key_prices_it() {
    let shipped = table_for(UPSTREAM, &input_only(5.0));
    let rows: [(&str, &str, (f64, PriceSource)); 2] = [
        (
            "a 30m model price leaves 3600 to the derived rule",
            "[models.m.pricing]\ncache_write_per_mtok = { \"30m\" = 2.0 }\n",
            (10.0, PriceSource::Derived),
        ),
        (
            "a generic 1h model price wins 3600",
            "[models.m.pricing]\ncache_write_per_mtok = { \"1h\" = 6.0 }\n",
            (6.0, PriceSource::ModelConfig),
        ),
    ];
    for (name, extra, expected) in rows {
        let (pricing, sources) = resolve(&config_with(extra), None, Some(&shipped)).expect(name);

        let got = pricing
            .cache_write_rates()
            .get(&3600)
            .map(|rate| (*rate, sources.cache_write_at(3600)));

        assert_eq!(got, Some(expected), "{name}");
        assert_eq!(pricing.cache_write_1h_per_mtok, Some(expected.0), "{name}");
    }
}
