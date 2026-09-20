use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::*;
use crate::config::CANARY_INTERVAL;
use crate::field_capability::field_capability_key;

const ANTHROPIC: &str = "anthropic-api";
const GROUNDED_PATH: &str = "thinking.enabled.display";

fn key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(state_key, GROUNDED_PATH, ANTHROPIC).expect("a qualified path mints a key")
}

// Only exercised through `field_capability_key` to keep the const path live
// and catch a drift between the two modules' path grammar at compile time.
#[test]
fn field_capability_key_still_mints_the_fixture_path() {
    assert!(field_capability_key(GROUNDED_PATH).is_some());
}

#[test]
fn confirmation_absent_before_any_commit() {
    let registry = FieldCanaryRegistry::new();
    assert!(registry.snapshot(&key("model-a")).is_none());
}

#[test]
fn acknowledge_confirmation_reconciles_to_the_acknowledged_count() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    assert_eq!(registry.acknowledge_confirmation(&k, 1, 1), 1);
    assert_eq!(registry.acknowledge_confirmation(&k, 1, 2), 2);

    let snap = registry.snapshot(&k).expect("resident after a commit");
    assert_eq!(snap.confirmations, 2);
    assert_eq!(snap.incarnation, 1);
}

#[test]
fn confirmation_survives_a_reload_share() {
    // A hot reload shares the SAME Arc, never a fresh registry -- proven by
    // pointer identity, the only way "survives reload" is falsifiable rather
    // than an assertion that would pass against two independent registries
    // that happen to agree by coincidence.
    let registry = Arc::new(FieldCanaryRegistry::new());
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 3);

    let reloaded = Arc::clone(&registry);
    let snap = reloaded.snapshot(&k).expect("resident after reload share");
    assert_eq!(snap.confirmations, 3);
}

#[test]
fn a_new_incarnation_discards_the_prior_confirmation_count() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 5);

    // Incarnation 2 names a DIFFERENT verdict lifecycle for the same
    // identity (cleared and re-learned); its confirmation count starts
    // fresh rather than continuing from incarnation 1's tally.
    let reconciled = registry.acknowledge_confirmation(&k, 2, 1);
    assert_eq!(reconciled, 1);
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.incarnation, 2);
}

#[test]
fn reset_clears_all_resident_state_for_the_identity() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 4);
    let _guard = registry.begin_modified_request(&k, 1);

    registry.reset(&k);

    assert!(registry.snapshot(&k).is_none());
}

#[test]
fn cadence_counts_down_from_the_canary_interval_and_trips_once() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let mut trips = 0;
    for _ in 0..CANARY_INTERVAL {
        if registry.tick_cadence(&k, 1) {
            trips += 1;
        }
    }
    assert_eq!(trips, 1, "exactly one trip per full cadence cycle");

    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.cadence, CANARY_INTERVAL,
        "the trip resets the countdown"
    );
}

#[test]
fn cadence_resets_on_a_new_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    for _ in 0..(CANARY_INTERVAL - 1) {
        registry.tick_cadence(&k, 1);
    }
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.cadence, 1, "one tick away from tripping");

    // A new incarnation reseeds the countdown rather than inheriting the
    // near-trip state of the superseded verdict.
    registry.tick_cadence(&k, 2);
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.cadence, CANARY_INTERVAL - 1);
    assert_eq!(snap.incarnation, 2);
}

#[test]
fn modified_request_guard_increments_and_decrements_on_drop() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let guard = registry.begin_modified_request(&k, 1);
    assert_eq!(registry.snapshot(&k).expect("resident").outstanding, 1);

    drop(guard);
    assert_eq!(registry.snapshot(&k).expect("resident").outstanding, 0);
}

#[test]
fn modified_request_guard_decrements_on_an_early_return() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    fn early_return(registry: &FieldCanaryRegistry, k: &FieldVerdictKey) -> Result<(), ()> {
        let _guard = registry.begin_modified_request(k, 1);
        Err(())
    }

    let _ = early_return(&registry, &k);
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        0,
        "the guard's Drop must decrement even on an early `?`-style return"
    );
}

#[test]
fn modified_request_guard_reseeds_on_a_new_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 9);

    let guard = registry.begin_modified_request(&k, 2);
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.incarnation, 2,
        "a new incarnation replaces the resident state wholesale"
    );
    assert_eq!(
        snap.confirmations, 0,
        "the superseded incarnation's confirmation count must not leak into the fresh one"
    );
    assert_eq!(snap.outstanding, 1);
    drop(guard);
}

#[test]
fn modified_request_guard_preserves_state_at_the_same_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 6);

    let guard = registry.begin_modified_request(&k, 1);
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.confirmations, 6,
        "the same incarnation's confirmation count must survive begin_modified_request"
    );
    drop(guard);
}

