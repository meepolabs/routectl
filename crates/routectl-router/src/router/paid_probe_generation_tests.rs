// The publication-generation protocol: which of a publication, a shutdown, and a
// paid reservation wins, proven with a DETERMINISTICALLY PARKED ledger.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.
//
// The protocol is OPTIMISTIC: nothing blocks a publication or a shutdown, and the
// authorization abandons itself instead. So each test below drives the racing
// side to COMPLETION (synchronously, which is the point) and then asserts what the
// authorization did about it, rather than asserting that anything waited.

/// A ledger that PARKS inside its reservation until released, so a test can hold
/// an authorization at exactly the point a publication has to matter.
///
/// The park is the whole instrument. Without it the race is a timing accident:
/// the reservation would return before the racing publication ran, and the test
/// would pass against an unprotected implementation just as readily. With it the
/// authorization is provably mid-await when the publication happens, which is the
/// one interleaving the second generation check exists for.
struct ParkingLedger {
    /// Signalled once the reservation has been ENTERED, so the racing side can
    /// wait for that fact rather than sleeping and hoping.
    entered: tokio::sync::Semaphore,
    /// Awaited inside the reservation; the racing side adds a permit to let it
    /// finish. Never released at all by the timeout tests.
    release: tokio::sync::Semaphore,
    calls: AtomicUsize,
    answer: PaidProbeReservation,
}

impl ParkingLedger {
    fn answering(answer: PaidProbeReservation) -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            calls: AtomicUsize::new(0),
            answer,
        })
    }

    fn committing() -> Arc<Self> {
        Self::answering(PaidProbeReservation::Committed { used: 1, cap: CAP })
    }

    /// Wait until a reservation is parked inside the ledger.
    async fn wait_until_parked(&self) {
        let permit = self
            .entered
            .acquire()
            .await
            .expect("the entry signal must not be closed");
        permit.forget();
    }

    /// Let the parked reservation finish.
    fn release(&self) {
        self.release.add_permits(1);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl PaidProbeLedger for ParkingLedger {
    async fn reserve_paid_probe_unit(&self, _: &str, _: u32) -> PaidProbeReservation {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.add_permits(1);
        let permit = self
            .release
            .acquire()
            .await
            .expect("the release signal must not be closed");
        permit.forget();
        self.answer.clone()
    }
}

// ---------------------------------------------------------------------------
// Publication versus the ledger call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_publication_before_ledger_admission_costs_zero_ledger_calls() {
    // PUBLICATION FIRST. The pre-call generation check is what makes this free:
    // the attempt stops before the accounting layer is touched at all, so no unit
    // is committed for router state that no longer serves.
    //
    // The candidate is seeded at the incarnation the publication will leave
    // stamped, so the STALE check cannot be what refuses -- this has to be the
    // shared-generation check or the test proves nothing about it.
    let ledger = CountingLedger::committed();
    let router = Arc::new(Fixture::default().router_with_ledger(ledger.clone()));
    // Publish, then hand the candidate the NEW stamp, then supersede this router
    // by drawing a further ticket value from a replacement.
    router.publish_probe_incarnation();
    seed_candidate(&router, &key());
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&router);
    // The replacement's own publication is what supersedes the router under test.
    next.publish_probe_incarnation();
    assert!(
        !router.is_current_publication(),
        "premise: the router under test must be superseded",
    );
    // The replacement's publication cleared the SHARED list, so re-seed at the old
    // router's own still-matching stamp -- the shape an in-flight pass holds.
    seed_candidate_at(&router, &key(), router.probe_incarnation());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Superseded),
        "a superseded router must refuse before asking the accounting layer",
    );
    assert_eq!(
        ledger.calls(),
        0,
        "ZERO ledger calls: a publication that already landed must cost nothing",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "and the candidate is discarded, not requeued onto a cleared list",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "no concurrency slot may be left held",
    );
}

