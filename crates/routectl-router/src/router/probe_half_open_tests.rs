//! Half-open ownership, the probe deferral, and what a declined probe does not
//! consume.
//!
//! Ownership of the half-open claim is carried from the admission that took
//! it, never re-derived from the shared in-flight bit -- a probe that released
//! a claim it did not own would free a client's claim and admit a second
//! concurrent probe past the single-probe invariant.
//!
//! The deferral is the other half: probe mode declines a half-open-ready lane
//! INSIDE the gate's critical section, before the claim and before the RPM
//! debit, so a declined probe leaves recovery to real traffic and spends none
//! of the operator's rate budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, Usage};

use super::probe_test_support::{
    BoxStreamAlias, GROUNDED_PATH, OkProvider, remote_router, remote_router_with_rpm,
    router_on_base_with_failure_threshold,
};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{
    FreeValidatorOutcome, PROBE_BACKOFF_CEILING, PROBE_MAX_ATTEMPTS, ProbeValidator,
};

#[tokio::test]
async fn a_probe_completion_does_not_release_a_real_requests_half_open_claim() {
    // THE interleaving. A probe admitted through a CLOSED breaker holds no
    // half-open claim. If the breaker then trips and a REAL request takes the
    // single half-open slot, an unconditional
    // `release_probe_slot(state_key)` at probe completion would free the real
    // request's claim -- admitting a second concurrent probe past the
    // single-probe invariant the breaker depends on.
    //
    // Staged deterministically: the probe's provider signals when its dial
    // has begun and then blocks, so the breaker can be tripped and the real
    // request's claim taken WHILE the probe is in flight. Only then is the
    // probe released to complete.
    struct Gated {
        dial_started: Arc<tokio::sync::Notify>,
        may_finish: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl Provider for Gated {
        fn id(&self) -> &'static str {
            "p1"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("p1", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                model: "wire-model".to_string(),
                usage: Some(Usage::default()),
                ..Default::default()
            })
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
            Err(Error::upstream("p1", 500, "body"))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.dial_started.notify_one();
            self.may_finish.notified().await;
            Ok(TokenCount {
                input_tokens: 7,
                extras: serde_json::Map::new(),
            })
        }
    }

    let dial_started = Arc::new(tokio::sync::Notify::new());
    let may_finish = Arc::new(tokio::sync::Notify::new());
    let router = Arc::new(remote_router(Arc::new(Gated {
        dial_started: Arc::clone(&dial_started),
        may_finish: Arc::clone(&may_finish),
    })));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");

    // The breaker is CLOSED here, so the probe's own gate_check claims no
    // half-open slot -- its guard is inert.
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let worker = tokio::spawn({
        let router = Arc::clone(&router);
        async move { router.run_due_probes().await }
    });
    dial_started.notified().await;
    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "premise: the in-flight probe must hold NO half-open claim"
    );

    // Now trip the breaker and let a REAL request take the half-open slot.
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;
    // The client request admits AND KEEPS its ownership token, as a real
    // dispatch does across its own await. Dropping it here would release the
    // claim and there would be nothing for the probe to wrongly free.
    let (client_refusal, client_guard) = router.admit_dispatch("m1", "p1");
    assert!(
        client_refusal.is_none(),
        "premise: the client request must be admitted as the half-open attempt"
    );
    assert!(
        router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "premise: the client request now OWNS the half-open claim"
    );

    // Release the probe. Its completion must leave that claim alone.
    may_finish.notify_one();
    worker.await.expect("worker must not panic");

    assert!(
        router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "a probe's completion must not release a claim it never held -- doing so \
         admits a second concurrent probe past the single-probe invariant"
    );
    // And the client's own guard still governs that claim: dropping it is what
    // releases the slot, proving the claim was live the whole time rather than
    // already gone.
    drop(client_guard);
    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "the client's guard owns the release"
    );
}

#[tokio::test]
async fn a_probe_leaves_no_half_open_claim_after_completing() {
    // Positive control for the test above: through a CLOSED breaker the probe
    // dials and completes, and no half-open claim exists afterwards either.
    // Without this pairing, "must not release a real request's claim" would
    // also pass on code that strands claims everywhere.
    //
    // A probe can no longer be made to HOLD a claim across its dial -- an
    // admission that takes the slot declines before dialing -- so the
    // ownership property is pinned at the admission level instead, by
    // `exactly_one_admission_claims_the_half_open_slot` and
    // `a_refused_admission_claims_nothing`.
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
    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "a completed probe must leave no half-open claim behind"
    );
}

