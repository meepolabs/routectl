//! The matrix shows what dispatch does: a cell resolves through the
//! precedence seam's display mode, so the operator wire pin and the Converse
//! representability check reach the doctor exactly as they reach dispatch,
//! and each cell names the tier its winner beat.

use std::time::{Duration, Instant};

use routectl_core::capability::{FailurePhase, Verdict};
use routectl_router::{CapabilityMatrixPanel, LearnedRegistryEntry, beta_capability_key};

use super::lane_matrix_tests::{available, cell, context, negative};
use super::matrix::build_capability_matrix_panel;
use super::*;

const PINNED_FLAG: &str = "fx-pinned";
const CLIENT_FLAG: &str = "fx-client";
const BEDROCK_LANE: &str = "bed#anthropic.claude-test-v1:0";
const CONVERSE_LANE: &str = "cv#us.amazon.nova-2-lite-v1:0";

/// A Bedrock Messages entry whose `anthropic_beta` floor pins
/// [`PINNED_FLAG`], and a Converse entry serving a non-Claude model.
fn bedrock_config(extra: &str) -> Config {
    toml::from_str(&format!(
        "version = 3\n\
         [providers.bed]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         anthropic_beta = [\"{PINNED_FLAG}\"]\n\
         [providers.cv]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         api_shape = \"converse\"\n\
         [models.claude]\n\
         provider = \"bed\"\n\
         upstream = \"anthropic.claude-test-v1:0\"\n\
         [models.nova]\n\
         provider = \"cv\"\n\
         upstream = \"us.amazon.nova-2-lite-v1:0\"\n\
         {extra}"
    ))
    .expect("fixture config parses")
}

fn beta(flag: &str) -> String {
    beta_capability_key(flag).expect("a well-formed flag")
}

fn verified(lane: &str, cap: &str, now: Instant) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        verdict: Verdict::VerifiedWorking,
        ..negative(lane, cap, FailurePhase::F1, now, now)
    }
}

fn acting_negative(lane: &str, cap: &str, now: Instant) -> LearnedRegistryEntry {
    negative(
        lane,
        cap,
        FailurePhase::F1,
        now,
        now + Duration::from_hours(1),
    )
}

fn panel(config: Config, entries: Vec<LearnedRegistryEntry>) -> CapabilityMatrixPanel {
    let now = Instant::now();
    build_capability_matrix_panel(&context(config, available(entries, now, 0)))
}

/// Mutation check: gather `pinned` as `false` in the lane-cell helper -> red:
/// the cell falls to the learned negative and shows `withhold`.
#[test]
fn a_pinned_beta_flag_with_an_acting_learned_negative_shows_the_send_dispatch_takes() {
    // Arrange
    let now = Instant::now();
    let key = beta(PINNED_FLAG);
    let entries = vec![acting_negative(BEDROCK_LANE, &key, now)];

    // Act
    let panel = panel(bedrock_config(""), entries);

    // Assert
    let pinned = cell(&panel, BEDROCK_LANE, &key);
    assert_eq!(pinned.action, "allow", "dispatch sends a pinned flag");
    assert_eq!(pinned.layer, Some("pin"));
    assert_eq!(pinned.source, Some("pin"));
    assert_eq!(pinned.verdict, "forced_supported");
    assert_eq!(pinned.supported, Some(true));
}

/// Mutation check: build the cell context without the lane's egress kind ->
/// red: the flag shows the learned positive's `allow`.
#[test]
fn a_converse_lane_whose_model_refuses_the_beta_bag_key_withholds_a_client_flag() {
    // Arrange
    let now = Instant::now();
    let key = beta(CLIENT_FLAG);
    let entries = vec![verified(CONVERSE_LANE, &key, now)];

    // Act
    let panel = panel(bedrock_config(""), entries);

    // Assert
    let refused = cell(&panel, CONVERSE_LANE, &key);
    assert_eq!(refused.action, "withhold");
    assert_eq!(refused.layer, Some("representability"));
    assert_eq!(refused.source, Some("representability"));
    assert_eq!(refused.verdict, "unrepresentable");
    assert_eq!(refused.supported, Some(false));
}