#[test]
fn modified_request_outstanding_saturates_rather_than_wraps() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    // Drive the counter to its ceiling without holding u64::MAX guards.
    registry
        .states
        .lock()
        .insert(k.clone(), CanaryState::fresh(1, 0));
    if let Some(entry) = registry.states.lock().get_mut(&k) {
        entry.outstanding = u64::MAX;
    }

    let guard = registry.begin_modified_request(&k, 1);
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        u64::MAX,
        "saturating_add must not wrap past u64::MAX"
    );
    drop(guard);
}

#[test]
fn claim_canary_admits_exactly_one_slot_at_a_time() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let first = registry.claim_canary(&k, 1);
    assert!(first.is_some());
    assert!(
        registry.claim_canary(&k, 1).is_none(),
        "a second concurrent claim on the same identity must be refused"
    );

    drop(first);
    assert!(
        registry.claim_canary(&k, 1).is_some(),
        "dropping the first guard must release the slot for a fresh claim"
    );
}

#[test]
fn dropped_canary_guard_releases_the_claim_without_recording_an_outcome() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let guard = registry.claim_canary(&k, 1).expect("first claim admitted");
    drop(guard);

    assert!(!registry.snapshot(&k).expect("resident").canary_claimed);
    assert!(
        registry
            .snapshot(&k)
            .expect("resident")
            .last_outcome
            .is_none()
    );
}

#[test]
fn settled_canary_guard_releases_the_claim_and_records_the_outcome() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let guard = registry.claim_canary(&k, 1).expect("claim admitted");
    guard.settle(CanaryOutcome::Confirmed);

    let snap = registry.snapshot(&k).expect("resident");
    assert!(!snap.canary_claimed);
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Confirmed));
}

/// Slot-RELEASE behavior against a live claim is not this test's contract and is
/// owned by the dedicated tests that hold one at the current incarnation (see
/// `field_verdict_tests`' stale-disproof case); here the fresh incarnation was
/// never claimed, so the flag below only witnesses that the settlement did not
/// invent a claim.
#[test]
fn a_stale_settlement_leaves_the_fresh_incarnation_unclaimed_and_unmutated() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 7);

    let guard = registry.claim_canary(&k, 1).expect("claim admitted");
    // The identity moves to a new incarnation while the canary is still
    // in flight -- e.g. the verdict was cleared and re-learned mid-probe.
    registry.acknowledge_confirmation(&k, 2, 1);

    guard.settle(CanaryOutcome::Regressed);

    let snap = registry.snapshot(&k).expect("resident");
    assert!(
        !snap.canary_claimed,
        "the fresh incarnation starts unclaimed, and the stale settlement neither \
         claims it nor is credited with releasing it"
    );
    assert_eq!(
        snap.incarnation, 2,
        "the settlement must not roll the resident state back to the stale incarnation"
    );
    assert_eq!(
        snap.confirmations, 1,
        "a stale settlement leaves the current incarnation's confirmation count untouched"
    );
    assert_eq!(
        snap.last_outcome, None,
        "a stale settlement records no outcome"
    );
}

#[test]
fn explicit_release_matches_the_implicit_drop_path() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let guard = registry.claim_canary(&k, 1).expect("claim admitted");
    guard.release();

    let snap = registry.snapshot(&k).expect("resident");
    assert!(!snap.canary_claimed);
    assert!(snap.last_outcome.is_none());
}

/// Regression: a claim taken at a superseded incarnation must not free the slot
/// a LIVE canary of the current incarnation holds. Releasing unconditionally
/// admits two concurrent canaries for one identity -- each restoring the field
/// under test -- so the single-flight guarantee that makes a canary's verdict
/// attributable would hold only while no reload raced it.
///
/// Mutation check: drop the `entry.incarnation != incarnation` guard from
/// `release_canary_claim` and this goes red on the reclaim assertion.
#[test]
fn a_stale_guard_drop_cannot_release_a_live_claim_of_a_newer_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let stale = registry.claim_canary(&k, 1).expect("stale claim admitted");
    // The identity moves on (a reload re-learned the verdict) and the canary of
    // the new incarnation takes the slot while the stale request is still out.
    let live = registry.claim_canary(&k, 2).expect("live claim admitted");

    drop(stale);

    assert!(
        registry.snapshot(&k).expect("resident").canary_claimed,
        "the live canary still holds the slot after the stale guard drops"
    );
    assert!(
        registry.claim_canary(&k, 2).is_none(),
        "so no second canary is admitted for the identity while the live one is in flight"
    );

    // And the live guard still owns its own release.
    drop(live);
    assert!(
        registry.claim_canary(&k, 2).is_some(),
        "the slot frees once the claim that actually holds it drops"
    );
}