#[tokio::test]
async fn cancelling_a_probe_mid_dial_strands_no_half_open_claim() {
    // Cancellation is a normal exit for the worker: the driver's biased
    // shutdown select drops the run future mid-dial. Whatever the probe holds
    // must come back on that drop, or the breaker latches for the process.
    //
    // Driven with the breaker CLOSED, because that is now the only state in
    // which a probe reaches its dial at all: an admission handed the half-open
    // slot declines before dialing. The property still bites -- the guard's
    // `Drop` runs on the cancelled path, and this test would catch a drop
    // handler that released a claim it did not own just as well as one that
    // stranded a claim it did.
    struct Blocking {
        dial_started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl Provider for Blocking {
        fn id(&self) -> &'static str {
            "p1"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("p1", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::default())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
            Err(Error::upstream("p1", 500, "body"))
        }
        async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
            self.dial_started.notify_one();
            // Never completes: only cancellation ends this.
            std::future::pending::<()>().await;
            unreachable!("the future is dropped before it can resolve")
        }
    }

    let dial_started = Arc::new(tokio::sync::Notify::new());
    let router = Arc::new(remote_router(Arc::new(Blocking {
        dial_started: Arc::clone(&dial_started),
    })));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let worker = tokio::spawn({
        let router = Arc::clone(&router);
        async move { router.run_due_probes().await }
    });
    dial_started.notified().await;

    // Cancel mid-dial.
    worker.abort();
    let _ = worker.await;

    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "a cancelled probe must strand no half-open claim"
    );
    // And the lease came back, so the job is re-leasable rather than wedged
    // in-flight forever.
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "the cancelled lease must release its concurrency slot on drop"
    );
}

#[test]
fn a_refused_admission_claims_nothing() {
    // The ownership token is an admission fact, so a refusal must never carry
    // a claim -- otherwise a gate-blocked caller would release someone else's
    // on drop. Asserted at the state level, where both outcomes are reachable.
    let mut state =
        crate::runtime_state::ProviderState::new(&crate::config::ProviderRuntimePolicy::default());
    state.force_open(std::time::Instant::now(), Duration::from_hours(1));

    // Within cooldown: refused, nothing claimed.
    let admission = state.try_dispatch_admitting(std::time::Instant::now());
    assert_eq!(
        admission.decision,
        crate::runtime_state::GateDecision::CircuitOpen
    );
    assert!(
        !admission.claimed_half_open,
        "a refusal must claim no half-open slot"
    );
}

#[test]
fn exactly_one_admission_claims_the_half_open_slot() {
    // The single-probe invariant, stated in terms of OWNERSHIP rather than of
    // the shared bit: past the cooldown the first admission claims and later
    // ones are refused, so at most one token in existence says it owns.
    let mut state =
        crate::runtime_state::ProviderState::new(&crate::config::ProviderRuntimePolicy::default());
    let now = std::time::Instant::now();
    state.force_open(now, Duration::from_millis(1));
    let past_cooldown = now + Duration::from_millis(5);

    let first = state.try_dispatch_admitting(past_cooldown);
    let second = state.try_dispatch_admitting(past_cooldown);

    assert_eq!(first.decision, crate::runtime_state::GateDecision::Allow);
    assert!(first.claimed_half_open, "the first admission owns the slot");
    assert_eq!(
        second.decision,
        crate::runtime_state::GateDecision::CircuitOpen
    );
    assert!(
        !second.claimed_half_open,
        "a second concurrent admission must be refused and own nothing"
    );
}

