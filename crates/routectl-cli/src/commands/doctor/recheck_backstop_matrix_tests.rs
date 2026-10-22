//! The doctor's action for a negative the free token-count re-check owns
//! matches the runtime decision on both sides of the re-check backstop.
//!
//! The runtime lapse and the display lapse read one rule; this pins that the
//! snapshot carries what the display needs to apply it, by reading a real
//! Router's resident entry at the same explicit instants on both surfaces.

use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::{
    ModelEntry, NegativeState, ProviderEntry, RECHECK_BACKSTOP, Router, TEST_RECHECK_PATH,
    plant_acting_field_verdict_for_tests,
};

use super::lane_matrix_tests::{available, cell, context};
use super::matrix::build_capability_matrix_panel;
use super::*;

const PROVIDER: &str = "p1";
const UPSTREAM: &str = "claude-sonnet-4-5";
const LANE: &str = "p1#claude-sonnet-4-5";

fn recheck_config() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        PROVIDER.to_string(),
        ProviderEntry::anthropic_api("env://KEY"),
    );
    config
        .models
        .insert("m1".to_string(), ModelEntry::new(PROVIDER, UPSTREAM));
    config
}

/// The display action the doctor reports for the cell at `at`, read from the
/// router's own snapshot.
fn doctor_action(config: &Config, entries: &[LearnedRegistryEntry], at: Instant) -> &'static str {
    let panel =
        build_capability_matrix_panel(&context(config.clone(), available(entries.to_vec(), at, 0)));
    cell(&panel, LANE, &entries[0].feature_key).action
}

/// The runtime decay state at `at`, read without claiming a re-probe slot.
fn runtime_state(router: &Router, feature_key: &str, at: Instant) -> NegativeState {
    router
        .learned_registry()
        .negative_state_in_generation(
            router.registry_generation(),
            LANE,
            feature_key,
            "anthropic-api",
            at,
        )
        .expect("the live generation reads its own key")
        .0
}

/// Mutation check: read `lapsed` in `LearnedInput::from_entry` as
/// `now >= entry.expires_at` again -> red at the decay-deadline row: the doctor
/// reports a re-probe the runtime does not take.
#[test]
fn the_doctor_action_for_a_recheck_owned_negative_matches_runtime_across_the_backstop() {
    // Arrange
    let config = recheck_config();
    let router = Router::new(Arc::new(config.clone())).with_synthetic_field_rows_for_tests();
    plant_acting_field_verdict_for_tests(&router, LANE, TEST_RECHECK_PATH, 0);
    let entries = router.learned_capability_snapshot();
    let [planted] = entries.as_slice() else {
        panic!("premise: exactly the planted negative is resident: {entries:?}");
    };
    assert!(
        planted.state_key == LANE && planted.feature_key.ends_with(TEST_RECHECK_PATH),
        "premise: the resident negative is the planted re-check row: {planted:?}",
    );
    let expires_at = planted.expires_at;
    let backstop = expires_at + RECHECK_BACKSTOP;
    let rows = [
        (
            "at the decay deadline",
            expires_at,
            NegativeState::Acting,
            "route_away",
        ),
        (
            "just inside the backstop",
            expires_at + RECHECK_BACKSTOP.saturating_sub(Duration::from_secs(1)),
            NegativeState::Acting,
            "route_away",
        ),
        (
            "at the backstop",
            backstop,
            NegativeState::Lapsed,
            "reprobe",
        ),
    ];

    for (case, at, runtime, action) in rows {
        // Act
        let observed_runtime = runtime_state(&router, &planted.feature_key, at);
        let observed_action = doctor_action(&config, &entries, at);

        // Assert
        assert_eq!(observed_runtime, runtime, "{case}: runtime");
        assert_eq!(observed_action, action, "{case}: doctor action");
    }
}