#[tokio::test]
async fn a_commit_that_wins_before_publication_yields_a_valid_authorization() {
    // COMMIT FIRST. Both generation checks pass, the authorization is
    // constructed, and a publication arriving AFTERWARDS does not invalidate it --
    // the unit is spent, so refusing to use it would pay for nothing.
    let ledger = CountingLedger::committed();
    let router = Arc::new(Fixture::default().router_with_ledger(ledger.clone()));
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    let PaidProbeOutcome::Authorized(auth) = outcome else {
        panic!("an uncontended attempt must authorize: {outcome:?}");
    };
    let authorized_at = auth.incarnation();
    assert_eq!(ledger.calls(), 1);

    // NOW publish, and lower the cap to zero -- the two changes that would refuse
    // a fresh attempt.
    let mut config = (*router.config).clone();
    config
        .fidelity
        .paid_probe_daily_caps
        .insert(PROVIDER.to_string(), 0);
    let mut router = Arc::try_unwrap(router).ok().expect("sole owner");
    router.config = Arc::new(config);
    router.publish_probe_incarnation();

    assert_eq!(
        auth.incarnation(),
        authorized_at,
        "the committed authorization still names the generation it was granted at",
    );
    assert_eq!(
        auth.committed().cap(),
        CAP,
        "and the cap its unit was committed under, not the lowered one",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "nothing re-reserved and nothing refunded",
    );
    // The control that keeps the assertions above from being about a router that
    // would authorize anything: a FRESH attempt now refuses.
    seed_candidate(&router, &key());
    let fresh = router.authorize_paid_probe().await;
    assert_eq!(fresh.refusal(), Some(PaidProbeRefusal::CapZero));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publication_during_the_await_supersedes_the_consumed_reservation() {
    // PUBLICATION INSIDE THE AWAIT, the interleaving the POST-ACKNOWLEDGEMENT
    // check exists for. The ledger parks, the publication runs to completion
    // SYNCHRONOUSLY while the reservation is provably in flight, and then the
    // reservation commits.
    //
    // The unit MAY be consumed -- it is, here -- and there must still be no
    // authorization: a call made on its strength would be a paid dial on behalf of
    // router state that no longer serves. The consumed unit is accepted underuse,
    // since the ledger has no release method.
    let ledger = ParkingLedger::committing();
    let router = Arc::new(Fixture::default().router_with_ledger_arc(ledger.clone()));
    seed_candidate(&router, &key());

    let authorizing = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.authorize_paid_probe().await })
    };
    ledger.wait_until_parked().await;
    assert_eq!(
        ledger.calls(),
        1,
        "premise: the reservation must be IN FLIGHT before the publication races it",
    );

    // THE PUBLICATION, synchronous and NOT waiting on the parked reservation --
    // which is the property this design has and a blocking one does not. It
    // completes here, inline, while the ledger call is still parked.
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&router);
    next.publish_probe_incarnation();
    assert!(
        !router.is_current_publication(),
        "the publication completed while the reservation was parked",
    );

    // Only now let the reservation answer.
    ledger.release();
    let outcome = authorizing.await.expect("the authorizer must not panic");

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Superseded),
        "a commit acknowledged after a publication must NOT authorize a call",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "the reservation was submitted once, and stays consumed -- there is no \
         release method, and underuse is the cheap side of this trade",
    );
    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .paid_reservations_committed_total,
        1,
        "and the spend is COUNTED, because the commit -- not the pass's final \
         refusal -- is where the unit became irreversible",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a superseded attempt must not requeue onto a list the publication cleared",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "and its concurrency slot must be released",
    );
}

// ---------------------------------------------------------------------------
// Shutdown versus the ledger call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_shutdown_before_ledger_admission_costs_zero_ledger_calls() {
    // The shutdown counterpart of the pre-call check. `shutdown_probe_work`
    // advances the shared ticket to a terminal generation NO router is stamped
    // with, so every live router immediately reads itself superseded.
    let ledger = CountingLedger::committed();
    let router = Arc::new(Fixture::default().router_with_ledger(ledger.clone()));
    seed_candidate(&router, &key());
    let stamped = router.probe_incarnation();

    router.shutdown_probe_work();

    assert!(
        !router.is_current_publication(),
        "shutdown must supersede every live router",
    );
    assert_eq!(
        router.probe_incarnation(),
        stamped,
        "and it does so WITHOUT restamping this router -- the terminal generation \
         is unstamped, so nothing can become current again (no ABA)",
    );
    // Re-seed at the router's own unchanged stamp: the incarnation check cannot be
    // what refuses, so this exercises the shared-generation read.
    seed_candidate_at(&router, &key(), stamped);

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::Superseded));
    assert_eq!(
        ledger.calls(),
        0,
        "ZERO ledger calls after a shutdown has advanced the generation",
    );
    assert!(router.all_recorded_paid_candidates_for_tests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_during_the_await_supersedes_and_never_requeues() {
    // Shutdown inside the await. The refusal must not requeue a retained payload
    // into a daemon that is going away -- and the shutdown must not have waited for
    // the parked reservation to get there.
    let ledger = ParkingLedger::committing();
    let router = Arc::new(Fixture::default().router_with_ledger_arc(ledger.clone()));
    seed_candidate(&router, &key());

    let authorizing = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.authorize_paid_probe().await })
    };
    ledger.wait_until_parked().await;

    // SYNCHRONOUS shutdown, completing inline while the reservation is parked.
    router.shutdown_probe_work();

    ledger.release();
    let outcome = authorizing.await.expect("the authorizer must not panic");

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Superseded),
        "a commit acknowledged after shutdown authorizes nothing",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "no candidate -- and no retained payload -- may survive the shutdown clear",
    );
    assert_eq!(router.probe_scheduler_snapshot().in_flight, 0);
}

