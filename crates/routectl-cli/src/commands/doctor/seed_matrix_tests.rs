//! The shipped beta seed as its own matrix layer: seeded columns on lanes of
//! the seed's kind, the seed cell's source and action, a cleared seed from a
//! replayed marker, and a learned beta negative's withhold action.

use std::time::{Duration, Instant};

use routectl_core::capability::FailurePhase;
use routectl_router::{BetaSeedScope, CATALOG_VERSION, CapabilityMatrixPanel, beta_capability_key};
use routectl_usage::CapabilityEvent;

use super::gather::gather_capability_matrix;
use super::lane_matrix_tests::{available, cell, context, negative, now_ms, seeded_ledger};
use super::matrix::build_capability_matrix_panel;
use super::*;

const SEED_KIND: &str = "bedrock";
const SEEDED: &str = "fx-seeded";
const SEED: &[&str] = &[SEEDED];
const BEDROCK_LANE: &str = "aws#up";
const OTHER_LANE: &str = "p#up";

fn seed_scope(flags: &'static [&'static str]) -> BetaSeedScope {
    BetaSeedScope::new(SEED_KIND, flags)
}

fn beta(flag: &str) -> String {
    beta_capability_key(flag).expect("a well-formed flag")
}

/// One Bedrock lane and one OpenAI-compatible lane, each on upstream `up`.
fn seed_config() -> Config {
    toml::from_str(
        "version = 3\n\
         [providers.aws]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid\"\n\
         api_key_ref = \"literal:k\"\n\
         [models.claude]\n\
         provider = \"aws\"\n\
         upstream = \"up\"\n\
         [models.gpt]\n\
         provider = \"p\"\n\
         upstream = \"up\"\n",
    )
    .expect("seed config parses")
}

fn empty_source() -> CapabilityMatrixSource {
    CapabilityMatrixSource::Empty {
        replay: MatrixReplaySummary::default(),
        seed_clears: Vec::new(),
    }
}

fn panel_with(
    config: Config,
    source: CapabilityMatrixSource,
    seed: BetaSeedScope,
) -> CapabilityMatrixPanel {
    build_capability_matrix_panel(&DoctorContext {
        beta_seed: seed,
        ..context(config, source)
    })
}

#[test]
fn a_seeded_flag_is_a_seed_cell_on_a_bedrock_lane_with_no_ledger_row() {
    let panel = panel_with(seed_config(), empty_source(), seed_scope(SEED));

    let seeded = cell(&panel, BEDROCK_LANE, &beta(SEEDED));
    assert_eq!(seeded.verdict, "broken");
    assert_eq!(seeded.supported, Some(false));
    assert_eq!(seeded.source, Some("seed"));
    assert_eq!(seeded.layer, Some("seed"));
    assert_eq!(seeded.action, "withhold");
    assert_eq!(seeded.expires_at_ms, None, "the seed never decays");
    assert_eq!(
        seeded.first_seen_ms, None,
        "no ledger row backs a seed cell"
    );
    assert_eq!(seeded.age_ms, None);

    let other = cell(&panel, OTHER_LANE, &beta(SEEDED));
    assert_eq!(other.verdict, "unknown");
    assert_eq!(other.source, None, "the seed applies to bedrock lanes only");
    assert_eq!(other.action, "none");
}

#[test]
fn the_seed_withholds_with_the_capability_switch_off() {
    let mut config = seed_config();
    config.capability.enabled = false;

    let panel = panel_with(config, empty_source(), seed_scope(SEED));

    assert_eq!(cell(&panel, BEDROCK_LANE, &beta(SEEDED)).action, "withhold");
}

#[test]
fn no_seed_column_appears_without_a_lane_of_the_seed_kind() {
    let mut config = seed_config();
    config.models.remove("claude");
    config.providers.remove("aws");

    let panel = panel_with(config, empty_source(), seed_scope(SEED));

    assert!(
        !panel.columns.contains(&beta(SEEDED)),
        "columns: {:?}",
        panel.columns
    );
}

#[test]
fn seeded_columns_count_toward_the_other_column_cap() {
    const ELEVEN: &[&str] = &[
        "fx-00", "fx-01", "fx-02", "fx-03", "fx-04", "fx-05", "fx-06", "fx-07", "fx-08", "fx-09",
        "fx-10",
    ];

    let panel = panel_with(seed_config(), empty_source(), seed_scope(ELEVEN));

    let seeded: Vec<String> = ELEVEN.iter().map(|flag| beta(flag)).collect();
    let shown = panel
        .columns
        .iter()
        .filter(|column| seeded.contains(column))
        .count();
    assert_eq!(shown, 10);
    assert_eq!(panel.other_overflow, 1);
}

#[test]
fn a_learned_beta_negative_withholds_with_its_age_and_expiry() {
    let now = Instant::now();
    let entry = negative(
        BEDROCK_LANE,
        &beta(SEEDED),
        FailurePhase::F1,
        now.checked_sub(Duration::from_secs(5))
            .expect("the monotonic clock reaches back this far"),
        now + Duration::from_mins(1),
    );

    let panel = panel_with(
        seed_config(),
        available(vec![entry], now, 1_000_000),
        seed_scope(SEED),
    );

    let learned = cell(&panel, BEDROCK_LANE, &beta(SEEDED));
    assert_eq!(learned.verdict, "broken");
    assert_eq!(
        learned.source,
        Some("live"),
        "a learned verdict beats the seed"
    );
    assert_eq!(learned.layer, Some("learned"));
    assert_eq!(learned.action, "withhold");
    assert_eq!(learned.age_ms, Some(5_000));
    assert_eq!(learned.expires_at_ms, Some(1_060_000));
}

/// A `cleared` row for a beta key on the Bedrock lane.
fn cleared_row(flag: &str) -> CapabilityEvent {
    CapabilityEvent {
        ts: now_ms(),
        lane_key: BEDROCK_LANE.to_string(),
        capability: beta(flag),
        verdict: "cleared".to_string(),
        phase: String::new(),
        source: "live".to_string(),
        tier: String::new(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: i64::from(CATALOG_VERSION),
        overlay_revision: 0,
        provider_kind: Some(SEED_KIND.to_string()),
        vocab_version: Some(routectl_router::CURRENT_VOCAB_VERSION),
    }
}

#[test]
fn a_replayed_seed_clear_renders_a_cleared_seed_cell() {
    let (_dir, path) = seeded_ledger(&[cleared_row(SEEDED)]);
    let mut config = seed_config();
    config.usage.db_path = path;

    let source = gather_capability_matrix(&config, false, 0, seed_scope(SEED));
    let panel = panel_with(config, source, seed_scope(SEED));

    let cleared = cell(&panel, BEDROCK_LANE, &beta(SEEDED));
    assert_eq!(cleared.verdict, "cleared");
    assert_eq!(cleared.supported, Some(true));
    assert_eq!(cleared.source, Some("seed"));
    assert_eq!(cleared.layer, Some("seed"));
    assert_eq!(cleared.action, "allow");
}

#[test]
fn a_seed_clear_on_another_lane_leaves_the_seed_withholding() {
    let marker = SeedClearMarker {
        state_key: "aws#elsewhere".to_string(),
        provider_kind: SEED_KIND.to_string(),
        feature_key: beta(SEEDED),
    };
    let source = CapabilityMatrixSource::Empty {
        replay: MatrixReplaySummary::default(),
        seed_clears: vec![marker],
    };

    let panel = panel_with(seed_config(), source, seed_scope(SEED));

    assert_eq!(cell(&panel, BEDROCK_LANE, &beta(SEEDED)).action, "withhold");
}
