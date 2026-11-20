//! The remaining outage a capacity snapshot carries: the rest of a live park
//! or of an open breaker window, and unknown whenever a re-test decides it,
//! read without touching the token bucket or the half-open slot.

use super::*;
use crate::config::{Config, resolve_tuning};
use routectl_core::UpstreamReason;

const OUTCOME: LastOutcome = LastOutcome::Http5xx;
const FORCED: Duration = Duration::from_mins(10);
/// The shipped baseline breaker cooldown.
const BASELINE: Duration = Duration::from_mins(1);

/// A slot seeded with the shipped defaults (five failures, 60 s cooldown).
fn default_slot() -> ProviderState {
    ProviderState::from_gate(resolve_tuning(&Config::default(), None, None).gate())
}

fn limited_slot(rpm_limit: u32) -> ProviderState {
    ProviderState::new(&ProviderRuntimePolicy {
        rpm_limit: Some(rpm_limit),
        ..Default::default()
    })
}

/// A default slot whose breaker opened on its baseline cooldown at `t0`.
fn baseline_open_at(t0: Instant) -> ProviderState {
    let mut s = default_slot();
    let mut opened = false;
    for _ in 0..5 {
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow, "premise: admitted");
        opened |= s.record_failure(t0, OUTCOME);
    }
    assert!(opened, "premise: the fifth failure opens the breaker");
    s
}

fn park_until(until: Instant) -> Option<CredentialPark> {
    Some(CredentialPark {
        reason: UpstreamReason::CredentialExpired,
        until,
    })
}

const fn unusable_since(since: Instant) -> Option<Unusable> {
    Some(Unusable {
        cause: UnusableCause::Revoked,
        since,
    })
}

#[test]
fn a_forced_open_reports_the_rest_of_its_forced_window() {
    // Arrange
    let mut s = default_slot();
    let t0 = Instant::now();
    s.force_open(t0, FORCED);
    let elapsed = Duration::from_secs(90);

    // Act
    let snapshot = s.capacity_snapshot(t0 + elapsed);

    // Assert
    assert_eq!(snapshot.reason(), Some(EligibilityReason::BreakerOpen));
    assert_eq!(snapshot.outage_remaining, FORCED.checked_sub(elapsed));
}

#[test]
fn a_baseline_open_reports_the_rest_of_its_default_cooldown() {
    // (row, elapsed since the trip, expected remaining)
    let rows = [
        ("just tripped", Duration::ZERO, BASELINE),
        (
            "twenty seconds in",
            Duration::from_secs(20),
            Duration::from_secs(40),
        ),
        (
            "one millisecond left",
            Duration::from_millis(59_999),
            Duration::from_millis(1),
        ),
    ];
    for (row, elapsed, expected) in rows {
        // Arrange
        let t0 = Instant::now();
        let s = baseline_open_at(t0);

        // Act
        let snapshot = s.capacity_snapshot(t0 + elapsed);

        // Assert
        assert_eq!(
            snapshot.reason(),
            Some(EligibilityReason::BreakerOpen),
            "{row}"
        );
        assert_eq!(snapshot.outage_remaining, Some(expected), "{row}");
    }
}

#[test]
fn a_half_open_ready_breaker_reports_no_outage() {
    // Arrange
    let t0 = Instant::now();
    let s = baseline_open_at(t0);

    // Act
    let snapshot = s.capacity_snapshot(t0 + BASELINE);

    // Assert
    assert_eq!(snapshot.circuit, CircuitPhase::HalfOpenReady);
    assert_eq!(snapshot.outage_remaining, None);
}

#[test]
fn a_breaker_probe_in_flight_reports_no_outage() {
    // Arrange
    let t0 = Instant::now();
    let mut s = baseline_open_at(t0);
    let lapsed = t0 + BASELINE;
    assert!(
        s.try_dispatch_admitting(lapsed).claimed_half_open,
        "premise"
    );

    // Act
    let snapshot = s.capacity_snapshot(lapsed);

    // Assert
    assert_eq!(snapshot.reason(), Some(EligibilityReason::BreakerOpen));
    assert_eq!(snapshot.outage_remaining, None);
}

