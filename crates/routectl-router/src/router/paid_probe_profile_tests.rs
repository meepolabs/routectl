//! What the catalog must confirm before a paid-probe body may be sized, and
//! what the derived allowance is worth on the real wire.
//!
//! Two halves, and both are needed. The derivation half enumerates every
//! missing or degenerate catalog fact by name, each against a FULLY PRICED
//! control that differs in exactly that one fact -- so a check whose removal
//! would widen the admitted set has a case that goes red. The wire half sends
//! the derived allowance through the REAL Anthropic request serializer, because
//! the floor is a claim about that serializer's behavior and a hand-written
//! body would only restate the claim.

use super::*;

use std::sync::Arc;

use routectl_core::{ChatRequest, Message, MessageContent, Provider, ReasoningConfig, Role};

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::resolved::ResolvedModel;
use crate::router::probe_test_support::{GROUNDED_PATH, OkProvider};

/// A stamp date for a synthetic effective row. Its value is never read by the
/// derivation -- staleness is not one of the facts under test -- so any
/// well-formed date serves.
const STAMP: &str = "2026-01-01";

/// A priced-looking baked row with both base rates and an output ceiling set.
///
/// Built from the catalog's own sentinel and then narrowed, so a field this
/// module does not name keeps whatever the sentinel says rather than a value
/// invented here.
fn row(input: Option<f32>, output: Option<f32>, ceiling: Option<u32>) -> CatalogRow {
    let mut row = CatalogRow::sentinel();
    row.input_cost_per_token = input;
    row.output_cost_per_token = output;
    row.max_output_tokens = ceiling;
    row
}

/// A present effective cell carrying `row`.
fn present(row: CatalogRow) -> EffectiveRow {
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: STAMP.to_string(),
    }
}

/// THE fully-priced control cell: both rates finite and positive, ceiling well
/// above either floor. Every negative case below is this cell with exactly one
/// fact changed, so a case that goes green when a check is removed is a case
/// that was not discriminating.
fn fully_priced() -> EffectiveRow {
    present(row(Some(3.0e-6), Some(1.5e-5), Some(64_000)))
}

/// A single-seat router whose `m1` model carries `effective_row` and
/// `adaptive`, on an ANTHROPIC-API provider entry with no operator output
/// ceiling -- the shape every catalog and rate case below varies one fact of.
fn router_with(effective_row: EffectiveRow, adaptive: bool) -> Router {
    router_built(effective_row, adaptive, 0, ProviderKind::AnthropicApi)
}

/// Which `[providers]` entry shape a fixture router installs.
#[derive(Clone, Copy)]
enum ProviderKind {
    AnthropicApi,
    /// A kind whose egress is a DIFFERENT serializer, so the Anthropic wire
    /// shape's minimum output allowance says nothing about it.
    OpenaiCompat,
}

impl ProviderKind {
    fn entry(self) -> ProviderEntry {
        match self {
            Self::AnthropicApi => ProviderEntry::anthropic_api("literal:k"),
            Self::OpenaiCompat => {
                ProviderEntry::openai_compat("https://api.example.invalid", "literal:k")
            }
        }
    }
}