#[test]
fn a_client_flag_on_a_claude_lane_is_not_withheld_for_representability() {
    // Arrange
    let now = Instant::now();
    let key = beta(CLIENT_FLAG);
    let entries = vec![verified(BEDROCK_LANE, &key, now)];

    // Act
    let panel = panel(bedrock_config(""), entries);

    // Assert
    let carried = cell(&panel, BEDROCK_LANE, &key);
    assert_eq!(carried.action, "allow");
    assert_eq!(carried.layer, Some("learned"));
}

const FEATURE: &str = "web_search";

/// (name, lane, column, layer, overrode_layer, action)
type TierRow<'a> = (
    &'a str,
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
);
const FORCE_FEATURE: &str =
    "[capability.overrides.\"bed:claude\"]\nforce_supported = [\"web_search\"]\n";

/// Mutation check: map `overrode_layer` to `None` in the lane-cell helper ->
/// red at every row naming a beaten tier.
#[test]
fn each_cell_names_the_tier_its_winner_beat() {
    // Arrange
    let now = Instant::now();
    let pinned = beta(PINNED_FLAG);
    let client = beta(CLIENT_FLAG);
    let entries = vec![
        acting_negative(BEDROCK_LANE, &pinned, now),
        acting_negative(BEDROCK_LANE, FEATURE, now),
        acting_negative(BEDROCK_LANE, &client, now),
        verified(CONVERSE_LANE, &client, now),
    ];

    // Act
    let panel = panel(bedrock_config(FORCE_FEATURE), entries);

    // Assert
    let rows: [TierRow; 4] = [
        (
            "a pin over a learned negative",
            BEDROCK_LANE,
            &pinned,
            Some("pin"),
            Some("learned"),
            "allow",
        ),
        (
            "an override over a learned negative",
            BEDROCK_LANE,
            FEATURE,
            Some("override"),
            Some("learned"),
            "allow",
        ),
        (
            "representability over a learned positive",
            CONVERSE_LANE,
            &client,
            Some("representability"),
            Some("learned"),
            "withhold",
        ),
        (
            "a learned negative with nothing beneath it",
            BEDROCK_LANE,
            &client,
            Some("learned"),
            None,
            "withhold",
        ),
    ];
    for (name, lane, column, layer, overrode, action) in rows {
        let shown = cell(&panel, lane, column);
        assert_eq!(
            (shown.layer, shown.overrode_layer, shown.action),
            (layer, overrode, action),
            "{name}"
        );
    }
}

#[test]
fn nicknames_that_disagree_only_on_the_beaten_tier_keep_one_verdict() {
    // Arrange: two nicknames on one lane both force-support the feature; only
    // `claude` has a false prior beneath the override.
    let config = bedrock_config(
        "[models.claude2]\n\
         provider = \"bed\"\n\
         upstream = \"anthropic.claude-test-v1:0\"\n\
         [capability.overrides.bed]\n\
         force_supported = [\"web_search\"]\n",
    );
    let mut ctx = context(config, available(Vec::new(), Instant::now(), 0));
    let mut priors = ctx
        .capability
        .config
        .take()
        .map(|c| c.priors)
        .unwrap_or_default();
    priors.push(PriorCell {
        nickname: "claude".to_string(),
        verified_at: "2026-01-01".to_string(),
        capabilities: vec![(FEATURE.to_string(), false)],
    });
    ctx.capability.config = Some(CapabilityConfig { priors });

    // Act
    let panel = build_capability_matrix_panel(&ctx);

    // Assert
    let forced = cell(&panel, BEDROCK_LANE, FEATURE);
    assert_eq!(forced.verdict, "forced_supported");
    assert_eq!(forced.action, "allow");
    assert_eq!(forced.layer, Some("override"));
    assert_eq!(forced.overrode_layer, None, "the nicknames disagree");
    assert!(
        forced.nickname_actions.is_empty(),
        "an agreeing verdict and action list no nicknames: {:?}",
        forced.nickname_actions
    );
}