#[test]
fn a_live_park_reports_the_time_until_it_lapses() {
    // Arrange: the breaker is open too, so the park must win by priority.
    let mut s = default_slot();
    let t0 = Instant::now();
    s.force_open(t0, FORCED);
    let until = t0 + CREDENTIAL_PARK_BACKSTOP;
    let now = t0 + Duration::from_secs(5);

    // Act
    let snapshot = s.capacity_snapshot_under(now, park_until(until), None);

    // Assert
    assert_eq!(snapshot.reason(), Some(EligibilityReason::AccountParked));
    assert_eq!(snapshot.outage_remaining, Some(until - now));
}

#[test]
fn a_park_whose_retest_is_in_flight_reports_no_outage() {
    // Arrange
    let mut s = default_slot();
    let t0 = Instant::now();
    let park = park_until(t0);
    let admission = s.admit_under_park(t0, AdmissionKind::ClientDispatch, park);
    assert!(
        admission.claimed_half_open,
        "premise: the re-test is claimed"
    );

    // Act
    let snapshot = s.capacity_snapshot_under(t0, park, None);

    // Assert
    assert_eq!(snapshot.reason(), Some(EligibilityReason::AccountParked));
    assert_eq!(snapshot.outage_remaining, None);
}

#[test]
fn an_unusable_credential_reports_no_outage_over_a_live_park_and_open_breaker() {
    // Arrange
    let mut s = default_slot();
    let t0 = Instant::now();
    s.force_open(t0, FORCED);
    let park = park_until(t0 + CREDENTIAL_PARK_BACKSTOP);

    // Act
    let snapshot = s.capacity_snapshot_under(t0, park, unusable_since(t0));

    // Assert
    assert_eq!(
        snapshot.reason(),
        Some(EligibilityReason::CredentialUnusable)
    );
    assert_eq!(snapshot.outage_remaining, None);
}

#[test]
fn an_untimed_or_absent_refusal_reports_no_outage() {
    // Arrange
    let t0 = Instant::now();
    let closed = default_slot();
    let mut drained = limited_slot(1);
    assert_eq!(drained.try_dispatch(t0), GateDecision::Allow, "premise");

    // Act
    let closed_snapshot = closed.capacity_snapshot(t0);
    let drained_snapshot = drained.capacity_snapshot(t0);

    // Assert
    assert_eq!(closed_snapshot.reason(), None);
    assert_eq!(closed_snapshot.outage_remaining, None);
    assert_eq!(
        drained_snapshot.reason(),
        Some(EligibilityReason::RateLimited)
    );
    assert_eq!(drained_snapshot.outage_remaining, None);
}

#[test]
fn repeated_snapshots_leave_the_bucket_and_the_probe_slot_untouched() {
    // Arrange: a limited bucket below full, on a breaker whose window has
    // lapsed so the next client dispatch would claim the probe slot.
    let t0 = Instant::now();
    let mut s = limited_slot(10);
    assert_eq!(
        s.try_dispatch(t0),
        GateDecision::Allow,
        "premise: one token spent"
    );
    s.force_open(t0, Duration::from_secs(1));
    let lapsed = t0 + Duration::from_secs(1);
    let (tokens, last_refill, epoch) = (s.rpm_tokens, s.rpm_last_refill, s.epoch());
    let park = park_until(lapsed);

    // Act
    for step in 0..8u64 {
        let at = lapsed + Duration::from_secs(step);
        let snapshot = s.capacity_snapshot_under(at, park, None);
        assert_eq!(snapshot.circuit, CircuitPhase::HalfOpenReady, "premise");
    }

    // Assert
    assert_eq!(s.rpm_tokens, tokens, "the bucket level is not refilled");
    assert_eq!(s.rpm_last_refill, last_refill, "the refill anchor holds");
    assert!(!s.half_open_probe_in_flight(), "no probe slot was claimed");
    assert_eq!(s.epoch(), epoch, "no claim advanced the epoch");
    assert!(
        s.try_dispatch_admitting(lapsed).claimed_half_open,
        "the slot is still free for the next real dispatch"
    );
}
