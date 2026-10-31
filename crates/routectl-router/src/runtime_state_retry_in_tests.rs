//! The wait a breaker or RPM refusal carries: the rest of the open window, or
//! the time to the next whole token, read from the state at the refusal's own
//! instant; unknown while the outcome of a half-open re-test decides it.

use super::*;
use crate::config::tuning::{BreakerThreshold, GateTuning};
use std::num::NonZeroU32;

const COOLDOWN: Duration = Duration::from_secs(10);

fn gate(rpm_limit: Option<u32>) -> GateTuning {
    GateTuning {
        threshold: BreakerThreshold::After(NonZeroU32::MIN),
        cooldown: COOLDOWN,
        rpm_limit,
    }
}

/// A breaker that tripped at `t0` on its baseline window.
fn tripped_at(t0: Instant) -> ProviderState {
    let mut s = ProviderState::from_gate(gate(None));
    assert!(
        s.record_failure(t0, LastOutcome::Http5xx),
        "premise: tripped"
    );
    s
}

fn circuit_wait(decision: &GateDecision) -> Option<Duration> {
    match decision {
        GateDecision::CircuitOpen { retry_in } => *retry_in,
        other => panic!("expected a breaker refusal, got {other:?}"),
    }
}

fn rpm_wait(decision: &GateDecision) -> Option<Duration> {
    match decision {
        GateDecision::RateLimited { retry_in } => *retry_in,
        other => panic!("expected an RPM refusal, got {other:?}"),
    }
}

#[test]
fn a_breaker_refusal_waits_out_the_rest_of_its_open_window() {
    // (row, elapsed since the trip, expected wait)
    let rows = [
        ("just tripped", Duration::ZERO, COOLDOWN),
        (
            "three seconds in",
            Duration::from_secs(3),
            Duration::from_secs(7),
        ),
        (
            "one millisecond left",
            COOLDOWN.saturating_sub(Duration::from_millis(1)),
            Duration::from_millis(1),
        ),
    ];
    for (row, elapsed, expected) in rows {
        // Arrange
        let t0 = Instant::now();
        let mut s = tripped_at(t0);

        // Act
        let decision = s.try_dispatch(t0 + elapsed);

        // Assert
        assert_eq!(circuit_wait(&decision), Some(expected), "{row}");
    }
}

#[test]
fn a_forced_window_sets_the_breaker_wait() {
    // Arrange: a forced window longer than the baseline.
    let t0 = Instant::now();
    let mut s = ProviderState::from_gate(gate(None));
    s.force_open(t0, Duration::from_secs(30));

    // Act
    let decision = s.try_dispatch(t0 + Duration::from_secs(12));

    // Assert
    assert_eq!(circuit_wait(&decision), Some(Duration::from_secs(18)));
}

#[test]
fn a_breaker_refusal_has_no_known_wait_once_its_window_lapsed() {
    // (row, kind refused after the lapse, whether a client re-test is in flight)
    let rows = [
        (
            "client behind an in-flight re-test",
            AdmissionKind::ClientDispatch,
            true,
        ),
        (
            "background probe deferring the re-test",
            AdmissionKind::BackgroundProbe,
            false,
        ),
    ];
    for (row, kind, retest_in_flight) in rows {
        // Arrange
        let t0 = Instant::now();
        let mut s = tripped_at(t0);
        let lapsed = t0 + COOLDOWN;
        if retest_in_flight {
            let claim = s.admit_under_park(lapsed, AdmissionKind::ClientDispatch, None);
            assert!(
                claim.claimed_half_open,
                "{row}: premise: the re-test is claimed"
            );
        }

        // Act
        let refused = s.admit_under_park(lapsed + Duration::from_secs(1), kind, None);

        // Assert
        assert_eq!(circuit_wait(&refused.decision), None, "{row}");
    }
}

#[test]
fn an_rpm_refusal_waits_for_the_next_whole_token() {
    // (row, rpm_limit, bucket level at the anchor, time since the anchor, expected wait)
    let rows = [
        ("empty, one per second", 60, 0.0, 0, 1_000),
        ("a quarter token held", 60, 0.25, 0, 750),
        ("half a token refilled since the anchor", 60, 0.0, 500, 500),
        ("empty, two per minute", 2, 0.0, 0, 30_000),
        ("a third of a millisecond rounds up", 60, 0.999_666, 0, 1),
    ];
    for (row, rpm_limit, level, since_anchor_ms, expected_ms) in rows {
        // Arrange
        let mut s = ProviderState::from_gate(gate(Some(rpm_limit)));
        let anchor = Instant::now();
        s.rpm_tokens = level;
        s.rpm_last_refill = anchor;

        // Act
        let decision = s.try_dispatch(anchor + Duration::from_millis(since_anchor_ms));

        // Assert
        assert_eq!(
            rpm_wait(&decision),
            Some(Duration::from_millis(expected_ms)),
            "{row}"
        );
    }
}

#[test]
fn a_zero_rpm_cap_refuses_with_no_known_wait() {
    // Arrange: a zero cap never refills.
    let mut s = ProviderState::from_gate(gate(Some(0)));

    // Act
    let decision = s.try_dispatch(Instant::now() + Duration::from_mins(5));

    // Assert
    assert_eq!(rpm_wait(&decision), None);
}
