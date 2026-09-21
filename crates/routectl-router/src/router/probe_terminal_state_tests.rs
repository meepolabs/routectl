//! Terminal markers, their own capacity bound, and stale settlement.
//!
//! A settled question must not be re-asked within an incarnation, so
//! terminal identities are tombstoned. The markers carry their OWN capacity
//! bound rather than the queue's -- the queue bounds concurrent work while
//! markers accumulate one per identity settled -- and exhausting it FAILS
//! CLOSED, because a marker that could not be stored would let its identity
//! re-enter the queue on the next admitted request.
//!
//! A settlement for state the scheduler no longer tracks releases its slot
//! and does nothing else: no candidate, no reschedule.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};

use super::Router;
use super::probe_test_support::{
    BoxStreamAlias, FailingProvider, GROUNDED_PATH, OkProvider, ZeroCountProvider,
    grounding_request, remote_router, remote_router_with_lanes, remote_router_with_paid_cap,
    remote_router_with_rpm, router_on_base_with_failure_threshold, settle_terminally,
};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{
    PROBE_QUEUE_DEPTH, PROBE_TOMBSTONE_CAPACITY, ProbeActivation, ProbeValidator,
};

/// Lanes the overflow fixtures install: terminal capacity plus a MARGIN.
///
/// The margin exists so a test can drive settlements PAST capacity and observe
/// the saturation path. Sized a few above rather than exactly at capacity
/// because several of these tests queue a handful of extra identities before
/// any of them settles -- at exactly capacity those would have no lane to
/// resolve against and would settle `Unavailable`, which measures the wrong
/// thing.
const OVERFLOW_LANES: usize = PROBE_TOMBSTONE_CAPACITY + 8;

#[tokio::test]
async fn a_resolved_identity_is_tombstoned_until_the_incarnation_advances() {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    router.run_due_probes().await;
    assert_eq!(router.probe_scheduler_snapshot().resolved_total, 1);

    // Re-activation is refused for the REST of this incarnation: the
    // question is answered, so a burst of later traffic must not re-ask it.
    assert_eq!(
        router.activate_probe_lane(&key, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.tombstoned, 1);
    assert_eq!(snap.tombstone_refusals_total, 1);

    // A publication clears it: the answer can legitimately have changed.
    router.publish_probe_incarnation();
    assert_eq!(
        router.activate_probe_lane(&key, ProbeValidator::CountTokens),
        ProbeActivation::Queued,
        "retirement must clear the tombstone so the next incarnation may ask"
    );
    assert_eq!(router.probe_scheduler_snapshot().tombstoned, 0);
}

#[tokio::test]
async fn an_exhausted_identity_is_tombstoned_for_its_incarnation() {
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    router.run_due_probes().await;
    assert_eq!(router.probe_scheduler_snapshot().free_exhausted_total, 1);

    assert_eq!(
        router.activate_probe_lane(&key, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned
    );
}

#[tokio::test]
async fn a_persistent_refusal_of_routectls_own_request_bounds_reactivation_to_one_dial() {
    // A 400 on routectl's OWN probe body is a PERSISTENT property of what this
    // build sends to this lane: the probe now travels under the admitted
    // request's own beta context and Claude-Code classification, so the refusal
    // is not a header artifact a later identical request would avoid.
    //
    // So it is terminal for the incarnation. Without the marker, reactivation is
    // UNBOUNDED -- N admitted requests become N dials and N RPM debits, all
    // refused identically. This drives N real admitted requests and asserts one
    // dial.
    let provider = Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    // RPM limit set so the budget is OBSERVABLE: at the default the bucket is
    // disabled and `rpm_available` reads `None`, which would make the spend
    // assertion below vacuous.
    let router = remote_router_with_rpm(
        provider.clone() as Arc<dyn routectl_core::Provider>,
        100,
        None,
    );

    let before = router
        .gate_status_for_tests("m1")
        .rpm_available
        .expect("premise: the bucket must be enabled to observe a debit");

    // Ten admitted requests on the same lane, each running the worker.
    for _ in 0..10 {
        let _ = router.complete(grounding_request()).await;
        router.run_due_probes().await;
    }

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        1,
        "a persistent refusal must be dialed ONCE per incarnation, not once per \
         admitted request"
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.tombstoned, 1,
        "the refusal must lay exactly one terminal marker"
    );
    assert!(
        snap.tombstone_refusals_total > 0,
        "the later activations must be refused BY that marker, not by luck"
    );

    // And the rate budget: exactly one probe token, not ten. Compared against
    // the admitted requests' own debits by bounding the total spend -- each
    // admitted dispatch also charges one, so ten requests plus one probe is
    // eleven, while an unbounded probe would be twenty.
    let after = router
        .gate_status_for_tests("m1")
        .rpm_available
        .expect("bucket still enabled");
    let spent = before - after;
    assert!(
        spent < 12.0,
        "an unbounded probe would double the lane's rate spend; spent {spent}"
    );
}

#[tokio::test]
async fn a_tombstoned_identity_is_probeable_again_after_a_republication() {
    // The bound is per INCARNATION, not forever: a reload republishes and the
    // markers clear, so a lane whose refusal may have been configuration-shaped
    // is asked again. This is what keeps the terminal marker from being a
    // permanent blacklist.
    let provider = Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone() as Arc<dyn routectl_core::Provider>);

    let _ = router.complete(grounding_request()).await;
    router.run_due_probes().await;
    assert_eq!(provider.count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(router.probe_scheduler_snapshot().tombstoned, 1);

    router.publish_probe_incarnation();

    let _ = router.complete(grounding_request()).await;
    router.run_due_probes().await;
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        2,
        "a republished incarnation may ask again"
    );
}

