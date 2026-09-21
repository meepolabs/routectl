//! The production probe worker: what it executes, what it settles, and
//! what it refuses to spend.
//!
//! The contract these pin is the paid boundary. A free step is spent only
//! when the lane actually took the question, and only a plan that spends its
//! LAST free step makes the paid class a candidate -- so every outcome that
//! did not ask (a malformed probe, an unsupported lane, a gate refusal, an
//! unmodelled failure class) must leave the plan where it was.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};

use super::Router;
use super::probe_lifecycle::PROBE_ACTIVATION_REFUSED_EVENT;
use super::probe_test_support::{
    BoxStreamAlias, FailingProvider, GROUNDED_PATH, OkProvider, ZeroCountProvider,
    grounding_request, remote_router, remote_router_with_lanes_and_paid_cap,
    remote_router_with_paid_cap,
};
use crate::config::Config;
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{
    FreeValidatorOutcome, PROBE_MAX_CONCURRENCY, PROBE_OPERATION_TIMEOUT, PROBE_QUEUE_DEPTH,
    ProbeValidator,
};

#[tokio::test]
async fn the_worker_executes_a_free_validator_and_resolves_the_job() {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let _ = router.complete(grounding_request()).await;
    assert_eq!(router.probe_scheduler_snapshot().queued, 1);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1, "the worker must lease and run the queued job");
    assert!(
        provider.count_calls.load(Ordering::SeqCst) > 0,
        "the free count-token validator must have been executed"
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.in_flight, 0, "the worker releases its slot");
    assert_eq!(snap.queued, 0);
}

#[tokio::test]
async fn a_spent_free_plan_surfaces_a_paid_candidate_and_never_dials_one() {
    // A well-formed zero SPENDS the step without answering, which is the
    // only shape that exhausts the plan: a 400 abandons (routectl's own
    // request was wrong) and a 5xx retries, neither of which may advance
    // toward paid eligibility.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider.clone(), 3);
    let _ = router.complete(grounding_request()).await;
    let calls_after_dispatch = provider.calls.load(Ordering::SeqCst);

    let executed = router.run_due_probes_recording().await;

    // Every executed validator was free, and none was paid.
    assert!(!executed.is_empty(), "the worker executed no validator");
    for validator in &executed {
        assert!(
            validator.is_free(),
            "the worker executed a paid validator: {validator:?}"
        );
    }
    assert_eq!(
        router.probe_scheduler_snapshot().free_exhausted_total,
        1,
        "the spent step must exhaust the one-step free plan"
    );

    // The paid class is a CANDIDATE only, never dialed: the only provider
    // calls are the count-token executions the free plan accounts for.
    let candidates = router.paid_probe_candidates();
    assert!(
        !candidates.is_empty(),
        "an exhausted free plan must surface a paid candidate"
    );
    for candidate in &candidates {
        assert_eq!(candidate.validator, ProbeValidator::PaidCompletion);
    }
    let count_token_executions = executed
        .iter()
        .filter(|v| **v == ProbeValidator::CountTokens)
        .count();
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        calls_after_dispatch + count_token_executions,
        "a provider call the free plan cannot account for means a paid dial fired"
    );
}

#[tokio::test]
async fn a_bad_request_on_routectls_own_probe_abandons_without_a_candidate() {
    // A 400/401 on a body routectl built is a defect in the
    // PROBE, not evidence about the capability. It must not spend a free
    // step, and must never reach paid eligibility -- the cap here is
    // non-zero precisely so a candidate WOULD be recorded if the outcome
    // were treated as a spent step.
    let provider = Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider.clone(), 3);
    let _ = router.complete(grounding_request()).await;

    router.run_due_probes().await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.abandoned_probe_requests_total, 1,
        "a refused probe request must be recorded as abandoned"
    );
    assert_eq!(
        snap.free_exhausted_total, 0,
        "an abandoned probe must not spend a free step"
    );
    assert!(
        router.paid_probe_candidates().is_empty(),
        "an abandoned probe must never surface a paid candidate"
    );
}

#[tokio::test]
async fn an_auth_failure_on_the_probe_request_also_abandons() {
    let provider = Arc::new(FailingProvider {
        status: 401,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider.clone(), 3);
    let _ = router.complete(grounding_request()).await;

    router.run_due_probes().await;

    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .abandoned_probe_requests_total,
        1
    );
    assert!(router.paid_probe_candidates().is_empty());
}

