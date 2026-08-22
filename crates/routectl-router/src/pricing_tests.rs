//! Unit tests for the price-unit boundary. Split out of `pricing.rs` via
//! `#[path]` so the implementation file stays under the readability ceiling.

use super::*;
use crate::catalog::{self, CatalogRow};
use crate::catalog_overlay::{CatalogOverlay, OverlayCell, OverlaySource};
use crate::config::{ProviderEntry, RegistryEntry};

/// The baked-catalog oracle this module round-trips against: `anthropic-api`
/// `claude-sonnet-4-6` is baked at $3.00 input / $15.00 output per million
/// tokens (3e-6 / 1.5e-5 per token). Hand-read off the vendor's published
/// price list, not off a program run.
const ORACLE_KIND: &str = "anthropic-api";
const ORACLE_MODEL: &str = "claude-sonnet-4-6";
const ORACLE_INPUT_PER_MTOK: f64 = 3.0;
const ORACLE_OUTPUT_PER_MTOK: f64 = 15.0;

/// A baked cell that prices NEITHER dimension: the `openai-compat` `"*"`
/// catch-all is `price_ambiguous` at codegen (it matches sub-providers at
/// genuinely different prices), so it carries real multipliers and no rates.
const AMBIGUOUS_KIND: &str = "openai-compat";
const AMBIGUOUS_MODEL: &str = "some-unpriced-vendor-model";

fn no_overlay() -> CatalogOverlay {
    CatalogOverlay::default()
}

/// A config with one API-key provider and no `[registry]` pricing at all, so
/// every lookup against it falls through to the catalog layer.
fn config_without_registry(provider: &str, kind_entry: ProviderEntry) -> Config {
    let mut config = Config::default();
    config.providers.insert(provider.to_string(), kind_entry);
    config
}

fn anthropic_config() -> Config {
    config_without_registry("paid", ProviderEntry::anthropic_api("env://KEY"))
}

/// Add a `[registry]` row pricing `upstream` at the given per-million rates.
fn with_registry_pricing(mut config: Config, upstream: &str, pricing: PricingConfig) -> Config {
    config.registry.insert(
        upstream.to_string(),
        RegistryEntry {
            pricing: Some(pricing),
            provider: None,
        },
    );
    config
}

/// An overlay whose cell for `selector` is JSON `null`: the operator disabled
/// the entry outright.
fn overlay_with_null_cell(selector: &str) -> CatalogOverlay {
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(selector.to_string(), None);
    overlay
}

#[test]
fn baked_per_token_rates_round_trip_to_the_exact_per_mtok_oracle() {
    // Arrange: the oracle model has no [registry] row, so the catalog fills.
    let config = anthropic_config();

    // Act
    let (pricing, source) =
        effective_pricing(&config, &no_overlay(), ORACLE_KIND, ORACLE_MODEL, "paid")
            .expect("the baked oracle cell prices both dimensions");

    // Assert: EXACT equality, not an epsilon comparison -- rounding once to
    // the 1e-4 quantum is what makes the recovered decimal exact.
    assert_eq!(source, PricingSource::Catalog);
    assert_eq!(pricing.input_per_mtok, Some(ORACLE_INPUT_PER_MTOK));
    assert_eq!(pricing.output_per_mtok, Some(ORACLE_OUTPUT_PER_MTOK));
}

/// Every distinct per-token rate the baked table carries, paired with the
/// per-million value it MUST convert to. Both columns are authored by hand off
/// the arithmetic (`per_token * 1e6`), not read off a program run, so the pairs
/// are an independent oracle for [`per_mtok`] rather than a restatement of it.
///
/// Spans the table's full range: `3e-7` ($0.30/Mtok) is the cheapest baked
/// rate, `2.5e-5` ($25.00/Mtok) the dearest, and the three-significant-digit
/// entries (`4.35e-7`, `8.7e-7`) are the ones a naive `f32 -> f64` multiply
/// leaves representation dust on.
const RATE_ORACLE: [(f32, f64); 9] = [
    (3e-7, 0.3),
    (4.35e-7, 0.435),
    (8.7e-7, 0.87),
    (1e-6, 1.0),
    (1.2e-6, 1.2),
    (3e-6, 3.0),
    (5e-6, 5.0),
    (1.5e-5, 15.0),
    (2.5e-5, 25.0),
];

