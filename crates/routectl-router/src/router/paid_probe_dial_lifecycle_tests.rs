// The call's bounds and its lifecycle: the operation timeout, cancellation while
// the call is in flight, and that a reload or lowered cap landing AFTER the commit
// cancels nothing.
//
// An `include!`d FRAGMENT of `paid_probe_dial_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason --
// imports live in the host.

// ---------------------------------------------------------------------------
// The one call is bounded
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_never_answering_upstream_expires_at_the_probe_operation_bound() {
    // A provider that never answers must not strand the held slot and the spent
    // unit forever. The bound is the SHARED probe operation timeout, the same one
    // the free worker wraps its validator in, so a paid call cannot outlive a free
    // one by taking its own.
    //
    // On a PAUSED clock, so the bound is exercised deterministically rather than
    // by waiting out real seconds, and under the test's OWN generous wait so a
    // missing production bound fails by name instead of stalling the suite.
    let dial = Dial::legacy(CompleteAnswer::Pending);

    let pass = tokio::time::timeout(PROBE_OPERATION_TIMEOUT * 4, dial.router.run_paid_probe())
        .await
        .expect("the call did not expire at its bound -- the dial await is unbounded");

    assert!(
        matches!(pass, PaidProbePass::Dialed(PaidProbeDialOutcome::TimedOut)),
        "a wedged upstream must expire, not hang: {pass:?}",
    );
    assert_eq!(
        dial.provider.calls(),
        1,
        "premise: the call WAS submitted -- which is why expiry is an outcome \
         rather than a clean refusal",
    );
    assert_eq!(
        dial.fallback_provider.calls(),
        0,
        "and a timeout must not reach for another seat",
    );
    assert!(
        dial.router
            .all_recorded_paid_candidates_for_tests()
            .is_empty(),
        "a timeout must NOT requeue: the unit is spent and the upstream may yet \
         have received the call",
    );
    assert_eq!(
        dial.router.probe_scheduler_snapshot().in_flight,
        0,
        "and the slot must be released on the expiry path",
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_pass_mid_call_releases_the_slot_and_requeues_nothing() {
    // Cancellation in the shape production produces it: the whole pass future is
    // DROPPED while the call is in flight. The slot comes back because the
    // authorization's `Drop` owns it; the unit does not, because no release method
    // exists.
    //
    // Zero or one call is acceptable -- the drop may land before or after the
    // submission -- but a SECOND call never is.
    let dial = Dial::legacy(CompleteAnswer::Pending);

    {
        let mut pass = Box::pin(dial.router.run_paid_probe());
        let polled = futures::poll!(&mut pass);
        assert!(polled.is_pending(), "premise: the pass must be mid-flight");
    }

    assert!(
        dial.provider.calls() <= 1,
        "a dropped pass must never have dialed twice: {}",
        dial.provider.calls(),
    );
    assert_eq!(dial.fallback_provider.calls(), 0, "and never a second seat");
    assert_eq!(
        dial.recorder.count(RESERVE),
        1,
        "the unit was committed once, and cancellation neither un-commits nor \
         re-reserves it",
    );
    assert!(
        dial.router
            .all_recorded_paid_candidates_for_tests()
            .is_empty(),
        "a dropped pass must not requeue a candidate whose unit is spent",
    );
    assert_eq!(
        dial.router.probe_scheduler_snapshot().in_flight,
        0,
        "and its paid slot must be released on the drop path",
    );
}

// ---------------------------------------------------------------------------
// A commit is final: later state changes do not cancel the call
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_and_a_cap_lowering_after_the_commit_do_not_cancel_the_call() {
    // The unit is spent by the time the call is made, so abandoning the call
    // afterwards would pay for nothing. A reload and a cap lowering are precisely
    // the two changes that would refuse a FRESH attempt -- and the fresh attempt
    // at the end is what keeps this case about the IN-FLIGHT call rather than
    // about a router that would dial regardless.
    let dial = Dial::legacy(CompleteAnswer::Slow);
    let provider = Arc::clone(&dial.provider);
    let recorder = dial.recorder.clone();
    let router = Arc::new(dial.router);

    let pass = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.run_paid_probe().await })
    };
    // Wait until the call is provably in flight before changing anything.
    provider.wait_until_called().await;

    // A PUBLICATION, landing while the call is outstanding. It supersedes the
    // router under test without touching the committed unit.
    let mut next = Router::new(Arc::clone(&router.config));
    next.carry_over_learned_from(&router);
    next.publish_probe_incarnation();
    assert!(
        !router.is_current_publication(),
        "premise: the router under test must be superseded mid-call",
    );

    provider.release();
    let pass = pass.await.expect("the dialing task must not panic");

    assert!(
        matches!(pass, PaidProbePass::Dialed(PaidProbeDialOutcome::Completed)),
        "a publication landing after the commit must not cancel the call: {pass:?}",
    );
    assert_eq!(
        recorder.events(),
        vec![RESERVE, COMPLETE],
        "one reservation, one call, in that order",
    );

    // THE CONTROL: on the SAME router, with the cap lowered to zero, a fresh pass
    // refuses at the cap -- so the assertions above are about the committed call
    // rather than about a permissive router.
    let mut router = Arc::try_unwrap(router)
        .ok()
        .expect("the dialing task has finished, so this is the sole owner");
    let mut lowered = (*router.config).clone();
    lowered
        .fidelity
        .paid_probe_daily_caps
        .insert(PROVIDER.to_string(), 0);
    router.config = Arc::new(lowered);
    router.publish_probe_incarnation();
    router
        .paid_probe_candidates
        .lock()
        .push_back(PaidProbeCandidate {
            key: key(),
            incarnation: router.probe_incarnation(),
            validator: ProbeValidator::PaidCompletion,
            payload: payload(),
        });

    let fresh = router.run_paid_probe().await;

    assert!(
        matches!(fresh, PaidProbePass::Refused(PaidProbeRefusal::CapZero)),
        "a fresh pass under the lowered cap must refuse at the cap: {fresh:?}",
    );
    assert_eq!(provider.calls(), 1, "and no second call may have been made");
}