// ---------------------------------------------------------------------------
// A wedged ledger delays nothing and times out terminally
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_forever_pending_ledger_delays_neither_publication_nor_shutdown() {
    // THE availability property. A ledger that never answers must not be able to
    // hold a reload or a shutdown open. Both are synchronous calls here, made
    // while the reservation is parked and never released -- if either could block
    // on the paid future, this test would hang rather than fail.
    let ledger = ParkingLedger::committing();
    let router = Arc::new(Fixture::default().router_with_ledger_arc(ledger.clone()));
    seed_candidate(&router, &key());

    let authorizing = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.authorize_paid_probe().await })
    };
    ledger.wait_until_parked().await;

    // Never released. Both operations must complete anyway, and they are measured
    // to keep a regression that made either wait from reading as a slow pass.
    //
    // The clock is PAUSED, which matters in two ways here. It lets the reservation
    // timeout fire without spending five real seconds of suite time; and because
    // tokio's pause does not affect `std::time::Instant`, the measurement below is
    // still real elapsed time, so a publication that genuinely blocked on the
    // parked future would show up as a real wait rather than as an advanced
    // virtual clock.
    let started = std::time::Instant::now();
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&router);
    next.publish_probe_incarnation();
    router.shutdown_probe_work();
    let elapsed = started.elapsed();

    assert!(
        elapsed < PAID_PROBE_RESERVATION_TIMEOUT,
        "publication and shutdown took {elapsed:?}, which is at or past the \
         reservation timeout -- they waited on the paid future",
    );
    assert!(
        !router.is_current_publication(),
        "both completed while the reservation was still pending",
    );

    // And the abandoned attempt itself ends terminally rather than hanging.
    //
    // Bounded by a wait of its OWN, generously past the production bound: without
    // it, deleting the production timeout makes this test HANG instead of failing,
    // and a stall is a worse regression mode than a red -- it reports as a suite
    // that never finishes rather than as a named assertion. Measured: with the
    // production bound removed, the unbounded form ran past two minutes.
    let settled = tokio::time::timeout(PAID_PROBE_RESERVATION_TIMEOUT * 4, authorizing)
        .await
        .expect(
            "the attempt neither completed nor expired -- the ledger await is \
             unbounded, so a wedged accounting layer strands a claimed candidate \
             and a held slot forever",
        )
        .expect("the authorizer must not panic");
    assert_eq!(
        settled.refusal(),
        Some(PaidProbeRefusal::ReservationTimeout),
        "a ledger that never answers must time out, not hang: {settled:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn a_reservation_timeout_is_terminal_and_releases_its_slot() {
    // The timeout itself, on a PAUSED clock so the bound is exercised
    // deterministically rather than by waiting out real seconds.
    //
    // Terminal UNKNOWN: the actor may still commit the unit after this returns, so
    // there is nothing to retry toward -- a requeue could pair a second unit with
    // the first, and no refund exists to undo either.
    let ledger = ParkingLedger::committing();
    let router = Fixture::default().router_with_ledger_arc(ledger.clone());
    seed_candidate(&router, &key());

    // Bounded by the test's own wait, well past the production bound, for the same
    // reason as in `a_forever_pending_ledger_delays_neither_publication_nor_shutdown`:
    // a missing production timeout must fail here rather than stall the suite.
    let outcome = tokio::time::timeout(
        PAID_PROBE_RESERVATION_TIMEOUT * 4,
        router.authorize_paid_probe(),
    )
    .await
    .expect("the attempt did not expire at its bound -- the ledger await is unbounded");

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::ReservationTimeout),
        "a reservation that never answers must expire at the bound",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "premise: the call WAS submitted -- which is why expiry is unknown rather \
         than a clean refusal",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a timeout must NOT requeue: the actor may yet commit, so a retry could \
         pair a second unit with the first",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "and the paid slot must be released",
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_authorize_future_requeues_nothing_and_releases_the_slot() {
    // Cancellation of the whole attempt, in the shape a timeout or a shutdown
    // produces it: the authorize future is DROPPED mid-await. Same posture as an
    // expiry -- the actor may have received the reservation, so nothing is
    // requeued and nothing may be dialed -- and the slot must come back.
    let ledger = ParkingLedger::committing();
    let router = Fixture::default().router_with_ledger_arc(ledger.clone());
    seed_candidate(&router, &key());

    {
        let mut attempt = Box::pin(router.authorize_paid_probe());
        // Polled far enough to be parked inside the reservation, then dropped.
        let polled = futures::poll!(&mut attempt);
        assert!(
            polled.is_pending(),
            "premise: the attempt must be mid-await"
        );
        assert_eq!(
            ledger.calls(),
            1,
            "premise: the reservation must have been submitted before the drop",
        );
    }

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a dropped attempt must not requeue a candidate whose unit may be committed",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "and its paid slot must be released on the drop path",
    );
}

