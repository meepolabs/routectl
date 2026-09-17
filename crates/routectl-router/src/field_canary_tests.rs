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

#[test]
fn a_stale_settlement_releases_the_claim_without_mutating_verdict_state() {
    let registry = FieldCanaryRegistry::new();
    let k = key("model-a");
    registry.acknowledge_confirmation(&k, 1, 7);

    let guard = registry.claim_canary(&k, 1).expect("claim admitted");
    // The identity moves to a new incarnation while the canary is still
    // in flight -- e.g. the verdict was cleared and re-learned mid-probe.
    registry.acknowledge_confirmation(&k, 2, 1);

    guard.settle(CanaryOutcome::Regressed);

    let snap = registry.snapshot(&k).expect("resident");
    assert!(!snap.canary_claimed, "the claim is always released");
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

/// Mutation check: delete the call to `reseed_if_stale` in
/// `acknowledge_confirmation` and this test goes red, because a canary claim
/// and non-default cadence left over from the superseded incarnation would
/// then leak into the fresh one instead of being discarded. (The
/// `confirmations` field alone cannot pin this guard: `acknowledge_confirmation`
/// always overwrites it unconditionally, guard or not.)
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