/// The settlement half of the same invariant: a stale settlement must not free
/// the live slot either, and must still record no outcome.
#[test]
fn a_stale_settlement_cannot_release_a_live_claim_of_a_newer_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let stale = registry.claim_canary(&k, 1).expect("stale claim admitted");
    let _live = registry.claim_canary(&k, 2).expect("live claim admitted");

    stale.settle(CanaryOutcome::Regressed);

    let snap = registry.snapshot(&k).expect("resident");
    assert!(
        snap.canary_claimed,
        "the live canary keeps the slot through a stale settlement"
    );
    assert_eq!(
        snap.last_outcome, None,
        "and the stale settlement records no outcome against the new incarnation"
    );
}

/// Mutation check: delete the `Greater` arm's reseed in `admit_incarnation`
/// (make it return `Newer` without replacing the entry) and this test goes red,
/// because a canary claim and non-default cadence left over from the superseded
/// incarnation would then leak into the fresh one instead of being discarded.
/// (The `confirmations` field alone cannot pin this guard:
/// `acknowledge_confirmation` always overwrites it unconditionally, guard or
/// not.)
#[test]
fn quorum_reseed_guard_is_load_bearing() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.tick_cadence(&k, 1);
    let stale_claim = registry.claim_canary(&k, 1).expect("claim admitted");

    registry.acknowledge_confirmation(&k, 2, 1);

    let snap = registry.snapshot(&k).expect("resident");
    assert!(
        !snap.canary_claimed,
        "a fresh incarnation must not inherit a claim held by the superseded one"
    );
    assert_eq!(
        snap.cadence, CANARY_INTERVAL,
        "a fresh incarnation must not inherit the superseded one's cadence countdown"
    );
    drop(stale_claim);
}

/// Mutation check: comment out the claim-refusal branch in `claim_canary`
/// (`if entry.canary_claimed { return None; }`) and this test goes red,
/// because a second concurrent claim would then be silently admitted.
#[test]
fn claim_refusal_guard_is_load_bearing() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    let _first = registry.claim_canary(&k, 1).expect("first claim admitted");
    assert!(registry.claim_canary(&k, 1).is_none());
}

/// Mutation check: drop the `if entry.incarnation == incarnation` guard in
/// `settle_canary` (always record the outcome) and this test goes red,
/// because a stale settlement would then overwrite `last_outcome` for the
/// live incarnation.
#[test]
fn stale_settlement_rollback_guard_is_load_bearing() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    let guard = registry.claim_canary(&k, 1).expect("claim admitted");
    registry.acknowledge_confirmation(&k, 2, 1);
    guard.settle(CanaryOutcome::Regressed);
    assert_eq!(registry.snapshot(&k).expect("resident").last_outcome, None);
}

// ---------------------------------------------------------------------------
// Settlement routing: each outcome moves exactly the state it owns
// ---------------------------------------------------------------------------

#[test]
fn the_canary_cadence_is_one_hundred_eligible_requests() {
    // Mutation check for the cadence constant: change 100 to 99 and this goes
    // red on both halves -- the 99th tick would come due, and the 100th would
    // not. Asserted as a COUNT of ticks rather than against the constant itself,
    // which would be a tautology while code and literal agree.
    //
    // Driven as a real caller drives it: a due tick CLAIMS, and the claim is
    // what consumes the trip (dueness is sticky otherwise -- see
    // `an_unclaimed_due_interval_stays_due_until_a_claim_consumes_it`). Reading
    // the tick alone without claiming would report every request after the first
    // trip, which is the sticky flag doing its job rather than a cadence defect.
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let mut due_on: Vec<u32> = Vec::new();
    for tick in 1..=200 {
        if registry.tick_cadence(&k, 1) {
            due_on.push(tick);
            // Settling immediately releases the slot, so the next interval's
            // claim is admitted -- the shape of a canary that completes.
            registry
                .claim_canary(&k, 1)
                .expect("the slot is free, so a due interval claims")
                .settle(CanaryOutcome::Inconclusive);
        }
    }

    assert_eq!(
        due_on,
        vec![100, 200],
        "a canary is due on every hundredth eligible request, not the 99th or the 101st",
    );
}