#[tokio::test]
async fn a_transient_failure_retries_the_step_rather_than_spending_it() {
    // Positive control for the two abandonment tests: a 5xx takes a
    // DIFFERENT path, so their assertions are about the bad-request class
    // rather than about every failure being abandoned.
    let provider = Arc::new(FailingProvider {
        status: 503,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider.clone(), 3);
    let _ = router.complete(grounding_request()).await;

    router.run_due_probes().await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.abandoned_probe_requests_total, 0);
    assert_eq!(snap.free_exhausted_total, 0);
    assert_eq!(
        snap.backing_off, 1,
        "a transient fault must leave the step retryable"
    );
}

#[tokio::test(start_paused = true)]
async fn a_wedged_validator_is_cancelled_at_the_timeout_and_frees_real_concurrency() {
    // The timeout must END the future, not merely mark the slot free: a
    // released slot whose operation is still running would let real
    // in-flight work exceed the concurrency bound with the counter never
    // showing it. The provider-owned LIVE count is what discriminates --
    // it can only fall back to zero if the future was actually dropped.
    let live = Arc::new(AtomicUsize::new(0));
    let router = remote_router(Arc::new(StallingProvider {
        live: Arc::clone(&live),
    }));
    for n in 0..PROBE_MAX_CONCURRENCY + 2 {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    }

    // Paused clock: the worker's own timeout is what has to end this, so
    // if cancellation is missing the call hangs rather than passing.
    let ran = router.run_due_probes().await;

    assert!(ran > 0, "the worker leased at least one job");
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "a cancelled operation must no longer be running once its slot is freed"
    );
    assert!(
        router.probe_scheduler_snapshot().timeouts_total > 0,
        "the timeout must be recorded, not silently absorbed"
    );
}

/// A provider whose `count_tokens` stalls far past the operation
/// timeout, tracking how many of its calls are still LIVE. The live
/// count is what distinguishes a real cancellation from a slot merely
/// marked free while the operation keeps running.
struct StallingProvider {
    live: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for StallingProvider {
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
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        struct Live(Arc<AtomicUsize>);
        impl Drop for Live {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        let _live = Live(Arc::clone(&self.live));
        tokio::time::sleep(PROBE_OPERATION_TIMEOUT * 10).await;
        Ok(TokenCount {
            input_tokens: 1,
            extras: serde_json::Map::new(),
        })
    }
}

#[tokio::test(start_paused = true)]
async fn the_worker_leases_no_more_than_the_concurrency_bound_at_once() {
    let live = Arc::new(AtomicUsize::new(0));
    let router = remote_router(Arc::new(StallingProvider {
        live: Arc::clone(&live),
    }));
    for n in 0..PROBE_QUEUE_DEPTH {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    }

    let ran = router.run_due_probes().await;

    assert!(
        ran <= PROBE_MAX_CONCURRENCY,
        "one worker pass ran {ran} operations against a bound of {PROBE_MAX_CONCURRENCY}"
    );
}

#[tokio::test]
async fn the_worker_runs_nothing_for_a_retired_incarnation() {
    let previous = Router::new(Arc::new(Config::default()));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    previous.activate_probe_lane(&key, ProbeValidator::CountTokens);
    let mut next = Router::new(Arc::new(Config::default()));
    next.carry_over_learned_from(&previous);
    next.publish_probe_incarnation();

    let ran = next.run_due_probes().await;

    assert_eq!(ran, 0, "retired work must never execute");
}

#[tokio::test]
async fn a_paid_candidate_refused_for_capacity_is_counted_rather_than_dropped_silently() {
    // The candidate list is bounded, and a refusal at that bound must be
    // OBSERVABLE. It costs no upstream call -- a candidate is not permission to
    // spend -- but it loses the only signal an exhausted lane produces, so an
    // operator reading the snapshot has to be able to tell a complete list from
    // a truncated one.
    //
    // Driven through the real worker: each lane's well-formed zero spends its
    // one free step, exhausting the plan and making it a candidate. Past the
    // bound the recording refuses.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_lanes_and_paid_cap(provider, PROBE_QUEUE_DEPTH + 4, 3);

    // NEGATIVE control first: inside the bound nothing is refused.
    for n in 0..PROBE_QUEUE_DEPTH {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
        router.run_due_probes().await;
    }
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        PROBE_QUEUE_DEPTH,
        "premise: the list must actually be AT its bound before overflowing it"
    );
    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .paid_candidate_capacity_refusals_total,
        0,
        "NEGATIVE control: nothing refused while capacity remains"
    );

    // POSITIVE: past the bound, each exhausted lane is refused and counted.
    for n in PROBE_QUEUE_DEPTH..PROBE_QUEUE_DEPTH + 4 {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
        router.run_due_probes().await;
    }

    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        PROBE_QUEUE_DEPTH,
        "the list stays at its bound"
    );
    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .paid_candidate_capacity_refusals_total,
        4,
        "every refused candidate must be counted, not silently dropped"
    );
}