#[tokio::test]
async fn a_probe_admission_at_the_gate_does_not_disturb_a_live_client_claim() {
    // Ownership is carried from the admission that took the claim, and
    // `Router::admit` is where that carrying happens: it arms a guard only
    // when `GateAdmission::claimed_half_open` says THIS call set the bit.
    //
    // Re-deriving from the SHARED in-flight bit instead -- arming whenever
    // some claim exists -- looks identical while the probe is alone, and
    // differs exactly here: a client holds the claim, a probe is admitted and
    // DROPPED, and the drop frees a claim the probe never owned. That admits a
    // second concurrent probe past the breaker's single-probe invariant.
    //
    // Driven at the gate rather than through the worker, so no dial, timeout,
    // or settlement sits between the admission and the drop.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base_with_failure_threshold(
        "https://api.anthropic.com",
        provider,
        // One failure opens the breaker, which is what makes a half-open slot
        // exist to be owned at all.
        Some(1),
    );

    // Trip the breaker, then let its cooldown lapse so the next CLIENT
    // admission is handed the single half-open slot.
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;

    let (client_refusal, client_guard) = router.admit_dispatch("m1", "p1");
    assert!(
        client_refusal.is_none(),
        "premise: the client takes the lapsed half-open attempt"
    );
    assert!(
        router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "premise: the client's claim is live on shared state"
    );

    // A probe asks while the client's claim is outstanding. Probe mode refuses
    // a half-open-ready lane, and this lane's slot is already taken, so the
    // probe owns nothing -- its guard must be inert.
    let (probe_refusal, probe_guard) = router.admit_probe_dispatch("m1", "p1");
    assert!(
        probe_refusal.is_some(),
        "premise: a probe is declined while the breaker is not closed"
    );
    drop(probe_guard);

    // THE assertion: the client's claim survives the probe's admission and
    // drop. Under a shared-bit re-derivation this reads false.
    assert!(
        router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "a probe's drop must not release a claim it never owned"
    );

    // Positive control on the same state: the OWNER's drop does release it, so
    // the assertion above is about ownership rather than an unreleasable bit.
    drop(client_guard);
    assert!(
        !router.gate_status_for_tests("m1").half_open_probe_in_flight,
        "the owning admission's drop must release the claim"
    );
}

#[tokio::test]
async fn a_deferred_probe_charges_no_rpm_token() {
    // The deferral must land BEFORE both side effects of admission, not after.
    //
    // Handing the half-open claim back is only half the job: the RPM debit
    // happens in the same critical section and NO path refunds it, so a probe
    // that admitted and then declined would silently spend a slice of the
    // operator's rate budget on a request it never sent -- every tick, for as
    // long as the lane stays half-open-ready.
    //
    // Pinned on the rate budget itself, not on a counter: `rpm_available` is
    // the value a real client request competes for.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_rpm(provider.clone(), 60, Some(1));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Breaker open with a lapsed cooldown: the lane is HalfOpenReady, which is
    // the one state where a probe would otherwise take the recovery attempt.
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;
    let before = router
        .gate_status_for_tests("m1")
        .rpm_available
        .expect("premise: the RPM bucket must be ENABLED, or this pins nothing");

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1, "premise: the probe must have been leased and run");
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "the probe must not dial a half-open-ready lane"
    );
    let gate = router.gate_status_for_tests("m1");
    assert!(
        (gate.rpm_available.expect("bucket still enabled") - before).abs() < 0.5,
        "a deferred probe must charge NO rpm token: {before} -> {:?}",
        gate.rpm_available
    );
    assert!(
        !gate.half_open_probe_in_flight,
        "and must claim no half-open slot"
    );
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "the recovery attempt must still be on offer to real traffic"
    );
    // The decisive consequence: a REAL request is still admitted as the
    // half-open attempt, with its token available.
    assert!(
        router.gate_check("m1", "p1").is_none(),
        "the next real request must still be admitted as the half-open attempt"
    );
}

#[tokio::test]
async fn a_probe_through_a_closed_breaker_does_charge_its_rpm_token() {
    // POSITIVE CONTROL for the test above. The same RPM-enabled fixture with
    // the breaker CLOSED: the probe dials and DOES spend a token, so the
    // no-charge assertion is about the DEFERRAL rather than about this fixture
    // never charging anything.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_rpm(provider.clone(), 60, Some(1));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let before = router
        .gate_status_for_tests("m1")
        .rpm_available
        .expect("premise: bucket enabled");

    router.run_due_probes().await;

    assert!(
        provider.count_calls.load(Ordering::SeqCst) > 0,
        "premise: through a closed breaker the probe dials"
    );
    let after = router
        .gate_status_for_tests("m1")
        .rpm_available
        .expect("bucket enabled");
    assert!(
        before - after > 0.5,
        "a probe that actually dials spends its token: {before} -> {after}"
    );
}