#[test]
fn an_unclaimed_due_interval_stays_due_until_a_claim_consumes_it() {
    // The STICKY half of dueness, and the reason it exists: a trip whose claim
    // is unavailable must not be spent. Without this, an identity whose slot is
    // busy at the moment it comes due -- an earlier canary still in flight, or a
    // request whose sole settlement slot a sibling row owns -- would wait another
    // full interval, and for a row that is consistently unable to claim, forever.
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    // A claim held by a canary still in flight, so the trip below cannot claim.
    let in_flight = registry.claim_canary(&k, 1).expect("first claim admitted");

    // Arrange -- reach the trip.
    for _ in 1..CANARY_INTERVAL {
        assert!(
            !registry.tick_cadence(&k, 1),
            "not due before the interval elapses",
        );
    }
    assert!(
        registry.tick_cadence(&k, 1),
        "the interval elapsed, so the identity is due",
    );
    assert!(
        registry.claim_canary(&k, 1).is_none(),
        "premise: the slot is held, so this due interval cannot be claimed",
    );

    // Act + assert -- it stays due on every subsequent request, rather than
    // waiting out another interval.
    for request in 1..=5 {
        assert!(
            registry.tick_cadence(&k, 1),
            "request {request} after an unclaimed trip must still find the identity due",
        );
    }

    // And the FIRST claim that succeeds consumes it: the flag clears, so the
    // identity is not permanently due either.
    in_flight.settle(CanaryOutcome::Inconclusive);
    let claimed = registry
        .claim_canary(&k, 1)
        .expect("the released slot admits the pending due interval");
    assert!(
        !registry.snapshot(&k).expect("resident").due,
        "a successful claim consumes the trip, so the identity is no longer due",
    );
    drop(claimed);
    assert!(
        !registry.tick_cadence(&k, 1),
        "and the next request is back on a fresh interval rather than instantly due",
    );
}

#[test]
fn a_confirmed_settlement_resets_the_cadence_and_the_modified_tally() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    // Arrange -- 40 modified requests since the last confirmation, and a
    // cadence part-way through its cycle.
    for _ in 0..40 {
        drop(registry.begin_modified_request(&k, 1));
        registry.tick_cadence(&k, 1);
    }
    let claim = registry.claim_canary(&k, 1).expect("claim admitted");

    // Act
    claim.settle(CanaryOutcome::Confirmed);

    // Assert
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Confirmed));
    assert_eq!(
        snap.cadence, CANARY_INTERVAL,
        "a confirmation restarts the full interval",
    );
    assert_eq!(
        snap.modified_since_confirmation, 0,
        "a confirmation resets the wrong-repair tally: those 40 requests are now vouched for",
    );
    assert_eq!(
        registry.disproved_requests_total(),
        0,
        "a confirmation transfers nothing into the lifetime disproved total",
    );
    assert!(
        !snap.preflight_suspended,
        "a confirmed verdict keeps acting pre-flight",
    );
}

#[test]
fn a_regressed_settlement_transfers_the_modified_tally_into_the_lifetime_total() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    for _ in 0..7 {
        drop(registry.begin_modified_request(&k, 1));
    }
    let claim = registry.claim_canary(&k, 1).expect("claim admitted");

    // Act -- the unrepaired canary SUCCEEDED, so the verdict was wrong and
    // every request repaired since the last confirmation was affected by it.
    claim.settle(CanaryOutcome::Regressed);

    // Assert
    assert_eq!(
        registry.disproved_requests_total(),
        7,
        "the lifetime alarm counts REQUESTS affected by a repair later disproved",
    );
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.modified_since_confirmation, 0,
        "the tally moves rather than being counted twice",
    );
    assert!(
        snap.preflight_suspended,
        "pre-flight is suspended for the identity at once, before the durable clear",
    );
    assert!(!snap.canary_claimed, "and the claim is released");
}

#[test]
fn the_lifetime_disproved_total_is_monotonic_across_incarnations() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    for _ in 0..3 {
        drop(registry.begin_modified_request(&k, 1));
    }
    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);
    registry.reset(&k);

    // A fresh verdict lifecycle for the same identity, disproved again.
    for _ in 0..5 {
        drop(registry.begin_modified_request(&k, 2));
    }
    registry
        .claim_canary(&k, 2)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);

    assert_eq!(
        registry.disproved_requests_total(),
        8,
        "the alarm is a lifetime total: a cleared verdict does not zero it",
    );
}

#[test]
fn an_inconclusive_settlement_reschedules_and_leaves_the_tally_alone() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    for _ in 0..4 {
        drop(registry.begin_modified_request(&k, 1));
        registry.tick_cadence(&k, 1);
    }
    let claim = registry.claim_canary(&k, 1).expect("claim admitted");

    // Act -- an unrelated failure proved nothing either way.
    claim.settle(CanaryOutcome::Inconclusive);

    // Assert
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Inconclusive));
    assert_eq!(
        snap.cadence, CANARY_INTERVAL,
        "another bounded interval is scheduled rather than retrying at once",
    );
    assert_eq!(
        snap.modified_since_confirmation, 4,
        "an inconclusive outcome vouches for nothing, so the tally stands",
    );
    assert_eq!(
        registry.disproved_requests_total(),
        0,
        "and nothing is charged to the alarm",
    );
    assert!(
        !snap.preflight_suspended,
        "an inconclusive outcome leaves the verdict acting",
    );
}

