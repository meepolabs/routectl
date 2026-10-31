//! The breaker epoch: which transitions advance it, and that a settlement
//! carrying an epoch that is no longer current changes nothing.

use super::*;
use crate::config::tuning::{BreakerThreshold, GateTuning};
use std::num::NonZeroU32;

const OUTCOME: LastOutcome = LastOutcome::Http5xx;
const COOLDOWN: Duration = Duration::from_secs(10);

fn gate(threshold: u32, rpm_limit: Option<u32>) -> GateTuning {
    GateTuning {
        threshold: NonZeroU32::new(threshold)
            .map_or(BreakerThreshold::Disabled, BreakerThreshold::After),
        cooldown: COOLDOWN,
        rpm_limit,
    }
}

fn closed(threshold: u32) -> ProviderState {
    ProviderState::from_gate(gate(threshold, None))
}

/// A tripped breaker at `t0` whose cooldown has lapsed by `t0 + COOLDOWN`.
fn open_at(t0: Instant) -> ProviderState {
    let mut s = closed(1);
    assert!(s.record_failure(t0, OUTCOME), "premise: tripped");
    s
}

/// A breaker whose half-open slot a client admission claimed at `t0 + COOLDOWN`.
fn half_open_at(t0: Instant) -> ProviderState {
    let mut s = open_at(t0);
    let claim = s.try_dispatch_admitting(t0 + COOLDOWN);
    assert!(claim.claimed_half_open, "premise: claimed");
    s
}

type Setup = fn(Instant) -> ProviderState;
type Step = fn(&mut ProviderState, Instant);

#[test]
fn the_epoch_advances_on_each_breaker_transition_and_nowhere_else() {
    let rows: [(&str, Setup, Step, u64); 9] = [
        (
            "closed -> open by debit",
            |_| closed(1),
            |s, t| {
                assert!(s.record_failure(t, OUTCOME));
            },
            1,
        ),
        (
            "closed -> open by force_open",
            |_| closed(5),
            |s, t| {
                s.force_open(t, COOLDOWN);
            },
            1,
        ),
        (
            "open -> half-open claim",
            open_at,
            |s, t| {
                assert!(s.try_dispatch_admitting(t + COOLDOWN).claimed_half_open);
            },
            1,
        ),
        (
            "half-open -> closed by success",
            half_open_at,
            |s, t| {
                assert!(s.record_success_closing(t + COOLDOWN, s.epoch()).closed);
            },
            1,
        ),
        (
            "half-open -> open by probe failure",
            half_open_at,
            |s, t| {
                assert!(
                    s.record_failure_debit(t + COOLDOWN, OUTCOME, s.epoch())
                        .opened
                );
            },
            1,
        ),
        (
            "rpm hand-back of a half-open-ready lane",
            |t0| {
                let mut s = ProviderState::from_gate(gate(1, Some(1)));
                assert_eq!(
                    s.try_dispatch(t0),
                    GateDecision::Allow,
                    "premise: token spent"
                );
                assert!(s.record_failure(t0, OUTCOME), "premise: tripped");
                s
            },
            |s, t| {
                let refused = s.try_dispatch_admitting(t + COOLDOWN);
                assert!(matches!(refused.decision, GateDecision::RateLimited { .. }));
                assert!(!s.half_open_probe_in_flight(), "nothing claimed");
            },
            0,
        ),
        (
            "closed admission",
            |_| closed(5),
            |s, t| {
                assert_eq!(s.try_dispatch(t), GateDecision::Allow);
            },
            0,
        ),
        (
            "closed success",
            |_| closed(5),
            |s, t| {
                assert!(!s.record_success_closing(t, s.epoch()).closed);
            },
            0,
        ),
        (
            "closed debit below the threshold",
            |_| closed(5),
            |s, t| {
                assert!(!s.record_failure(t, OUTCOME));
            },
            0,
        ),
    ];
    let mut mismatches = Vec::new();
    for (name, setup, step, want) in rows {
        // Arrange
        let t0 = Instant::now();
        let mut s = setup(t0);
        let before = s.epoch();

        // Act
        step(&mut s, t0);

        // Assert
        let advanced = s.epoch().wrapping_sub(before);
        if advanced != want {
            mismatches.push(format!("{name}: advanced by {advanced}, want {want}"));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// Everything a settlement can change, read without mutating.
fn observed(s: &ProviderState, now: Instant) -> (ProviderGateStatus, u32, u64) {
    (
        s.gate_status(now),
        s.consecutive_failures_for_tests(),
        s.epoch(),
    )
}

type Settle = fn(&mut ProviderState, Instant, u64);

#[test]
fn a_settlement_under_a_superseded_epoch_changes_nothing() {
    let rows: [(&str, Settle); 4] = [
        ("success", |s, now, epoch| {
            s.record_success_closing(now, epoch);
        }),
        ("failure debit", |s, now, epoch| {
            s.record_failure_debit(now, LastOutcome::Timeout, epoch);
        }),
        ("force open", |s, now, epoch| {
            s.force_open_in_epoch(now, Duration::from_hours(1), epoch);
        }),
        ("probe release", |s, _, epoch| {
            s.release_probe_slot_in_epoch(epoch)
        }),
    ];
    for (name, settle) in rows {
        // Arrange: a first claim settles and a later one holds the slot, so
        // the first claim's epoch is superseded.
        let t0 = Instant::now();
        let mut s = half_open_at(t0);
        let superseded = s.epoch();
        let t1 = t0 + COOLDOWN;
        assert!(
            s.record_failure_debit(t1, OUTCOME, superseded).opened,
            "premise"
        );
        let newer = s.try_dispatch_admitting(t1 + COOLDOWN);
        assert!(newer.claimed_half_open, "premise: {name}: newer claim");
        let now = t1 + COOLDOWN + Duration::from_secs(1);
        let before = observed(&s, now);

        // Act
        settle(&mut s, now, superseded);

        // Assert
        assert_eq!(
            observed(&s, now),
            before,
            "{name}: stale settlement applied"
        );
        // Positive control: the same settlement under the newer claim's
        // epoch is observable, so the snapshot can see this change.
        settle(&mut s, now, newer.epoch);
        assert_ne!(observed(&s, now), before, "{name}: control saw no change");
    }
}

#[test]
fn concurrent_closed_admissions_debit_toward_one_trip() {
    // Arrange: two admissions through the same closed breaker.
    let mut s = closed(2);
    let t0 = Instant::now();
    let first = s.try_dispatch_admitting(t0);
    let second = s.try_dispatch_admitting(t0);
    assert_eq!(first.epoch, second.epoch, "premise: one shared epoch");

    // Act
    let first_debit = s.record_failure_debit(t0, OUTCOME, first.epoch);
    let second_debit = s.record_failure_debit(t0, OUTCOME, second.epoch);

    // Assert
    assert!(!first_debit.tripped, "one failure is below the threshold");
    assert!(second_debit.opened, "the second shares the epoch and trips");
    assert!(matches!(
        s.try_dispatch(t0),
        GateDecision::CircuitOpen { .. }
    ));
}