#[tokio::test]
async fn a_lane_count_above_the_queue_depth_does_not_saturate_terminal_capacity() {
    // THE config-scale case, and why terminal capacity is separate from the
    // job queue: tombstones accumulate one per identity SETTLED during an
    // incarnation, while the queue bounds concurrent work. Sharing one bound
    // would make any deployment with more lanes than the queue depth fail
    // closed almost immediately.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);
    let lanes = PROBE_QUEUE_DEPTH * 4;
    assert!(
        lanes > PROBE_QUEUE_DEPTH && lanes < PROBE_TOMBSTONE_CAPACITY,
        "premise: more lanes than the queue depth, still inside terminal capacity"
    );

    settle_terminally(&router, lanes).await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.tombstoned, lanes,
        "every settled lane must hold its own terminal marker"
    );
    assert!(
        !snap.tombstone_saturated,
        "a realistic lane count must not exhaust terminal capacity"
    );
    assert_eq!(snap.tombstone_saturations_total, 0);
    // And an unrelated fresh lane still activates.
    let fresh = FieldVerdictKey::new("m-fresh", GROUNDED_PATH, "anthropic-api").expect("identity");
    assert_eq!(
        router.activate_probe_lane(&fresh, ProbeValidator::CountTokens),
        ProbeActivation::Queued
    );
}