// ---------------------------------------------------------------------------
// Requeue liveness against the shared generation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_requeue_from_a_superseded_router_is_refused() {
    // The requeue's SHARED-GENERATION read, which is a different question from its
    // incarnation comparison: a publication by a REPLACEMENT does not change this
    // router's own stamp, so a candidate carrying that stamp passes the incarnation
    // check. Without the shared read, an old router could resurrect a retained
    // payload onto a list the publication just cleared.
    let router = Arc::new(Fixture::default().router());
    let stamped = router.probe_incarnation();
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&router);
    next.publish_probe_incarnation();
    assert!(
        !router.is_current_publication(),
        "premise: this router must be superseded",
    );

    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        // MATCHES this router's own stamp, so only the shared read can refuse it.
        incarnation: stamped,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a superseded router must not resurrect a candidate, payload and all",
    );
}

#[tokio::test]
async fn a_requeue_after_shutdown_is_refused() {
    // The same read, against the terminal shutdown generation.
    let router = Fixture::default().router();
    let stamped = router.probe_incarnation();
    router.shutdown_probe_work();

    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        incarnation: stamped,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "nothing may be requeued after shutdown",
    );
}

#[tokio::test]
async fn a_requeue_at_a_retired_incarnation_is_refused_as_defense_in_depth() {
    // The OTHER liveness read: a candidate from a generation this router itself no
    // longer publishes under, while the router IS still current. Both reads are
    // needed, and this is the one the shared predicate cannot make.
    let router = Fixture::default().router();
    let live = router.probe_incarnation();
    assert!(
        router.is_current_publication(),
        "premise: this router must still be current, so the shared read passes",
    );

    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        incarnation: live - 1,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a candidate from a retired incarnation must not be resurrected onto the \
         live list, payload and all",
    );
    // POSITIVE control: the SAME call at the live incarnation is accepted, so the
    // refusal is about staleness rather than about every requeue being dropped.
    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        incarnation: live,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "a live candidate must still requeue",
    );
}

// ---------------------------------------------------------------------------
// The generation predicate and the publication callback
// ---------------------------------------------------------------------------

#[test]
fn the_shared_ticket_is_the_publication_authority_across_routers() {
    // The predicate's own contract, stated over the shape that makes it necessary:
    // two routers sharing one ticket. A per-router flag could not express this --
    // the publication touches only the replacement, so nothing writes to the
    // outgoing one.
    let previous = Fixture::default().router();
    assert!(
        previous.is_current_publication(),
        "a freshly built router holds the current publication",
    );

    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&previous);
    next.publish_probe_incarnation();

    assert!(
        next.is_current_publication(),
        "the replacement's own publication makes IT current",
    );
    assert!(
        !previous.is_current_publication(),
        "and supersedes the outgoing router, whose stamp nothing rewrote",
    );
    // MONOTONIC, so no ABA: nothing the outgoing router can do makes it current
    // again.
    previous.publish_probe_incarnation();
    assert!(
        previous.is_current_publication(),
        "an explicit republication by the old router draws a NEWER ticket value, \
         which is the only way back to current",
    );
    assert!(
        !next.is_current_publication(),
        "and that supersedes the replacement in turn",
    );
}