#[test]
fn a_stale_regressed_settlement_charges_the_alarm_nothing() {
    // A settlement arriving after the identity moved to a new incarnation
    // describes a verdict that no longer exists. Charging the alarm from it
    // would attribute the NEW incarnation's repaired requests to a disproof
    // of the OLD one.
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    let claim = registry.claim_canary(&k, 1).expect("claim admitted");
    registry.acknowledge_confirmation(&k, 2, 1);
    // The fresh incarnation needs exposure of its OWN for the alarm assertion to
    // be able to fail: any tally raised against the incarnation the reseed
    // replaced is gone with it, so a stale settlement would charge zero either
    // way and the guard this test exists for could be deleted unnoticed.
    drop(
        registry
            .begin_modified_request(&k, 2)
            .expect("accounts at the reseeded incarnation"),
    );
    assert_eq!(
        registry
            .snapshot(&k)
            .expect("resident")
            .modified_since_confirmation,
        1,
        "fixture premise: the live incarnation has exposure a stale disproof \
         would wrongly transfer",
    );

    claim.settle(CanaryOutcome::Regressed);

    assert_eq!(
        registry.disproved_requests_total(),
        0,
        "the stale settlement charges the live incarnation's exposure to nothing",
    );
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.modified_since_confirmation, 1,
        "and leaves it resident for whatever settles the CURRENT lifecycle",
    );
    assert!(
        !snap.preflight_suspended,
        "a stale settlement must not suspend the live incarnation",
    );
    assert_eq!(snap.last_outcome, None);
}

#[test]
fn the_modified_tally_counts_requests_and_does_not_fall_when_a_guard_drops() {
    // The two counts answer different questions and must not be conflated: the
    // in-flight count says how many requests are applying the repair RIGHT NOW,
    // the tally says how many have applied it since the last confirmation. A
    // tally that fell on Drop would report roughly zero affected requests for
    // every verdict, which is exactly the alarm's failure mode.
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");

    let first = registry.begin_modified_request(&k, 1);
    let second = registry.begin_modified_request(&k, 1);
    let mid = registry.snapshot(&k).expect("resident");
    assert_eq!((mid.outstanding, mid.modified_since_confirmation), (2, 2));

    drop(first);
    drop(second);

    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.outstanding, 0, "no request is applying the repair now");
    assert_eq!(
        snap.modified_since_confirmation, 2,
        "two requests were nonetheless modified since the last confirmation",
    );
}

#[test]
fn the_modified_tally_resets_on_a_new_incarnation() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    drop(registry.begin_modified_request(&k, 1));

    drop(registry.begin_modified_request(&k, 2));

    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(
        snap.modified_since_confirmation, 1,
        "a fresh lifecycle must not inherit the superseded one's affected-request tally",
    );
}

#[test]
fn the_modified_tally_saturates_rather_than_wrapping() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry
        .states
        .lock()
        .insert(k.clone(), CanaryState::fresh(1, 0));
    if let Some(entry) = registry.states.lock().get_mut(&k) {
        entry.modified_since_confirmation = u64::MAX;
    }

    drop(registry.begin_modified_request(&k, 1));

    assert_eq!(
        registry
            .snapshot(&k)
            .expect("resident")
            .modified_since_confirmation,
        u64::MAX,
    );
}

#[test]
fn a_suspended_identity_reports_its_suspension_until_its_state_is_dropped() {
    // Suspension is what keeps a disproved verdict from acting in the window
    // between the settlement and a durable clear that may be refused. It is
    // cleared only by dropping the state, never by a later request.
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);
    assert!(registry.snapshot(&k).expect("resident").preflight_suspended);

    registry.tick_cadence(&k, 1);
    drop(registry.begin_modified_request(&k, 1));
    assert!(
        registry.snapshot(&k).expect("resident").preflight_suspended,
        "ordinary traffic must not lift a suspension",
    );

    registry.reset(&k);
    assert!(
        registry.snapshot(&k).is_none(),
        "only dropping the identity's state ends the suspension",
    );
}

#[test]
fn the_outstanding_unconfirmed_total_sums_every_resident_identity() {
    let registry = FieldCanaryRegistry::new();
    let first = key("model-a");
    let second = key("model-b");
    drop(registry.begin_modified_request(&first, 1));
    drop(registry.begin_modified_request(&first, 1));
    drop(registry.begin_modified_request(&second, 1));

    assert_eq!(
        registry.outstanding_unconfirmed_total(),
        3,
        "the operator-facing outstanding count spans every verdict, not one key",
    );
}

/// Mutation check: delete the `Confirmed` arm's `modified_since_confirmation
/// = 0` and this goes red -- a confirmed verdict would keep carrying requests
/// it has vouched for, and a later disproof would charge them to the alarm
/// twice over.
#[test]
fn the_confirmation_tally_reset_is_load_bearing() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    for _ in 0..9 {
        drop(registry.begin_modified_request(&k, 1));
    }
    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Confirmed);
    for _ in 0..2 {
        drop(registry.begin_modified_request(&k, 1));
    }

    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);

    assert_eq!(
        registry.disproved_requests_total(),
        2,
        "only the requests repaired SINCE the last confirmation were affected by the \
         disproved verdict",
    );
}