#[tokio::test]
async fn tombstone_saturation_at_the_new_bound_fails_closed_for_the_overflow_identity() {
    // Past terminal capacity a marker cannot be stored. Dropping it would
    // fail OPEN -- that identity would be re-activatable on the next admitted
    // request, which is the re-ask loop tombstones exist to stop -- so the
    // scheduler raises an incarnation-level saturation marker and refuses
    // every later activation until retirement.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);
    let overflow_index = PROBE_TOMBSTONE_CAPACITY;

    settle_terminally(&router, overflow_index + 1).await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.tombstoned, PROBE_TOMBSTONE_CAPACITY,
        "the marker set must stay at its own bound"
    );
    assert!(
        snap.tombstone_saturations_total > 0,
        "a marker past capacity must be recorded as a saturation"
    );
    assert!(snap.tombstone_saturated, "the scheduler must fail closed");

    // THE overflow identity: its terminal marker could not be stored, so it
    // must NOT be re-activatable.
    let overflow_key = FieldVerdictKey::new(
        &format!("m{overflow_index}"),
        GROUNDED_PATH,
        "anthropic-api",
    )
    .expect("identity");
    assert_eq!(
        router.activate_probe_lane(&overflow_key, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned,
        "an identity whose terminal marker overflowed must not re-activate"
    );
    let fresh = FieldVerdictKey::new("m-fresh", GROUNDED_PATH, "anthropic-api").expect("identity");
    assert_eq!(
        router.activate_probe_lane(&fresh, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned,
        "saturation must fail closed for every identity, not just the overflow one"
    );
}

#[tokio::test]
async fn retirement_clears_tombstone_saturation() {
    // Failing closed is bounded in TIME by the incarnation: a publication
    // clears the markers and the saturation.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);
    settle_terminally(&router, PROBE_TOMBSTONE_CAPACITY + 1).await;
    assert!(router.probe_scheduler_snapshot().tombstone_saturated);

    router.publish_probe_incarnation();

    let snap = router.probe_scheduler_snapshot();
    assert!(
        !snap.tombstone_saturated,
        "retirement must clear saturation"
    );
    assert_eq!(snap.tombstoned, 0);
    let fresh = FieldVerdictKey::new("m-fresh", GROUNDED_PATH, "anthropic-api").expect("identity");
    assert_eq!(
        router.activate_probe_lane(&fresh, ProbeValidator::CountTokens),
        ProbeActivation::Queued,
        "a republished scheduler may ask again"
    );
}

#[tokio::test]
async fn saturation_warns_once_per_episode_and_keeps_counting() {
    // The WARN is an existence proof; the counter is the volume. Once
    // saturated EVERY later terminal settlement overflows, so a line each
    // would flood precisely when the daemon is busiest. One line per
    // saturation EPISODE -- it stays suppressed until retirement clears the
    // marker, even across incarnations.
    //
    // Driven entirely through the real worker: fill terminal capacity, then
    // keep settling identities terminally past it. Activation is refused once
    // saturated, but the identities queued BEFORE the marker set filled are
    // still in flight and their settlements are what overflow.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);

    // NEGATIVE control first: at exactly capacity, nothing has overflowed, so
    // no line may appear.
    let ((), events) =
        routectl_testkit::with_capture(settle_terminally(&router, PROBE_TOMBSTONE_CAPACITY)).await;
    assert!(
        !router.probe_scheduler_snapshot().tombstone_saturated,
        "premise: exactly at capacity is not saturated"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.message == crate::probe_scheduler::PROBE_TOMBSTONE_SATURATED_EVENT),
        "NEGATIVE control: no overflow yet, so no saturation line"
    );

    // POSITIVE: queue several identities BEFORE any of them settles, so their
    // terminal settlements all overflow the full marker set.
    let ((), events) = routectl_testkit::with_capture(async {
        let mut keys = Vec::new();
        for n in PROBE_TOMBSTONE_CAPACITY..PROBE_TOMBSTONE_CAPACITY + 4 {
            let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
                .expect("identity");
            if router.activate_probe_lane(&key, ProbeValidator::CountTokens)
                == ProbeActivation::Queued
            {
                keys.push(key);
            }
        }
        // Settle them all; each one's terminal marker overflows.
        for _ in 0..=keys.len() {
            router.run_due_probes().await;
        }
    })
    .await;

    let warns = events
        .iter()
        .filter(|e| e.message == crate::probe_scheduler::PROBE_TOMBSTONE_SATURATED_EVENT)
        .count();
    assert!(
        router.probe_scheduler_snapshot().tombstone_saturated,
        "premise: the settlements must have overflowed capacity"
    );
    assert_eq!(
        warns, 1,
        "POSITIVE control: exactly one saturation line per episode, saw {warns}"
    );
    assert!(
        router
            .probe_scheduler_snapshot()
            .tombstone_saturations_total
            >= 1,
        "the counter must carry the volume"
    );
}