#[test]
fn each_baked_rate_converts_to_its_hand_authored_per_mtok_value() {
    // EXACT equality against an independently authored expected value, not a
    // self-comparison: a conversion that quantized stably but landed on the
    // wrong decimal would satisfy the quantum sweep below and still render a
    // wrong price to the operator.
    for (per_token, expected_per_mtok) in RATE_ORACLE {
        assert_eq!(
            per_mtok(per_token),
            expected_per_mtok,
            "{per_token} per token must convert to ${expected_per_mtok} per Mtok"
        );
    }
}

#[test]
fn the_rate_oracle_covers_every_rate_the_baked_table_carries() {
    // The oracle is only a range claim if it spans the range: a codegen run
    // that introduces a new distinct rate must extend RATE_ORACLE with its
    // hand-computed per-million value, not silently ship unpinned.
    for baked in catalog::baked_table_rows() {
        for rate in [
            baked.row.input_cost_per_token,
            baked.row.output_cost_per_token,
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                RATE_ORACLE.iter().any(|(oracle, _)| *oracle == rate),
                "{}:{} carries rate {rate}, which RATE_ORACLE does not pin -- add its \
                 hand-computed per-million value",
                baked.provider_kind,
                baked.model_glob
            );
        }
    }
}

#[test]
fn every_distinct_baked_rate_round_trips_exactly() {
    // The quantum claim is about the whole table, not one cell: each distinct
    // baked rate must recover a value whose 1e-4 quantization is itself exact,
    // so no rendered rate is ever a hair off.
    for baked in catalog::baked_table_rows() {
        for rate in [
            baked.row.input_cost_per_token,
            baked.row.output_cost_per_token,
        ]
        .into_iter()
        .flatten()
        {
            let per_mtok = per_mtok(rate);
            assert_eq!(
                (per_mtok * 1e4).round() / 1e4,
                per_mtok,
                "{}:{} rate {rate} does not land on the 1e-4 quantum",
                baked.provider_kind,
                baked.model_glob
            );
        }
    }
}

#[test]
fn cache_dimensions_are_never_filled_from_the_catalog() {
    // The row's wm / rm multipliers must NOT become cache rates: codegen emits
    // economics-unconfirmed cells with sentinel multipliers next to real base
    // rates, so deriving cache dollars from them would fabricate figures.
    let config = anthropic_config();

    let (pricing, _) = effective_pricing(&config, &no_overlay(), ORACLE_KIND, ORACLE_MODEL, "paid")
        .expect("priced");

    assert_eq!(pricing.cache_read_per_mtok, None);
    assert_eq!(pricing.cache_write_5m_per_mtok, None);
    assert_eq!(pricing.cache_write_1h_per_mtok, None);
}

#[test]
fn an_ambiguous_catch_all_row_is_unpriced_and_never_zero() {
    // Arrange: one config, two selectors -- a model whose only matching baked
    // cell prices nothing, and the priced oracle as the POSITIVE CONTROL that
    // proves this fixture path can produce a price at all.
    let config = config_without_registry(
        "paid",
        ProviderEntry::openai_compat("https://example.invalid", "env://KEY"),
    );
    let priced_control = anthropic_config();

    // Act
    let ambiguous = effective_pricing(
        &config,
        &no_overlay(),
        AMBIGUOUS_KIND,
        AMBIGUOUS_MODEL,
        "paid",
    );
    let control = effective_pricing(
        &priced_control,
        &no_overlay(),
        ORACLE_KIND,
        ORACLE_MODEL,
        "paid",
    );

    // Assert: ABSENT, and specifically not a fabricated zero -- a zero rate
    // would silently bill real usage as free.
    assert!(ambiguous.is_none());
    assert_ne!(
        ambiguous.map(|(p, _)| p.input_per_mtok),
        Some(Some(0.0)),
        "an ambiguous cell must be unpriced, never priced at zero"
    );
    assert!(
        control.is_some(),
        "positive control: the same call shape does yield a price for a priced cell"
    );
}