#[tokio::test]
async fn a_zero_cap_router_records_no_candidate_though_the_free_plan_is_spent() {
    // Fail-closed default: `[fidelity]` caps default to zero, so an
    // exhausted free plan on an un-opted-in provider surfaces nothing. The
    // positive direction is
    // `a_spent_free_plan_surfaces_a_paid_candidate_and_never_dials_one`,
    // whose only difference from this fixture is the cap.
    let provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    let _ = router.complete(grounding_request()).await;

    router.run_due_probes().await;

    assert!(
        router.paid_probe_candidates().is_empty(),
        "a zero cap must record no paid candidate"
    );
    assert!(
        router.probe_scheduler_snapshot().free_exhausted_total > 0,
        "premise: the free plan must actually have been exhausted"
    );
}

#[test]
fn a_settled_free_outcome_makes_no_paid_candidate() {
    // The paid predicate is the worker's own gate, exercised directly for
    // the outcome the worker cannot manufacture on a dry fixture.
    assert!(!crate::probe_scheduler::paid_probe_permitted(
        FreeValidatorOutcome::Settled,
        5
    ));
    assert!(crate::probe_scheduler::paid_probe_permitted(
        FreeValidatorOutcome::Exhausted,
        5
    ));
}

// ---------------------------------------------------------------------
// The bounded queue-full diagnostic
// ---------------------------------------------------------------------

#[test]
fn repeated_queue_full_refusals_warn_once_but_keep_counting() {
    let router = Router::new(Arc::new(Config::default()));
    for n in 0..PROBE_QUEUE_DEPTH {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    }

    let events = routectl_testkit::capture_events(|| {
        for n in 0..40 {
            let key =
                FieldVerdictKey::new(&format!("overflow-{n}"), GROUNDED_PATH, "anthropic-api")
                    .expect("identity");
            router.activate_probe_lane(&key, ProbeValidator::CountTokens);
        }
    });

    let warns = events
        .iter()
        .filter(|e| e.message == PROBE_ACTIVATION_REFUSED_EVENT)
        .count();
    assert_eq!(
        warns, 1,
        "a saturated queue must warn once, not once per refused request"
    );
    assert_eq!(
        router.probe_scheduler_snapshot().queue_full_total,
        40,
        "the counter carries the suppressed volume"
    );
}

#[test]
fn the_bounded_queue_full_warn_still_fires_for_the_first_refusal() {
    // Positive control for the bound above: the latch must not suppress
    // the FIRST line, or the diagnostic would never appear at all.
    let router = Router::new(Arc::new(Config::default()));
    for n in 0..PROBE_QUEUE_DEPTH {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    }

    let events = routectl_testkit::capture_events(|| {
        let key =
            FieldVerdictKey::new("overflow", GROUNDED_PATH, "anthropic-api").expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    });

    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == PROBE_ACTIVATION_REFUSED_EVENT)
            .count(),
        1
    );
}

// ---------------------------------------------------------------------

