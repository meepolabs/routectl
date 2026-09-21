// The pre-dial gate, the call count, and what a paid dial must NOT settle.
//
// An `include!`d FRAGMENT of `paid_probe_dial_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason --
// imports live in the host.

// ---------------------------------------------------------------------------
// The gate still decides, and a refusal consumes the committed unit
// ---------------------------------------------------------------------------

/// A breaker cooldown far longer than any test spends, so the lane stays OPEN
/// rather than lapsing into half-open mid-case. A test whose subject is the
/// half-open state arranges a short one of its own.
const BREAKER_COOLDOWN: Duration = Duration::from_mins(5);

#[tokio::test]
async fn a_gate_refusal_makes_zero_provider_calls_on_a_committed_unit() {
    // THE gate case, and its premise is what makes it sharp: the unit IS
    // committed (the recorder shows the reservation) and the call still does not
    // happen. A paid probe must not bypass an operator rate limit, must not dial
    // into an open breaker, and must not take the breaker's single half-open
    // recovery attempt -- recovery belongs to real traffic, which a probe that
    // settles nothing cannot answer for.
    //
    // The consumed unit is deliberate underuse: the gate is asked AFTER the
    // commit because the commit is what authorizes, and no release method exists.
    let dial = Dial::legacy(CompleteAnswer::Ok);
    assert!(
        dial.router.force_open_breaker(LANE, BREAKER_COOLDOWN),
        "fixture premise: the acting lane must have a breaker to open",
    );

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::GateDeferred);
    assert_eq!(
        dial.recorder.events(),
        vec![RESERVE],
        "the unit must be committed and NO call made: {:?}",
        dial.recorder.events(),
    );
    assert_eq!(dial.provider.calls(), 0, "the acting lane must not dial");
    assert_eq!(
        dial.fallback_provider.calls(),
        0,
        "and a gate refusal must not reach for another seat",
    );
    assert!(
        dial.router
            .all_recorded_paid_candidates_for_tests()
            .is_empty(),
        "a gate refusal must NOT requeue: the unit is spent, so a retry could \
         pair a second unit with the first",
    );
    assert_eq!(
        dial.router.probe_scheduler_snapshot().in_flight,
        0,
        "and the authorization's slot must be released",
    );
}

#[tokio::test]
async fn a_half_open_ready_lane_defers_and_leaves_its_recovery_attempt() {
    // The half-open half of the gate, distinct from a lane still inside its
    // cooldown: the attempt IS available here, and the probe must decline it
    // rather than spend the breaker's one recovery signal on background work.
    let dial = Dial::legacy(CompleteAnswer::Ok);
    assert!(
        dial.router
            .force_open_breaker(LANE, Duration::from_millis(1)),
        "fixture premise: the lane must have a breaker to open",
    );
    tokio::time::sleep(Duration::from_millis(5)).await;

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::GateDeferred);
    assert_eq!(dial.provider.calls(), 0);
    let gate = dial.router.gate_status_for_tests(LANE);
    assert!(
        !gate.half_open_probe_in_flight,
        "the declined claim must come straight back, or the breaker latches open \
         and no real request can recover the lane",
    );
    assert_eq!(
        gate.circuit,
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "the lane must still be offering its recovery attempt to real traffic",
    );
}

#[tokio::test]
async fn a_closed_breaker_dials_which_is_the_gate_cases_positive_control() {
    // POSITIVE CONTROL for both gate cases: the SAME fixture with the breaker
    // CLOSED dials exactly once. Without it, "zero provider calls" above would be
    // satisfied by a paid path that never dials at all.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::Completed);
    assert_eq!(
        dial.provider.calls(),
        1,
        "through a closed breaker the paid probe must dial",
    );
    assert!(
        !dial
            .router
            .gate_status_for_tests(LANE)
            .half_open_probe_in_flight,
        "a closed-breaker admission claims nothing to begin with",
    );
}

// ---------------------------------------------------------------------------
// Exactly one call: no retry, no fallback, no provider loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_dial_calls_the_seats_provider_exactly_once() {
    let dial = Dial::legacy(CompleteAnswer::Ok);

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::Completed);
    assert_eq!(dial.recorder.count(COMPLETE), 1, "one call, once");
    assert_eq!(
        dial.fallback_provider.calls(),
        0,
        "the alias chain names a second lane, and a paid probe must not walk it",
    );
}

#[tokio::test]
async fn a_failing_dial_is_not_retried_and_never_falls_back() {
    // An upstream error is an ANSWER for this stage, not a reason to spend the
    // committed unit twice. The fixture's alias chain has a healthy second lane,
    // so a fallback would succeed -- which is exactly why its zero count is
    // evidence rather than a vacuous absence.
    for status in [429_u16, 500, 400] {
        let dial = Dial::legacy(CompleteAnswer::Failing(status));

        let outcome = dialed(&dial).await;

        assert!(
            matches!(outcome, PaidProbeDialOutcome::ProviderFailed(_)),
            "status {status} must report a provider failure: {outcome:?}",
        );
        assert_eq!(
            dial.provider.calls(),
            1,
            "status {status}: exactly one attempt, never a retry",
        );
        assert_eq!(
            dial.fallback_provider.calls(),
            0,
            "status {status}: and never the healthy second lane",
        );
        assert_eq!(
            dial.recorder.count(RESERVE),
            1,
            "status {status}: nor a second reservation",
        );
        assert!(
            dial.router
                .all_recorded_paid_candidates_for_tests()
                .is_empty(),
            "status {status}: a spent unit must not requeue the candidate",
        );
    }
}

// ---------------------------------------------------------------------------
// The breaker is neither credited nor debited, matching free probes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_paid_dial_settles_nothing_on_the_breaker_on_success_or_failure() {
    // A background probe is not client traffic. A probe success would close a
    // breaker the operator's own requests have not proven healthy, and a probe
    // failure would open or re-trip a lane serving clients fine.
    //
    // The fixture's failure threshold is ONE, so a single debit WOULD trip the
    // breaker -- which is what makes both assertions falsifiable.
    for answer in [CompleteAnswer::Ok, CompleteAnswer::Failing(503)] {
        let dial = Dial::legacy(answer);

        let outcome = dialed(&dial).await;

        assert!(
            matches!(
                outcome,
                PaidProbeDialOutcome::Completed | PaidProbeDialOutcome::ProviderFailed(_)
            ),
            "premise: the dial must have happened: {outcome:?}",
        );
        assert_eq!(dial.provider.calls(), 1, "premise: dialed");
        let gate = dial.router.gate_status_for_tests(LANE);
        assert_eq!(
            gate.circuit,
            crate::runtime_state::CircuitPhase::Closed,
            "a paid probe must not trip the client-traffic breaker, even at a \
             threshold of one",
        );
        assert_eq!(
            gate.last_outcome, None,
            "nor stamp its own outcome as the lane's last",
        );
        assert!(
            !gate.half_open_probe_in_flight,
            "nor leave a half-open claim behind",
        );
        assert!(
            dial.router.gate_check(LANE, PROVIDER).is_none(),
            "and the next REAL request must still be admitted",
        );
    }
}