/// THE single fixture router builder: one provider entry `p1` of `kind`, one
/// model `m1` carrying `effective_row`, `adaptive`, and the operator
/// `max_output_tokens` ceiling `configured_ceiling` (`0` meaning unset, the
/// production sentinel).
fn router_built(
    effective_row: EffectiveRow,
    adaptive: bool,
    configured_ceiling: u32,
    kind: ProviderKind,
) -> Router {
    let mut config = Config::default();
    config.providers.insert("p1".to_string(), kind.entry());
    config
        .models
        .insert("m1".to_string(), ModelEntry::new("p1", "claude-sonnet-4-5"));
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("m1".to_string()));
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let model = ResolvedModel::new("m1", "p1", provider, "claude-sonnet-4-5")
        .with_supports_adaptive_thinking(adaptive)
        .with_max_output_tokens(configured_ceiling)
        .with_effective_row(effective_row);
    let mut models = std::collections::BTreeMap::new();
    models.insert("m1".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// The identity for `m1` on the acting lane.
fn key() -> FieldVerdictKey {
    FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// The profile for a router built from `effective_row` and `adaptive`.
fn profile_for(effective_row: EffectiveRow, adaptive: bool) -> Option<PaidProbeProfile> {
    router_with(effective_row, adaptive).paid_probe_profile(&key())
}

/// The profile for a router carrying an operator `max_output_tokens` of
/// `configured_ceiling`.
fn profile_with_configured_ceiling(
    effective_row: EffectiveRow,
    adaptive: bool,
    configured_ceiling: u32,
) -> Option<PaidProbeProfile> {
    router_built(
        effective_row,
        adaptive,
        configured_ceiling,
        ProviderKind::AnthropicApi,
    )
    .paid_probe_profile(&key())
}

// ---------------------------------------------------------------------------
// Positive controls: a fully priced cell yields each shape's own floor
// ---------------------------------------------------------------------------

#[test]
fn a_fully_priced_legacy_cell_yields_the_legacy_floor() {
    let profile = profile_for(fully_priced(), false).expect("a fully priced cell must profile");

    assert_eq!(profile.max_tokens(), LEGACY_MIN_VIABLE_MAX_TOKENS);
    assert_eq!(profile.max_tokens(), 1025);
}

#[test]
fn a_fully_priced_adaptive_cell_yields_the_adaptive_floor() {
    let profile = profile_for(fully_priced(), true).expect("a fully priced cell must profile");

    assert_eq!(profile.max_tokens(), ADAPTIVE_MIN_VIABLE_MAX_TOKENS);
    assert_eq!(profile.max_tokens(), 1);
}

#[test]
fn the_profile_reports_the_catalog_ceiling_it_was_admitted_against() {
    let profile = profile_for(fully_priced(), false).expect("a fully priced cell must profile");

    assert_eq!(profile.output_ceiling_tokens(), 64_000);
}

// ---------------------------------------------------------------------------
// Every missing or degenerate catalog fact, by name
// ---------------------------------------------------------------------------

#[test]
fn a_missing_catalog_cell_yields_no_profile() {
    assert_eq!(profile_for(EffectiveRow::Missing, false), None);
}

#[test]
fn a_disabled_catalog_cell_yields_no_profile() {
    assert_eq!(profile_for(EffectiveRow::Disabled, false), None);
}

#[test]
fn an_absent_input_rate_yields_no_profile() {
    let cell = present(row(None, Some(1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn an_absent_output_rate_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), None, Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_zero_input_rate_yields_no_profile() {
    let cell = present(row(Some(0.0), Some(1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_zero_output_rate_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), Some(0.0), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_negative_input_rate_yields_no_profile() {
    let cell = present(row(Some(-3.0e-6), Some(1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_negative_output_rate_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), Some(-1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_nan_input_rate_yields_no_profile() {
    let cell = present(row(Some(f32::NAN), Some(1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_nan_output_rate_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), Some(f32::NAN), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn an_infinite_input_rate_yields_no_profile() {
    let cell = present(row(Some(f32::INFINITY), Some(1.5e-5), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn an_infinite_output_rate_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), Some(f32::NEG_INFINITY), Some(64_000)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn an_absent_output_ceiling_yields_no_profile() {
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), None));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_zero_output_ceiling_yields_no_profile() {
    // The one shape a zero ceiling could otherwise reach: the adaptive floor is
    // 1, so a derivation pinning only "ceiling >= floor" without treating zero
    // as unconfirmed would still refuse here -- but a derivation that clamped
    // instead would size a body at zero output tokens. Asserted for BOTH shapes
    // for that reason.
    let legacy = present(row(Some(3.0e-6), Some(1.5e-5), Some(0)));
    let adaptive = present(row(Some(3.0e-6), Some(1.5e-5), Some(0)));

    assert_eq!(profile_for(legacy, false), None);
    assert_eq!(profile_for(adaptive, true), None);
}

#[test]
fn an_identity_naming_no_resolved_model_yields_no_profile() {
    let router = router_with(fully_priced(), false);
    let absent =
        FieldVerdictKey::new("no-such-model", GROUNDED_PATH, "anthropic-api").expect("identity");

    assert_eq!(router.paid_probe_profile(&absent), None);
}

// ---------------------------------------------------------------------------
// Ceilings exactly at, and one below, each floor
// ---------------------------------------------------------------------------

#[test]
fn a_ceiling_exactly_at_the_legacy_floor_still_profiles() {
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), Some(1025)));

    let profile = profile_for(cell, false).expect("a ceiling at the floor is viable");

    assert_eq!(profile.max_tokens(), 1025);
    assert_eq!(profile.output_ceiling_tokens(), 1025);
}

#[test]
fn a_ceiling_one_below_the_legacy_floor_yields_no_profile() {
    // The constraint is VIABILITY, not a value to clamp: 1024 cannot carry the
    // legacy shape at all, so there is no smaller body to fall back to.
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), Some(1024)));

    assert_eq!(profile_for(cell, false), None);
}

#[test]
fn a_ceiling_exactly_at_the_adaptive_floor_still_profiles() {
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), Some(1)));

    let profile = profile_for(cell, true).expect("a ceiling at the floor is viable");

    assert_eq!(profile.max_tokens(), 1);
    assert_eq!(profile.output_ceiling_tokens(), 1);
}

#[test]
fn a_ceiling_below_the_legacy_floor_still_profiles_on_the_adaptive_shape() {
    // The floor is SHAPE-KEYED, so the same cell that is unviable for legacy is
    // viable for adaptive. Without this, a derivation that applied the legacy
    // floor to both shapes would pass every other case here.
    let cell = present(row(Some(3.0e-6), Some(1.5e-5), Some(1024)));

    let profile = profile_for(cell, true).expect("1024 is far above the adaptive floor");

    assert_eq!(profile.max_tokens(), 1);
}

// The pooled-resolution group and the ceiling/lane/serializer group live in
// sibling files to keep every file under the size ceiling. They compile into
// THIS module via `include!`, so the fixture helpers above stay in scope and no
// test's module path changes.
include!("paid_probe_profile_pooled_tests.rs");
include!("paid_probe_profile_wire_tests.rs");