#[tokio::test]
async fn below_capacity_no_saturation_is_raised() {
    // Positive control for the refusals above: under capacity the scheduler
    // does NOT fail closed.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);

    settle_terminally(&router, 3).await;

    let snap = router.probe_scheduler_snapshot();
    assert!(!snap.tombstone_saturated);
    assert_eq!(snap.tombstone_saturations_total, 0);
    let fresh = FieldVerdictKey::new("m-fresh", GROUNDED_PATH, "anthropic-api").expect("identity");
    assert_eq!(
        router.activate_probe_lane(&fresh, ProbeValidator::CountTokens),
        ProbeActivation::Queued
    );
}

// ---------------------------------------------------------------------
// A candidate is published only after a committed settlement
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_lease_retired_mid_flight_publishes_no_candidate() {
    // The settlement cannot commit (the job is gone), so no candidate may
    // be recorded -- one would describe router state already retired.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider, 3);
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let lease = router
        .lease_due_probe(std::time::Instant::now())
        .expect("job is due");

    // Retire underneath the outstanding lease, then settle it.
    router.publish_probe_incarnation();
    let release = lease.settle(
        crate::probe_scheduler::ProbeSettlement::SpentFreeStep,
        std::time::Instant::now(),
    );

    assert!(
        !release.committed(),
        "a settlement against retired state must not commit"
    );
    assert!(
        !release.exhausted_free_plan(),
        "an uncommitted settlement spends no free step"
    );
    assert!(
        router.paid_probe_candidates().is_empty(),
        "no candidate may be published from an uncommitted settlement"
    );
    assert!(router.all_recorded_paid_candidates_for_tests().is_empty());
    assert_eq!(router.probe_scheduler_snapshot().stale_settlements_total, 1);
}

#[tokio::test]
async fn a_committed_settlement_reports_that_it_committed() {
    // Positive control: the same settlement on live state DOES commit, so
    // the assertion above is about staleness rather than about `settle`
    // always returning false.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider, 3);
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let lease = router
        .lease_due_probe(std::time::Instant::now())
        .expect("job is due");

    let release = lease.settle(
        crate::probe_scheduler::ProbeSettlement::SpentFreeStep,
        std::time::Instant::now(),
    );

    assert!(release.committed());
    // The one-step free plan had no further step, so spending it ran the
    // plan out -- which is the fact that makes the paid class a candidate.
    assert!(release.exhausted_free_plan());
}

#[tokio::test]
async fn shutdown_during_an_outstanding_lease_publishes_no_candidate() {
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider, 3);
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let lease = router
        .lease_due_probe(std::time::Instant::now())
        .expect("job is due");

    router.shutdown_probe_work();
    let release = lease.settle(
        crate::probe_scheduler::ProbeSettlement::SpentFreeStep,
        std::time::Instant::now(),
    );

    assert!(!release.committed());
    assert!(!release.exhausted_free_plan());
    assert!(router.paid_probe_candidates().is_empty());
}

