//! The move predicate: which outages take a pinned conversation off its seat.
//!
//! Every snapshot is produced by a real `ProviderState` under the park and
//! unusable mark admission would see, so a row cannot describe a state the
//! gate never reports.

use super::*;

use std::time::Instant;

use routectl_core::UpstreamReason;
use routectl_core::failure_class::LastOutcome;

use crate::config::{Config, ProviderRuntimePolicy, resolve_tuning};
use crate::runtime_state::{
    AdmissionKind, CredentialPark, GateDecision, ProviderState, Unusable, UnusableCause,
};

const ONE_MS: Duration = Duration::from_millis(1);
/// One millisecond less than [`MOVE_FLOOR`].
const JUST_SHORT: Duration = Duration::from_millis(299_999);
/// How far into a ten-minute open exactly [`MOVE_FLOOR`] is left.
const FIVE_MIN: Duration = Duration::from_mins(5);

/// A slot on the shipped defaults: five failures open the breaker for 60 s.
fn default_slot() -> ProviderState {
    ProviderState::from_gate(resolve_tuning(&Config::default(), None, None).gate())
}

fn park(until: Instant) -> Option<CredentialPark> {
    Some(CredentialPark {
        reason: UpstreamReason::CredentialExpired,
        until,
    })
}

/// A slot whose breaker opened on its baseline cooldown at `t0`.
fn baseline_open_at(t0: Instant) -> ProviderState {
    let mut slot = default_slot();
    for _ in 0..5 {
        assert_eq!(slot.try_dispatch(t0), GateDecision::Allow, "premise");
        slot.record_failure(t0, LastOutcome::Http5xx);
    }
    slot
}

/// A slot whose breaker an upstream reset hint opened for `window` at `t0`.
fn forced_open_at(t0: Instant, window: Duration) -> ProviderState {
    let mut slot = default_slot();
    slot.force_open(t0, window);
    slot
}

/// One predicate case: its name, the home's snapshot, the expected trigger.
type MoveRow = (&'static str, CapacitySnapshot, Option<EligibilityReason>);

fn rows(t0: Instant) -> Vec<MoveRow> {
    let unusable = Some(Unusable {
        cause: UnusableCause::Revoked,
        since: t0,
    });
    let mut retesting = default_slot();
    assert!(
        retesting
            .admit_under_park(t0, AdmissionKind::ClientDispatch, park(t0))
            .claimed_half_open,
        "premise: the park's re-test is claimed"
    );
    let mut probing = baseline_open_at(t0);
    assert!(
        probing
            .try_dispatch_admitting(t0 + Duration::from_mins(1))
            .claimed_half_open,
        "premise: the breaker's probe is claimed"
    );
    let mut drained = ProviderState::new(&ProviderRuntimePolicy {
        rpm_limit: Some(1),
        ..Default::default()
    });
    assert_eq!(drained.try_dispatch(t0), GateDecision::Allow, "premise");
    let ten_min = Duration::from_mins(10);
    vec![
        (
            "an unusable credential moves, though its end is unknown",
            default_slot().capacity_snapshot_under(t0, None, unusable),
            Some(EligibilityReason::CredentialUnusable),
        ),
        (
            "an unusable credential moves over a short park",
            default_slot().capacity_snapshot_under(t0, park(t0 + ONE_MS), unusable),
            Some(EligibilityReason::CredentialUnusable),
        ),
        (
            "a ten-minute park moves",
            default_slot().capacity_snapshot_under(t0, park(t0 + ten_min), None),
            Some(EligibilityReason::AccountParked),
        ),
        (
            "a park with exactly the floor left moves",
            default_slot().capacity_snapshot_under(t0, park(t0 + MOVE_FLOOR), None),
            Some(EligibilityReason::AccountParked),
        ),
        (
            "a park one millisecond short of the floor stays",
            default_slot().capacity_snapshot_under(t0, park(t0 + JUST_SHORT), None),
            None,
        ),
        (
            "a park whose re-test is in flight stays",
            retesting.capacity_snapshot_under(t0, park(t0), None),
            None,
        ),
        (
            "a ten-minute open moves",
            forced_open_at(t0, ten_min).capacity_snapshot(t0),
            Some(EligibilityReason::BreakerOpen),
        ),
        (
            "an open with exactly the floor left moves",
            forced_open_at(t0, ten_min).capacity_snapshot(t0 + FIVE_MIN),
            Some(EligibilityReason::BreakerOpen),
        ),
        (
            "an open one millisecond short of the floor stays",
            forced_open_at(t0, ten_min).capacity_snapshot(t0 + FIVE_MIN + ONE_MS),
            None,
        ),
        (
            "the 60 s baseline open stays",
            baseline_open_at(t0).capacity_snapshot(t0),
            None,
        ),
        (
            "a half-open-ready breaker stays",
            baseline_open_at(t0).capacity_snapshot(t0 + Duration::from_mins(1)),
            None,
        ),
        (
            "a breaker probe in flight stays",
            probing.capacity_snapshot(t0 + Duration::from_mins(1)),
            None,
        ),
        (
            "an empty RPM bucket stays",
            drained.capacity_snapshot(t0),
            None,
        ),
        (
            "a healthy seat stays",
            default_slot().capacity_snapshot(t0),
            None,
        ),
    ]
}

#[test]
fn a_home_moves_only_for_an_unusable_credential_or_an_outage_of_the_floor_or_more() {
    for (name, snapshot, expected) in rows(Instant::now()) {
        assert_eq!(move_trigger(&snapshot), expected, "row: {name}");
    }
}