#[test]
fn an_unsupported_capability_rejection_spends_no_free_step() {
    // The upstream says the capability is not supported HERE. That is an
    // answer about the lane, but not one this stage may spend a step on:
    // `count_tokens` is the only executable free validator in this build, so
    // spending it would exhaust the plan and hand the lane to the PAID class on
    // the strength of a question the upstream refused to take.
    //
    // What this pins is the EXPLICIT arm, which is redundant with the
    // fail-closed catch-all that would refuse this class anyway. That is the
    // point of having both: the explicit arm is an AUDITED-CLASS RECORD -- it
    // says someone considered `FeatureUnsupported` and chose refusal, rather
    // than it falling through to the default by nobody's decision. This test
    // is what keeps that record honest if the default ever changes.
    //
    // Driven through the CLASS rather than an error fixture: the anthropic
    // token table carries an empty `feature_unsupported` set (this class
    // reaches the lane only via the reasoning-replay path), so no provider
    // double can produce one -- such a fixture would classify as a plain
    // `BadRequest` and exercise a DIFFERENT refusal arm while appearing to pin
    // this one.
    let unsupported = routectl_core::failure_class::FailureClass::FeatureUnsupported {
        capability: "thinking".to_string(),
    };

    assert_eq!(
        super::probe_failure_class::probe_outcome_for_class_for_tests(unsupported),
        FreeValidatorOutcome::ProbeRequestRefused,
        "an unsupported-capability rejection must be refused, not spent -- \
         spending it would make the lane a paid candidate"
    );

    // Control: a class that SHOULD retry still retries, so the assertion above
    // is about this class rather than about every class being refused.
    assert_eq!(
        super::probe_failure_class::probe_outcome_for_class_for_tests(
            routectl_core::failure_class::FailureClass::ServerError
        ),
        FreeValidatorOutcome::Transient
    );
}

#[tokio::test]
async fn a_lane_with_no_resolved_seat_makes_no_paid_candidate() {
    // The `Unavailable` direction: the step could not run on this lane at
    // all, so it spends nothing. With `count_tokens` the only executable free
    // validator in this build, treating this as spent would make ONE
    // unresolvable lane a paid candidate immediately.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router_with_paid_cap(provider.clone(), 3);
    // An identity naming a state key no resolved model answers for, so seat
    // resolution finds nothing.
    let key =
        FieldVerdictKey::new("no-such-model", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "premise: there is no seat, so nothing may be dialed"
    );
    assert!(
        router.paid_probe_candidates().is_empty(),
        "a lane whose free validator could not run must not become a paid \
         candidate"
    );
    assert!(router.all_recorded_paid_candidates_for_tests().is_empty());
    assert_eq!(
        router.probe_scheduler_snapshot().free_exhausted_total,
        0,
        "no free step may have been spent"
    );
}

#[test]
fn an_unmodelled_failure_class_spends_no_free_step() {
    // The `#[non_exhaustive]` catch-all, stated at the classifier boundary
    // where every variant is reachable. A class this build has not audited
    // must fail CLOSED -- refused, not spent -- or a variant added upstream
    // could walk a lane to the paid class purely by existing.
    //
    // `Unknown` stands in for that set: it is the one unaudited-shaped class
    // this build can reach, and it takes the same catch-all arm any unmodelled
    // variant takes. Reached through an error variant the classifier has no
    // specific arm for, rather than through a status code (every status this
    // lane can return is classified explicitly).
    let err = Error::normalize_response("p1", "unparseable");
    let class = routectl_core::failure_class::classify(&err, Some("anthropic-api")).class;
    assert_eq!(
        class,
        routectl_core::failure_class::FailureClass::Unknown,
        "premise: this error must actually reach the unmodelled class, or the \
         assertion below is about a class the match names explicitly"
    );

    assert_eq!(
        super::probe_failure_class::classify_probe_failure_for_tests(&err),
        FreeValidatorOutcome::ProbeRequestRefused,
        "an unmodelled failure class must be refused rather than spent"
    );
}