/// Mutation check: route `Regressed` to the `Inconclusive` arm (or the
/// reverse) and this goes red -- the two outcomes differ in exactly the three
/// facts asserted here, so a mis-routed settlement cannot pass both halves.
#[test]
fn the_settlement_outcome_routing_is_load_bearing() {
    let registry = FieldCanaryRegistry::new();
    let disproved = key("model-a");
    let inconclusive = key("model-b");
    for k in [&disproved, &inconclusive] {
        for _ in 0..3 {
            drop(registry.begin_modified_request(k, 1));
        }
    }

    registry
        .claim_canary(&disproved, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);
    registry
        .claim_canary(&inconclusive, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Inconclusive);

    let bad = registry.snapshot(&disproved).expect("resident");
    let unknown = registry.snapshot(&inconclusive).expect("resident");
    assert_eq!(
        (bad.preflight_suspended, bad.modified_since_confirmation),
        (true, 0),
        "a disproof suspends and drains",
    );
    assert_eq!(
        (
            unknown.preflight_suspended,
            unknown.modified_since_confirmation
        ),
        (false, 3),
        "an inconclusive outcome does neither",
    );
    assert_eq!(
        registry.disproved_requests_total(),
        3,
        "and only the disproof charged the alarm",
    );
}

#[test]
fn shared_state_survives_hostile_concurrency_at_512_threads() {
    let registry = Arc::new(FieldCanaryRegistry::new());
    let k = Arc::new(key("model-a"));
    let threads = 512;
    let barrier = Arc::new(Barrier::new(threads));
    let claims_admitted = Arc::new(AtomicUsize::new(0));
    // Holders live at this instant. A settled or dropped claim frees the slot
    // for a later thread, so the invariant under test is that the slot is
    // never held by two threads AT ONCE -- not that it is claimed only once
    // over the whole run.
    let live_holders = Arc::new(AtomicUsize::new(0));
    let max_live = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let registry = Arc::clone(&registry);
            let k = Arc::clone(&k);
            let barrier = Arc::clone(&barrier);
            let claims_admitted = Arc::clone(&claims_admitted);
            let live_holders = Arc::clone(&live_holders);
            let max_live = Arc::clone(&max_live);
            thread::spawn(move || {
                barrier.wait();
                // Every thread contends the same identity's cadence, claim,
                // and outstanding-count state at once.
                registry.tick_cadence(&k, 1);
                let _guard = registry.begin_modified_request(&k, 1);
                if let Some(claim) = registry.claim_canary(&k, 1) {
                    claims_admitted.fetch_add(1, Ordering::SeqCst);
                    let live = live_holders.fetch_add(1, Ordering::SeqCst) + 1;
                    max_live.fetch_max(live, Ordering::SeqCst);
                    // Decrement BEFORE releasing the slot, so an overlap can
                    // only be recorded when two guards genuinely coexist.
                    live_holders.fetch_sub(1, Ordering::SeqCst);
                    if i % 2 == 0 {
                        claim.settle(CanaryOutcome::Confirmed);
                    }
                    // Odd threads drop the claim unsettled -- exercising the
                    // Drop-release path under contention.
                }
                registry.acknowledge_confirmation(&k, 1, u32::try_from(i).unwrap());
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("no thread panics under contention");
    }

    assert_eq!(
        max_live.load(Ordering::SeqCst),
        1,
        "at most one of 512 concurrent claimants may hold the canary slot at a time"
    );
    assert!(
        claims_admitted.load(Ordering::SeqCst) >= 1,
        "the slot must be claimable at all under contention"
    );
    // The registry must end in a locally consistent state: no claim left
    // resident (every claim path above either settles or drops), and the
    // outstanding count returned to zero once every guard dropped.
    let snap = registry.snapshot(&k).expect("resident");
    assert!(!snap.canary_claimed);
    assert_eq!(snap.outstanding, 0);
}

#[test]
fn every_modified_request_is_tallied_exactly_once_under_hostile_concurrency() {
    // The alarm's whole claim is a COUNT, so the count is what a hostile
    // thread count has to pin: a lost increment under-reports the requests a
    // later disproof affected, and a double increment over-reports them. The
    // tally is drained into the lifetime total by one disproof at the end, so
    // the two numbers must agree exactly.
    let registry = Arc::new(FieldCanaryRegistry::new());
    let k = Arc::new(key("model-a"));
    let threads = 512;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let k = Arc::clone(&k);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                drop(registry.begin_modified_request(&k, 1));
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("no thread panics under contention");
    }

    assert_eq!(
        registry
            .snapshot(&k)
            .expect("resident")
            .modified_since_confirmation,
        threads as u64,
        "every one of 512 concurrent modified requests is tallied exactly once",
    );
    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);
    assert_eq!(
        registry.disproved_requests_total(),
        threads as u64,
        "and the disproof transfers the whole tally, losing none of it",
    );
}