#[tokio::test]
async fn the_publication_callback_stores_before_returning() {
    // The ordering the callback shape exists to own. The store must have happened
    // by the time the call returns, and the router it hands over must be the one
    // that was just stamped.
    let router = Arc::new(Fixture::default().router());
    let stored: Arc<parking_lot::Mutex<Option<Arc<Router>>>> =
        Arc::new(parking_lot::Mutex::new(None));
    // Observed from INSIDE the callback: the incarnation must already be stamped
    // when the store runs, which is what "stamp first" means.
    let stamped_at_store = Arc::new(parking_lot::Mutex::new(None));

    let retired = {
        let stored = Arc::clone(&stored);
        let stamped_at_store = Arc::clone(&stamped_at_store);
        router.publish_probe_incarnation_into(move |published| {
            *stamped_at_store.lock() = Some(published.probe_incarnation());
            *stored.lock() = Some(published);
        })
    };

    let published = stored.lock().take().expect(
        "the callback must have run BEFORE the call returned -- a store left to the \
         caller is one a caller can reorder or forget",
    );
    assert!(
        Arc::ptr_eq(&published, &router),
        "the callback receives the router that was stamped, not a copy",
    );
    assert_eq!(
        *stamped_at_store.lock(),
        Some(router.probe_incarnation()),
        "and it was ALREADY stamped when the store ran: a store before the stamp \
         leaves a window in which the published router carries the outgoing \
         incarnation",
    );
    assert_eq!(retired, 0, "nothing was queued, so nothing was retired");
}

#[tokio::test]
async fn the_publication_callback_retires_the_outgoing_incarnations_work() {
    // The callback form must do everything the plain stamp does -- it is the
    // production path, so a version that stored without retiring would leave work
    // from the previous incarnation runnable.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = crate::router::probe_test_support::remote_router(provider);
    let identity = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&identity, ProbeValidator::CountTokens);
    assert_eq!(
        router.probe_scheduler_snapshot().queued,
        1,
        "premise: one job must be queued under the outgoing incarnation",
    );
    let router = Arc::new(router);

    let mut stored = false;
    let retired = router.publish_probe_incarnation_into(|_published| {
        stored = true;
    });

    assert!(stored, "the callback must have run");
    assert_eq!(
        retired, 1,
        "the outgoing incarnation's work must be retired"
    );
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
}

// ---------------------------------------------------------------------------
// The public surface this protocol restored
// ---------------------------------------------------------------------------

#[test]
fn the_legacy_publication_surface_is_callable_synchronously() {
    // A COMPILE-SURFACE assertion: these two are used here at their established
    // signatures -- `&self`, synchronous, returning `usize` -- so a change back to
    // an async or guard-returning form breaks this test at compile time rather
    // than silently at an out-of-crate call site.
    //
    // Behavioural too, so it is not merely a signature echo: each must still do
    // its job. Written against the trait-free plain call because that IS the
    // compatibility contract.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = crate::router::probe_test_support::remote_router(provider);
    let identity = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&identity, ProbeValidator::CountTokens);
    assert_eq!(router.probe_scheduler_snapshot().queued, 1, "premise");

    // Bound to a `usize` with no `.await`, which is the surface being pinned.
    let retired: usize = router.publish_probe_incarnation();
    assert_eq!(retired, 1, "the sync stamp must retire the outgoing work");

    router.activate_probe_lane(&identity, ProbeValidator::CountTokens);
    let cancelled: usize = router.shutdown_probe_work();
    assert_eq!(
        cancelled, 1,
        "the sync shutdown must cancel queued work and return the count",
    );
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
}

#[test]
fn no_publication_guard_type_remains_on_the_surface() {
    // SOURCE-TEXT guard over the ABSENCE of the withdrawn guard type. The blocking
    // design exported a token an out-of-crate caller held across its own store;
    // this protocol has no such token, and re-introducing one would be a public
    // surface change that this test names rather than leaving to a baseline diff.
    //
    // Scans the crate root's export list plus the router module's, which together
    // are the only routes to the public surface for this module tree.
    for (source, what) in [
        (include_str!("../lib.rs"), "the crate root"),
        (include_str!("mod.rs"), "the router module"),
    ] {
        assert!(
            !source.contains("PaidProbePublication"),
            "{what} still names a publication guard type; the generation protocol \\
             needs no token held across a caller's store",
        );
        assert!(
            !source.contains("paid_probe_fence"),
            "{what} still references the withdrawn fence module",
        );
    }
}
