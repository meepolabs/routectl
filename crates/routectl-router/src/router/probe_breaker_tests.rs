//! The circuit breaker, half-open ownership, and bounded payload bytes.
//!
//! A background probe is not client traffic. It never credits or debits the
//! breaker, and it never takes the half-open recovery slot: that slot is how
//! the breaker asks whether the lane is healthy for CLIENTS, and a probe
//! neither answers that question nor should displace the real request that
//! would.
//!
//! Ownership of the half-open claim is carried from the admission that took
//! it, never re-derived from the shared in-flight bit -- a probe that
//! released a claim it did not own would free a client's claim and admit a
//! second concurrent probe past the single-probe invariant.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::probe_test_support::{
    FailingProvider, GROUNDED_PATH, OkProvider, remote_router,
    router_on_base_with_failure_threshold,
};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;

#[tokio::test]
async fn a_probe_admitted_as_the_half_open_attempt_declines_and_returns_the_slot() {
    // RECOVERY BELONGS TO REAL TRAFFIC. When the cooldown has lapsed, the
    // next admission is handed the breaker's single half-open slot. A probe
    // must not take it: this validator neither credits nor debits the
    // breaker, so a probe holding the slot produces no recovery signal while
    // displacing the real request that would -- the lane stays open a full
    // cooldown longer for nothing.
    //
    // Three properties, all on real breaker state:
    //
    //  - the probe does NOT dial (the claim is declined before the dial);
    //  - the claim comes straight back, so a REAL request can take it;
    //  - the circuit stays open at half-open-READY, awaiting that request.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Trip the breaker with a cooldown short enough to lapse, so the probe's
    // own admission WOULD be handed the single half-open slot.
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;

    let ran = router.run_due_probes().await;
    assert_eq!(ran, 1, "premise: the probe must have been leased and run");
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "a probe handed the half-open slot must DECLINE the dial rather than \
         spend the breaker's one recovery attempt on background work"
    );

    let gate = router.gate_status_for_tests("m1");
    assert!(
        !gate.half_open_probe_in_flight,
        "the declined claim must be returned immediately, or the breaker \
         latches open and no real request can ever recover the lane"
    );
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "the lane must still be offering its recovery attempt to real traffic"
    );
    assert!(
        gate.circuit_open_elapsed.is_some(),
        "the open window must survive the declined probe"
    );

    // Neither the free step NOR the attempt budget was spent: a deferral is
    // "not now", so the job backs off and asks again once a real request has
    // settled recovery. `three_deferrals_neither_abandon_nor_tombstone_the_identity`
    // pins the attempt half across the cap.
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.backing_off, 1,
        "the deferred step must remain queued to retry, not be consumed"
    );
    assert_eq!(snap.deferrals_total, 1);
    assert_eq!(snap.free_exhausted_total, 0);
    assert!(router.paid_probe_candidates().is_empty());
}

#[tokio::test]
async fn a_probe_on_a_closed_breaker_still_dials_and_holds_no_claim() {
    // POSITIVE CONTROL for the decline above: the same fixture, breaker
    // CLOSED. The probe dials normally and its admission takes no half-open
    // claim, so the decline test is about the CLAIM rather than about probes
    // never dialing at all.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1);
    assert!(
        provider.count_calls.load(Ordering::SeqCst) > 0,
        "through a closed breaker the probe must dial"
    );
    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "a closed-breaker admission claims nothing to begin with"
    );
}

#[tokio::test]
async fn a_probe_success_does_not_credit_a_closed_breakers_failure_count() {
    // The converse direction of the same rule: on a HEALTHY lane a probe
    // must not touch the breaker either. `last_outcome` is the observable --
    // a probe that recorded a success would stamp it.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(
        provider.count_calls.load(Ordering::SeqCst) > 0,
        "premise: dialed"
    );
    assert_eq!(
        router.gate_status_for_tests("m1").last_outcome,
        None,
        "a probe outcome must not be stamped on the client-traffic breaker"
    );
}

#[tokio::test]
async fn a_probe_rate_limit_does_not_open_the_client_traffic_breaker() {
    // THE security case. `count_tokens` is separately rate-limited upstream,
    // so a 429 on it says nothing about the inference lane -- debiting the
    // breaker would let background probing quarantine paid traffic.
    let provider = Arc::new(FailingProvider {
        status: 429,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(provider.calls.load(Ordering::SeqCst) > 0, "premise: dialed");
    let gate = router.gate_status_for_tests("m1");
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::Closed,
        "a probe 429 must not open the paid lane's breaker"
    );
    assert_eq!(
        gate.last_outcome, None,
        "a probe failure must not be stamped as the lane's last outcome"
    );
    // And real traffic still flows.
    assert!(
        router.gate_check("m1", "p1").is_none(),
        "client traffic must still be admitted after a probe rate limit"
    );
}

#[tokio::test]
async fn a_probe_server_error_does_not_re_trip_a_recovering_breaker() {
    // A 5xx on the probe must not open the breaker: that would hold a lane
    // serving clients fine closed on the strength of background work.
    //
    // Driven with the breaker CLOSED and its threshold at 1, so ONE debit
    // would trip it. A lapsed-cooldown fixture cannot be used here: the
    // probe declines the half-open slot and never dials, so there would be
    // no upstream error to classify at all.
    let provider = Arc::new(FailingProvider {
        status: 503,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base_with_failure_threshold(
        "https://api.anthropic.com",
        provider.clone(),
        Some(1),
    );
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(
        provider.calls.load(Ordering::SeqCst) > 0,
        "premise: the probe must have dialed and drawn the 503"
    );
    let gate = router.gate_status_for_tests("m1");
    assert!(!gate.half_open_probe_in_flight);
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::Closed,
        "a probe 5xx must not trip the client-traffic breaker, even at a \
         threshold of one"
    );
    // The decisive consequence: a REAL request is still admitted.
    assert!(
        router.gate_check("m1", "p1").is_none(),
        "the next real request must still be admitted"
    );
}

#[tokio::test]
async fn a_bad_request_probe_neither_debits_the_breaker_nor_claims_a_slot() {
    // Routectl's OWN malformed request is not evidence about the lane, so the
    // bad-request path must leave the breaker untouched. Breaker CLOSED at a
    // threshold of 1, so a single debit would be visible.
    let provider = Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base_with_failure_threshold(
        "https://api.anthropic.com",
        provider.clone(),
        Some(1),
    );
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(
        provider.calls.load(Ordering::SeqCst) > 0,
        "premise: the probe must have dialed and drawn the 400"
    );
    let gate = router.gate_status_for_tests("m1");
    assert!(
        !gate.half_open_probe_in_flight,
        "a refused probe request must leave no half-open claim"
    );
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::Closed,
        "routectl's own bad request must not debit the operator's breaker"
    );
    assert!(
        router.gate_check("m1", "p1").is_none(),
        "the next real request must still be admitted"
    );
}
