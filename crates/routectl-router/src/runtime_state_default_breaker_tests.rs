//! The breaker a slot gets when nothing configures one, and the
//! `circuit_failures = 0` switch that turns it off, each driven on a
//! `ProviderState` seeded through the resolver with controlled time.

use super::*;
use crate::config::{Config, resolve_tuning};

const OUTCOME: LastOutcome = LastOutcome::Http5xx;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("valid test toml")
}

/// A slot seeded exactly as the router seeds one for `entry` in `config`.
fn slot_for(config: &Config, entry: Option<&str>) -> ProviderState {
    ProviderState::from_gate(resolve_tuning(config, entry, None).gate())
}

/// Admit and fail `n` attempts at `now`, returning whether any of them
/// opened the breaker.
fn fail_attempts(s: &mut ProviderState, now: Instant, n: u32) -> bool {
    let mut opened = false;
    for _ in 0..n {
        assert_eq!(
            s.try_dispatch(now),
            GateDecision::Allow,
            "premise: admitted"
        );
        opened |= s.record_failure(now, OUTCOME);
    }
    opened
}

#[test]
fn the_shipped_default_opens_on_the_fifth_consecutive_failure() {
    // Arrange
    let mut s = slot_for(&Config::default(), None);
    let t0 = Instant::now();

    // Act / Assert: four debits leave it closed, the fifth opens it.
    assert!(!fail_attempts(&mut s, t0, 4), "four failures must not open");
    assert_eq!(s.consecutive_failures_for_tests(), 4);
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    assert!(s.record_failure(t0, OUTCOME), "the fifth failure opens");
    assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
}

#[test]
fn the_shipped_cooldown_admits_one_retest_at_sixty_seconds() {
    // Arrange: open the default breaker at a known instant.
    let mut s = slot_for(&Config::default(), None);
    let opened_at = Instant::now();
    assert!(fail_attempts(&mut s, opened_at, 5), "premise: opened");

    // Act
    let just_before = s.try_dispatch_admitting(opened_at + Duration::from_millis(59_999));
    let at_cooldown = s.try_dispatch_admitting(opened_at + Duration::from_mins(1));
    let concurrent = s.try_dispatch_admitting(opened_at + Duration::from_mins(1));

    // Assert
    assert_eq!(just_before.decision, GateDecision::CircuitOpen);
    assert!(!just_before.claimed_half_open);
    assert_eq!(at_cooldown.decision, GateDecision::Allow);
    assert!(
        at_cooldown.claimed_half_open,
        "exactly one re-test is admitted"
    );
    assert_eq!(concurrent.decision, GateDecision::CircuitOpen);
    assert!(!concurrent.claimed_half_open);
}

#[test]
fn circuit_failures_zero_never_opens() {
    let provider_zero = parse(
        r#"
[providers.seat-a]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"
circuit_failures = 0
"#,
    );
    let pool_zero = parse(
        r#"
[providers.seat-a]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"

[pools.team]
members = ["seat-a"]
circuit_failures = 0
"#,
    );
    let rows = [
        ("provider 0", &provider_zero, "seat-a"),
        ("pool 0, member unset", &pool_zero, "seat-a"),
        ("pool 0 on the pool's own slot", &pool_zero, "team"),
    ];
    for (row, config, entry) in rows {
        // Arrange
        let mut s = slot_for(config, Some(entry));
        let t0 = Instant::now();

        // Act
        let opened = fail_attempts(&mut s, t0, 50);

        // Assert
        assert!(!opened, "{row}: no failure may open the breaker");
        assert_eq!(s.consecutive_failures_for_tests(), 50, "{row}");
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow, "{row}");
    }
}