#[tokio::test]
async fn three_deferrals_neither_abandon_nor_tombstone_the_identity() {
    // The attempt budget bounds repeated ANSWERS, not repeated non-answers.
    //
    // `PROBE_MAX_ATTEMPTS` is three, so if a gate deferral charged an attempt,
    // three declines from a breaker that had simply not finished recovering
    // would ABANDON the job and TOMBSTONE the identity -- taking the lane out
    // of probing for the whole incarnation, permanently, on the strength of
    // three questions never asked.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Hold the lane half-open-ready across the whole episode, and drive MORE
    // deferrals than the attempt cap.
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut deferred = 0usize;
    for _ in 0..=PROBE_MAX_ATTEMPTS {
        // Each pass must actually lease, or the loop proves nothing: a job in
        // backoff is not due, so the lease is driven at a clock past its
        // backoff window.
        if let Some(lease) = router.lease_due_probe(Instant::now() + PROBE_BACKOFF_CEILING) {
            let validator = lease.validator();
            let outcome = router
                .run_free_validator_for_tests(&key, lease.payload(), validator)
                .await;
            assert_eq!(
                outcome,
                FreeValidatorOutcome::GateDeferred,
                "premise: each pass must be a gate DEFERRAL, not some other outcome"
            );
            let release = lease.settle(
                crate::probe_scheduler::ProbeSettlement::Deferred,
                Instant::now(),
            );
            assert!(release.committed());
            deferred += 1;
        }
    }

    assert!(
        deferred > PROBE_MAX_ATTEMPTS as usize,
        "premise: more deferrals than the attempt cap must have occurred, or \
         the cap was never reachable (saw {deferred})"
    );
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "premise: none of them dialed"
    );

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.abandoned_total, 0,
        "deferrals must not abandon the job at the attempt cap"
    );
    assert_eq!(
        snap.tombstoned, 0,
        "and must not tombstone the identity, which would refuse it for the \
         whole incarnation"
    );
    assert_eq!(
        snap.deferrals_total, deferred as u64,
        "every deferral is still counted, so the volume is observable"
    );
    // The job is still tracked and still backing off, so it can ask again.
    assert_eq!(snap.backing_off, 1, "the job remains queued to retry");
    assert_eq!(snap.free_exhausted_total, 0, "no free step was spent");
    assert!(router.paid_probe_candidates().is_empty());
}

#[tokio::test]
async fn a_lane_that_recovers_after_deferrals_still_dials() {
    // The EVENTUAL-SUCCESS control for the test above: deferrals leave the job
    // able to ask again, so once the breaker closes the very next lease dials
    // and settles normally. Without this, "not abandoned" could hold on a job
    // that was wedged rather than merely waiting.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    router.force_open_breaker("m1", Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Defer past the attempt cap.
    for _ in 0..=PROBE_MAX_ATTEMPTS {
        if let Some(lease) = router.lease_due_probe(Instant::now() + PROBE_BACKOFF_CEILING) {
            let validator = lease.validator();
            let _ = router
                .run_free_validator_for_tests(&key, lease.payload(), validator)
                .await;
            let _ = lease.settle(
                crate::probe_scheduler::ProbeSettlement::Deferred,
                Instant::now(),
            );
        }
    }
    assert_eq!(provider.count_calls.load(Ordering::SeqCst), 0, "premise");

    // The lane recovers: a real request settles the half-open attempt.
    router.record_success("m1");
    assert_eq!(
        router.gate_status_for_tests("m1").circuit,
        crate::runtime_state::CircuitPhase::Closed,
        "premise: the breaker must be closed now"
    );

    let lease = router
        .lease_due_probe(Instant::now() + PROBE_BACKOFF_CEILING)
        .expect("the deferred job must still be leasable, not abandoned");
    let validator = lease.validator();
    let outcome = router
        .run_free_validator_for_tests(&key, lease.payload(), validator)
        .await;

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        1,
        "once the breaker closes the deferred job DIALS"
    );
    assert_eq!(
        outcome,
        FreeValidatorOutcome::Settled,
        "and settles the question normally"
    );
}