#[tokio::test]
async fn a_worker_run_retired_mid_flight_publishes_no_candidate() {
    // End-to-end form of the commit gate: the WORKER (not a hand-held
    // lease) runs a job whose incarnation is retired while its validator is
    // in flight. The settlement cannot commit, so no candidate may be
    // published -- one would describe router state already gone.
    //
    // The provider performs the retirement from inside the call, which is
    // the only way to land it strictly between lease and settle.
    struct RetiringProvider {
        router: parking_lot::Mutex<Option<std::sync::Weak<Router>>>,
    }

    #[async_trait::async_trait]
    impl Provider for RetiringProvider {
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
            // Retire the incarnation the leased job belongs to, mid-flight.
            if let Some(weak) = self.router.lock().as_ref()
                && let Some(router) = weak.upgrade()
            {
                router.publish_probe_incarnation();
            }
            // A well-formed zero: a SPENT step, which is what would
            // otherwise exhaust the plan and publish a candidate.
            Ok(TokenCount {
                input_tokens: 0,
                extras: serde_json::Map::new(),
            })
        }
    }

    let provider = Arc::new(RetiringProvider {
        router: parking_lot::Mutex::new(None),
    });
    let router = Arc::new(remote_router_with_paid_cap(provider.clone(), 3));
    *provider.router.lock() = Some(Arc::downgrade(&router));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1, "premise: the worker leased and ran the job");
    assert_eq!(
        router.probe_scheduler_snapshot().stale_settlements_total,
        1,
        "premise: the settlement must have arrived against retired state"
    );
    assert!(
        router.paid_probe_candidates().is_empty(),
        "an uncommitted settlement must publish no paid candidate"
    );
    // Asserted on the RAW list: the live-incarnation filter on the reader
    // above would hide a wrongly-recorded candidate (it carries the retired
    // incarnation), which would make this guard unfalsifiable.
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "nothing may even be RECORDED from an uncommitted settlement"
    );
}

// ---------------------------------------------------------------------

#[tokio::test]
async fn a_bad_request_probe_stamps_no_last_outcome_on_the_lane() {
    // The breaker is ENABLED at a threshold of 1 here (see
    // `router_on_base_with_failure_threshold`), so a mutation that debited
    // `ProbeRequestRefused` would both trip the circuit and stamp an outcome.
    let provider = Arc::new(FailingProvider {
        status: 400,
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
        gate.last_outcome, None,
        "routectl's own bad request must not be stamped as the lane's outcome"
    );
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::Closed,
        "and must not trip the breaker at a threshold of one"
    );
}

#[tokio::test]
async fn a_zero_count_probe_stamps_no_last_outcome_on_the_lane() {
    // The credit direction: a well-formed zero is an ANSWER, so a mutation
    // that recorded a success for `Inconclusive` would stamp `Ok` here.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(provider.calls.load(Ordering::SeqCst) > 0, "premise: dialed");
    assert_eq!(
        router.gate_status_for_tests("m1").last_outcome,
        None,
        "a probe answer must not be credited to the client-traffic breaker"
    );
}

#[tokio::test]
async fn further_overflow_inside_one_episode_emits_no_second_warn() {
    // Repeated overflow after the first emits NO further line, while the
    // counter keeps rising.
    //
    // The jobs are QUEUED before capacity fills, which is what makes the test
    // non-vacuous: activation is REFUSED once saturated, so overflowing by
    // activating more identities would overflow nothing inside the capture and
    // the "no second line" assertion would hold trivially. Terminal
    // settlements are what overflow, so they must all land inside it.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes(provider, OVERFLOW_LANES);

    // Fill capacity exactly, then queue several MORE identities while
    // activation is still admitting.
    settle_terminally(&router, PROBE_TOMBSTONE_CAPACITY).await;
    assert!(
        !router.probe_scheduler_snapshot().tombstone_saturated,
        "premise: exactly at capacity is not yet saturated"
    );
    let mut queued = 0usize;
    for n in PROBE_TOMBSTONE_CAPACITY..PROBE_TOMBSTONE_CAPACITY + 6 {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        if router.activate_probe_lane(&key, ProbeValidator::CountTokens) == ProbeActivation::Queued
        {
            queued += 1;
        }
    }
    assert!(
        queued >= 2,
        "premise: at least two queued jobs must settle inside the capture, or \
         there is no repeated overflow to observe (queued {queued})"
    );
    let saturations_before = router
        .probe_scheduler_snapshot()
        .tombstone_saturations_total;

    // Settle them all inside the capture: every settlement overflows.
    let ((), events) = routectl_testkit::with_capture(async {
        for _ in 0..=queued {
            router.run_due_probes().await;
        }
    })
    .await;

    let snap = router.probe_scheduler_snapshot();
    assert!(
        snap.tombstone_saturated,
        "premise: the settlements must have overflowed capacity"
    );
    assert!(
        snap.tombstone_saturations_total > saturations_before + 1,
        "premise: MORE THAN ONE overflow must have occurred inside the capture \
         ({} -> {}), or a single WARN would be trivially correct",
        saturations_before,
        snap.tombstone_saturations_total
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == crate::probe_scheduler::PROBE_TOMBSTONE_SATURATED_EVENT)
            .count(),
        1,
        "several overflows inside one episode must still emit exactly one line"
    );
}