#[tokio::test]
async fn a_beta_context_breaching_a_bound_is_counted_and_warned_once_rather_than_skipped() {
    // A refused payload means the lane is NOT probed. That refusal is correct --
    // a probe under a reduced or rewritten beta context asks a different
    // question than the admitted request posed -- but it is otherwise
    // INVISIBLE: no job is queued, nothing settles, and no tombstone is laid, so
    // an un-probed lane reads exactly like one that was never admitted.
    //
    // Driven through the real dispatch walk with an admitted request whose beta
    // set breaches the per-source count bound.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    let mut admitted = grounding_request();
    admitted.anthropic_beta = (0..=crate::probe_scheduler::PROBE_BETA_MAX_COUNT_PER_SOURCE)
        .map(|n| format!("beta-{n}"))
        .collect();

    let events = Box::pin(routectl_testkit::with_capture(async {
        router.complete(admitted).await
    }))
    .await
    .1;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.activations_total, 0,
        "a refused payload must activate nothing"
    );
    assert_eq!(
        snap.payload_refusals_total, 1,
        "and the refusal must be COUNTED, not silently skipped"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == crate::probe_scheduler::PROBE_PAYLOAD_REFUSED_EVENT)
            .count(),
        1,
        "the bounded diagnostic must fire for the first refusal"
    );

    // The WARN is latched per incarnation while the counter keeps rising: the
    // condition is a property of a client's beta set, so a client repeating it
    // would otherwise put a line on every one of its requests.
    let events = Box::pin(routectl_testkit::with_capture(async {
        for _ in 0..5 {
            let mut again = grounding_request();
            again.anthropic_beta = (0..=crate::probe_scheduler::PROBE_BETA_MAX_COUNT_PER_SOURCE)
                .map(|n| format!("beta-{n}"))
                .collect();
            let _ = router.complete(again).await;
        }
    }))
    .await
    .1;
    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == crate::probe_scheduler::PROBE_PAYLOAD_REFUSED_EVENT)
            .count(),
        0,
        "the line is latched per incarnation, so later refusals stay suppressed"
    );
    assert_eq!(
        router.probe_scheduler_snapshot().payload_refusals_total,
        6,
        "while the counter carries the suppressed volume"
    );
}

#[tokio::test]
async fn a_beta_context_inside_every_bound_is_never_counted_as_a_refusal() {
    // POSITIVE CONTROL for the counter above: a realistic beta set activates
    // normally and increments nothing. Without this, the counter could fire on
    // every request and the test above would still be green.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    let mut admitted = grounding_request();
    admitted.anthropic_beta =
        routectl_core::identity::anthropic::default_claude_code_anthropic_betas()
            .iter()
            .map(|b| (*b).to_string())
            .collect();

    let _ = router.complete(admitted).await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.activations_total, 1, "a real beta set must activate");
    assert_eq!(
        snap.payload_refusals_total, 0,
        "and must be counted as no refusal at all"
    );
}

#[tokio::test]
async fn an_unknown_display_value_refuses_the_payload_and_tombstones_nothing() {
    // THE denial-of-probing case. The display value is client-supplied text, and
    // the settlement a probe reaches is terminal for the incarnation -- so if an
    // unmodeled value could reach a probe, one client sending junk in that field
    // would tombstone the lane's identity and stop every other client's traffic
    // from ever being probed on it.
    //
    // Refused at CAPTURE instead: the lane activates nothing, dials nothing, and
    // lays no marker, so the identity stays probeable for the next admitted
    // request that carries a value this build models. The refusal is counted, so
    // the un-probed lane is not invisible.
    //
    // A SHORT junk value deliberately: the byte ceiling cannot catch it, so this
    // measures the vocabulary rule rather than the size rule.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let mut admitted = grounding_request();
    admitted.routectl_internal.anthropic_thinking_display = Some("junk".to_string());
    assert!(
        !crate::probe_scheduler::PROBE_MODELED_DISPLAY_VALUES.contains(&"junk"),
        "premise: the value must be outside the modeled vocabulary"
    );

    let _ = router.complete(admitted).await;
    let ran = router.run_due_probes().await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.activations_total, 0,
        "an unmodeled display value must activate nothing"
    );
    assert_eq!(ran, 0, "so the worker has nothing to run");
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "and nothing is dialed"
    );
    assert_eq!(
        snap.tombstoned, 0,
        "NO terminal marker: the identity must stay probeable, or one client's \
         junk would stop the lane being probed for everyone"
    );
    assert_eq!(
        snap.payload_refusals_total, 1,
        "the refusal must be counted, so an un-probed lane is not invisible"
    );

    // And the identity IS still probeable: the same lane, with a modeled value,
    // activates and dials. This is what makes "no tombstone" mean something.
    let mut modeled = grounding_request();
    modeled.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    let _ = router.complete(modeled).await;
    assert_eq!(
        router.probe_scheduler_snapshot().activations_total,
        1,
        "a later modeled value on the same lane must still activate"
    );
    assert_eq!(router.run_due_probes().await, 1);
    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        1,
        "and must reach the upstream"
    );
}