#[test]
fn an_operator_registry_row_wins_whole_and_verbatim() {
    // Arrange: a [registry] row that DISAGREES with the baked rates and prices
    // only the input dimension. A per-field merge would backfill output from
    // the catalog, overwriting a deliberate omission.
    let config = with_registry_pricing(
        anthropic_config(),
        ORACLE_MODEL,
        PricingConfig {
            input_per_mtok: Some(1.25),
            ..Default::default()
        },
    );

    // Act
    let (pricing, source) =
        effective_pricing(&config, &no_overlay(), ORACLE_KIND, ORACLE_MODEL, "paid")
            .expect("the registry row prices this upstream");

    // Assert: verbatim -- the operator's rate, and the omitted dimensions
    // still omitted.
    assert_eq!(source, PricingSource::Registry);
    assert_eq!(pricing.input_per_mtok, Some(1.25));
    assert_eq!(
        pricing.output_per_mtok, None,
        "a deliberately omitted dimension must not be backfilled from the catalog"
    );
}

#[test]
fn a_null_overlay_cell_disables_the_catalog_fill() {
    // Arrange: the operator explicitly disabled the oracle's selector.
    let config = anthropic_config();
    let overlay = overlay_with_null_cell("anthropic-api:claude-sonnet-4-6*");

    // Act / Assert: Disabled folds to unpriced, same as Missing.
    assert!(
        effective_pricing(&config, &overlay, ORACLE_KIND, ORACLE_MODEL, "paid").is_none(),
        "a disabled cell must yield no pricing"
    );
}

#[test]
fn an_overlay_cell_rate_wins_over_the_baked_rate() {
    // The overlay is the correction channel: a rate it carries must be what
    // the fill converts, not the baked rate it replaced.
    let config = anthropic_config();
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "anthropic-api:claude-sonnet-4-6*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-08-21".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: Some(2.5e-6),
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    let (pricing, source) =
        effective_pricing(&config, &overlay, ORACLE_KIND, ORACLE_MODEL, "paid").expect("priced");

    assert_eq!(source, PricingSource::Catalog);
    assert_eq!(pricing.input_per_mtok, Some(2.5));
    assert_eq!(
        pricing.output_per_mtok,
        Some(ORACLE_OUTPUT_PER_MTOK),
        "an overlay cell fills sparsely; the baked output rate still applies"
    );
}

#[test]
fn an_empty_provider_kind_fills_from_no_catalog_cell() {
    // The kind is half the catalog key, so an empty one identifies no cell. A
    // subject with an unknown kind must fail closed rather than borrow the
    // rates of whatever a kindless lookup would happen to match.
    let config = anthropic_config();

    assert!(
        effective_pricing(&config, &no_overlay(), "", ORACLE_MODEL, "paid").is_none(),
        "an empty provider kind must not resolve a catalog row"
    );
}

#[test]
fn an_empty_provider_kind_still_honors_an_operator_registry_row() {
    // Failing closed applies to the CATALOG fill only: the registry table is
    // keyed on (upstream, provider) and never on the kind, so an operator who
    // priced an upstream explicitly gets that price regardless.
    let config = with_registry_pricing(
        anthropic_config(),
        ORACLE_MODEL,
        PricingConfig {
            input_per_mtok: Some(7.0),
            ..Default::default()
        },
    );

    let (pricing, source) =
        effective_pricing(&config, &no_overlay(), "", ORACLE_MODEL, "paid").expect("priced");

    assert_eq!(source, PricingSource::Registry);
    assert_eq!(pricing.input_per_mtok, Some(7.0));
}

#[test]
fn a_row_pricing_only_one_dimension_still_yields_pricing() {
    // Arrange: `None` is returned only when NEITHER dimension is priced. One
    // priced dimension is a real, partial rate table.
    let mut row = CatalogRow::sentinel();
    row.output_cost_per_token = Some(1.5e-5);

    // Act
    let pricing = pricing_from_catalog_row(&row).expect("one priced dimension is enough");

    // Assert
    assert_eq!(pricing.input_per_mtok, None);
    assert_eq!(pricing.output_per_mtok, Some(ORACLE_OUTPUT_PER_MTOK));
}

#[test]
fn a_row_pricing_neither_dimension_yields_no_pricing() {
    assert!(pricing_from_catalog_row(&CatalogRow::sentinel()).is_none());
}

#[test]
fn a_zero_baked_rate_converts_to_a_priced_zero_not_an_absence() {
    // A genuinely free tier is a real vendor offering, and codegen passes a
    // source zero through. `Some(0.0)` must stay a PRICE (contributing nothing
    // to a cost) rather than collapsing into the unpriced state.
    let mut row = CatalogRow::sentinel();
    row.input_cost_per_token = Some(0.0);

    let pricing = pricing_from_catalog_row(&row).expect("a free tier is priced");

    assert_eq!(pricing.input_per_mtok, Some(0.0));
}