#[tokio::test]
async fn an_unavailable_lane_is_terminal_for_the_incarnation_and_dials_once() {
    // `Unavailable` -- no resolved seat for the identity -- is terminal for the
    // same reason a refusal of routectl's own request is: seat resolution is
    // read from state FIXED within a published Router incarnation. A lane with
    // no seat now has none for every later request against this Router, so
    // re-asking cannot produce a different answer; it only re-queues and
    // re-refuses, once per admitted request.
    //
    // Driven by activating an identity whose state key names no installed
    // model, then running the worker. No dial is possible, so the observable is
    // the marker and the refusal of the next activation.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone() as Arc<dyn routectl_core::Provider>);
    let unresolvable =
        FieldVerdictKey::new("m-no-such-seat", GROUNDED_PATH, "anthropic-api").expect("identity");

    router.activate_probe_lane(&unresolvable, ProbeValidator::CountTokens);
    router.run_due_probes().await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "premise: an unresolvable identity cannot dial at all"
    );
    assert_eq!(
        snap.abandoned_probe_requests_total, 1,
        "premise: the settlement must be an abandonment"
    );
    assert_eq!(
        snap.tombstoned, 1,
        "an unavailable lane must lay a terminal marker for the incarnation"
    );
    assert_eq!(
        router.activate_probe_lane(&unresolvable, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned,
        "and later activations must be refused by it, not re-queued"
    );

    // Bounded in TIME by the incarnation: publication is exactly the event that
    // can change seat resolution, and it clears the markers.
    router.publish_probe_incarnation();
    assert_eq!(
        router.activate_probe_lane(&unresolvable, ProbeValidator::CountTokens),
        ProbeActivation::Queued,
        "a republished incarnation may ask again"
    );
}

#[tokio::test]
async fn a_transient_upstream_failure_on_a_closed_breaker_retries_without_a_marker() {
    // POSITIVE CONTROL for the terminal rule across both abandonment causes: a
    // RETRYABLE outcome must not be treated as persistent. Without this pairing
    // the scheduler could tombstone every outcome and both terminal tests above
    // would stay green.
    //
    // The breaker must be DISABLED for this to measure what it claims. Measured:
    // with `circuit_failures = Some(1)` the admitted dispatch's own 503 opens the
    // breaker, so the probe is gate-DEFERRED before dialing -- zero dials, one
    // deferral -- and the assertions below would pass on a path that never asked
    // the upstream anything. `None` leaves the breaker closed, so the probe
    // actually dials, actually receives the 503, and actually settles
    // `Retryable`.
    let provider = Arc::new(FailingProvider {
        status: 503,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base_with_failure_threshold(
        "https://api.anthropic.com",
        provider.clone() as Arc<dyn routectl_core::Provider>,
        None,
    );

    let _ = router.complete(grounding_request()).await;
    router.run_due_probes().await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        1,
        "premise: the probe must have DIALED and received the 503, not been \
         deferred before asking"
    );
    assert_eq!(
        snap.deferrals_total, 0,
        "premise: this is a retry path, not the gate-deferral path"
    );
    assert_eq!(snap.tombstoned, 0, "a transient failure lays no marker");
    assert_eq!(snap.backing_off, 1, "the job stays retryable");
    assert_eq!(
        snap.abandoned_probe_requests_total, 0,
        "and is not an abandonment at all"
    );
}