// ---------------------------------------------------------------------------
// Monotonic incarnation transitions: a superseded writer may not mutate
// ---------------------------------------------------------------------------

/// A straggler's cadence tick must not drag the identity back to its own
/// lifecycle, nor move the current one's countdown.
///
/// Deterministic rather than timing-based: the J->K carry is performed
/// directly, then the older writer arrives, which is exactly the interleaving a
/// real race produces.
///
/// Mutation check: make `admit_incarnation` reseed on any mismatch (the
/// direction-blind `!=` it replaced) and this goes red on the incarnation.
#[test]
fn an_older_tick_cannot_drag_a_carried_identity_backward() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 4);
    // Burn one tick so the countdown is mid-cycle and a reseed would be visible.
    registry.tick_cadence(&k, 1);
    let claim = registry.claim_canary(&k, 1).expect("claim admitted");
    claim.settle_confirmed_and_carry(2);
    let carried = registry.snapshot(&k).expect("resident");

    // The straggler: planned against incarnation 1, arriving after the carry.
    let due = registry.tick_cadence(&k, 1);

    assert!(
        !due,
        "a superseded tick never reports a canary due, so its planner forwards \
         the request unchanged instead of claiming a slot",
    );
    let after = registry.snapshot(&k).expect("resident");
    assert_eq!(
        after.incarnation, 2,
        "the stale tick must not move the identity back to its own lifecycle",
    );
    assert_eq!(
        after.cadence, carried.cadence,
        "nor move the current lifecycle's countdown",
    );
    assert_eq!(
        after.confirmations, carried.confirmations,
        "nor reset the confirmation count the carry preserved",
    );
}

/// A straggler's repair accounting must not erase the current lifecycle's tally
/// or its live claim, and must refuse to open so its planner fails open.
///
/// Mutation check: make `admit_incarnation` reseed on any mismatch and this goes
/// red -- the reseed wipes both the tally and the claim.
#[test]
fn an_older_begin_cannot_erase_a_tally_or_a_live_claim() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 2, 1);
    // The CURRENT lifecycle has exposure and a canary in flight.
    drop(
        registry
            .begin_modified_request(&k, 2)
            .expect("current lifecycle accounts"),
    );
    let _live = registry.claim_canary(&k, 2).expect("live claim admitted");

    let stale = registry.begin_modified_request(&k, 1);

    assert!(
        stale.is_none(),
        "a superseded planner is refused the accounting, so it forwards unchanged \
         rather than applying a repair nothing would count",
    );
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.incarnation, 2, "the identity did not move backward");
    assert_eq!(
        snap.modified_since_confirmation, 1,
        "the current lifecycle's exposure is intact",
    );
    assert!(
        snap.canary_claimed,
        "and its live canary still holds the slot",
    );
}

/// A straggler must not be admitted a canary alongside the live one: that is the
/// second concurrent canary for one identity the slot exists to prevent.
///
/// Mutation check: make `admit_incarnation` reseed on any mismatch and this goes
/// red -- the reseed clears `canary_claimed` and the stale claim is admitted.
#[test]
fn an_older_claim_cannot_admit_a_second_canary() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 2, 1);
    let _live = registry.claim_canary(&k, 2).expect("live claim admitted");

    assert!(
        registry.claim_canary(&k, 1).is_none(),
        "the superseded claim is refused while the current lifecycle's canary is \
         in flight",
    );
    assert!(
        registry.snapshot(&k).expect("resident").canary_claimed,
        "and the live claim is undisturbed",
    );
}

/// An older acknowledgement must not overwrite a newer lifecycle's confirmation
/// count, and reports the count that actually stands.
#[test]
fn an_older_acknowledgement_cannot_overwrite_a_newer_count() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 5, 3);

    let reported = registry.acknowledge_confirmation(&k, 4, 99);

    assert_eq!(
        reported, 3,
        "the stale acknowledgement reports the count that stands, not its own",
    );
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.confirmations, 3, "and writes nothing");
    assert_eq!(snap.incarnation, 5, "leaving the identity where it was");
}

/// A NEWER caller still reseeds: the monotonic rule refuses only backward
/// motion, and this is the positive control proving the refusal above is
/// directional rather than a blanket "mismatch does nothing".
#[test]
fn a_newer_caller_still_reseeds_the_identity() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 2);
    drop(
        registry
            .begin_modified_request(&k, 1)
            .expect("accounts at the resident incarnation"),
    );

    let opened = registry.begin_modified_request(&k, 9);

    assert!(opened.is_some(), "a newer lifecycle may account");
    let snap = registry.snapshot(&k).expect("resident");
    assert_eq!(snap.incarnation, 9, "and it reseeds the slot forward");
    assert_eq!(
        snap.modified_since_confirmation, 1,
        "starting its own exposure from this request alone",
    );
    assert_eq!(
        snap.confirmations, 0,
        "with no count inherited from the old one"
    );
}

/// The carry path resets the cadence, and this test makes that observable: the
/// countdown is MID-CYCLE before the settlement, so a carry that omitted the
/// reset would leave it there.
///
/// Mutation check: delete `entry.apply_confirmed()` from
/// `settle_confirmed_and_carry` and this goes red on the cadence.
#[test]
fn the_carry_path_restarts_a_mid_cycle_cadence() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 1);
    // Drive the countdown well off its reset value, and build up exposure.
    for _ in 0..5 {
        registry.tick_cadence(&k, 1);
    }
    drop(
        registry
            .begin_modified_request(&k, 1)
            .expect("accounts at the resident incarnation"),
    );
    let mid = registry.snapshot(&k).expect("resident");
    assert_ne!(
        mid.cadence, CANARY_INTERVAL,
        "fixture premise: the countdown is mid-cycle before the settlement",
    );
    assert_eq!(
        mid.modified_since_confirmation, 1,
        "fixture premise: and the lifecycle has exposure to vouch for",
    );

    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle_confirmed_and_carry(2);

    let after = registry.snapshot(&k).expect("resident");
    assert_eq!(
        after.cadence, CANARY_INTERVAL,
        "the carry restarts the full interval rather than leaving the countdown \
         wherever the confirmed cycle ended",
    );
    assert_eq!(
        after.modified_since_confirmation, 0,
        "and vouches for the requests the confirmed verdict had modified",
    );
    assert_eq!(after.incarnation, 2, "on the minted lifecycle");
}

// ---------------------------------------------------------------------------
// In-flight accounting across a carry
// ---------------------------------------------------------------------------

/// A modified request still IN FLIGHT when a confirmation carries the identity
/// forward must not strand the in-flight count above zero.
///
/// The guard's decrement is incarnation-scoped, which is required so a straggler
/// cannot decrement a counter belonging to the lifecycle that replaced it. That
/// scoping alone leaks: the guard opened at N can never find N resident again
/// after the carry, so its drop no-ops and `outstanding` stays at 1 forever --
/// an identity that permanently reports phantom in-flight traffic. The carry
/// itself therefore has to zero the count it is abandoning.
///
/// Mutation check: delete `entry.outstanding = 0;` from
/// `settle_confirmed_and_carry` and this goes red on the post-carry count.
#[test]
fn a_request_in_flight_across_a_carry_leaves_no_phantom_outstanding() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 1);
    let in_flight = registry
        .begin_modified_request(&k, 1)
        .expect("accounts at the resident incarnation");
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        1,
        "fixture premise: one request is genuinely in flight before the carry",
    );

    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle_confirmed_and_carry(2);

    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        0,
        "the carry abandons the lifecycle the in-flight guards were opened \
         against, so it must zero the count rather than leave requests no drop \
         can ever decrement",
    );

    // The straggler drop: it must still be a no-op against the new lifecycle.
    drop(in_flight);
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        0,
        "and the abandoned guard's drop neither resurrects nor underflows the \
         count",
    );
}

/// The carry's reset must not become a blunt unconditional zero: a request that
/// began under the NEW incarnation is genuinely in flight, and a straggler from
/// the old one dropping afterwards must not decrement it.
///
/// This is the paired positive control for the reset above -- it is what
/// distinguishes "scoped drops plus a reset at the carry" from "drops that
/// decrement whatever is resident".
///
/// Mutation check: make `end_modified_request` ignore the incarnation (drop its
/// `entry.incarnation != incarnation` early return) and this goes red.
#[test]
fn an_abandoned_guard_cannot_decrement_the_new_lifecycles_count() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 1);
    let abandoned = registry
        .begin_modified_request(&k, 1)
        .expect("accounts at the resident incarnation");

    registry
        .claim_canary(&k, 1)
        .expect("claim admitted")
        .settle_confirmed_and_carry(2);

    // A fresh request under the CARRIED lifecycle.
    let live = registry
        .begin_modified_request(&k, 2)
        .expect("accounts at the carried incarnation");
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        1,
        "fixture premise: the new lifecycle has one genuine request in flight",
    );

    drop(abandoned);

    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        1,
        "a guard from the superseded lifecycle must not decrement the count the \
         lifecycle that replaced it owns",
    );
    drop(live);
    assert_eq!(
        registry.snapshot(&k).expect("resident").outstanding,
        0,
        "and the live guard's own drop still settles its entry",
    );
}
