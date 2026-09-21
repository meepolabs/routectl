// The lifecycle transition: publication versus shutdown, and requeue versus the
// clear. Both races are driven to a DETERMINISTIC outcome rather than sampled.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// Publication versus shutdown
// ---------------------------------------------------------------------------

#[test]
fn a_reload_parked_before_its_commit_point_never_stores_after_shutdown() {
    // THE publication-after-shutdown race. A reload's commit point is its call to
    // `publish_probe_incarnation_into`; a reload that reached it while a shutdown
    // was running would otherwise stamp, re-arm the scheduler the shutdown just
    // cleared, and -- worst -- STORE a replacement router into a daemon that is
    // going away, through a callback nothing downstream can undo.
    //
    // Deterministic by construction: the reload thread waits on a barrier until
    // the shutdown has fully completed, so "shutdown first" is arranged rather
    // than raced for.
    let router = Arc::new(Fixture::default().router());
    seed_candidate(&router, &key());
    let stamped_before = router.probe_incarnation();

    // The reload is parked here, before its commit point.
    let gate = Arc::new(std::sync::Barrier::new(2));
    let stored = Arc::new(AtomicUsize::new(0));
    let reload = {
        let router = Arc::clone(&router);
        let gate = Arc::clone(&gate);
        let stored = Arc::clone(&stored);
        std::thread::spawn(move || {
            // Released only after the shutdown below has returned.
            gate.wait();
            router.publish_probe_incarnation_into(|_published| {
                stored.fetch_add(1, Ordering::SeqCst);
            })
        })
    };

    // SHUTDOWN RUNS FIRST, to completion.
    router.shutdown_probe_work();
    let ticket_after_shutdown = router.probe_incarnation_ticket.load(Ordering::Acquire);

    // Now release the parked reload.
    gate.wait();
    let retired = reload.join().expect("the reload thread must not panic");

    assert_eq!(
        stored.load(Ordering::SeqCst),
        0,
        "the callback MUST NOT run after shutdown: a store here publishes a \
         router into a daemon that has already stopped, and nothing downstream \
         can undo it",
    );
    assert_eq!(retired, 0, "and the publication must report doing nothing");
    assert_eq!(
        router.probe_incarnation_ticket.load(Ordering::Acquire),
        ticket_after_shutdown,
        "the ticket must stay at its TERMINAL value -- a post-shutdown draw would \
         move the generation every liveness read depends on",
    );
    assert_eq!(
        router.probe_incarnation(),
        stamped_before,
        "and no router may be restamped after shutdown",
    );
    assert!(
        !router.is_current_publication(),
        "nothing is current after shutdown, so nothing may reserve",
    );
}

#[tokio::test]
async fn no_candidate_can_reserve_after_a_shutdown_beat_a_reload() {
    // The consequence the test above exists for, stated as behaviour: with the
    // lifecycle terminal, a claimed candidate cannot reach the accounting layer at
    // all -- no publication reopened the window.
    let ledger = CountingLedger::committed();
    let router = Arc::new(Fixture::default().router_with_ledger(ledger.clone()));
    let stamped = router.probe_incarnation();
    router.shutdown_probe_work();

    // A no-op publication after shutdown, the shape the parked reload performs.
    let mut stored = false;
    let retired = router.publish_probe_incarnation_into(|_published| {
        stored = true;
    });
    assert!(
        !stored,
        "premise: the post-shutdown publication stores nothing"
    );
    assert_eq!(retired, 0);

    // Seeded at the router's own unchanged stamp, so the incarnation comparison
    // cannot be what refuses -- this has to be the terminal generation.
    seed_candidate_at(&router, &key(), stamped);

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Superseded),
        "no candidate may reserve once the lifecycle is terminal",
    );
    assert_eq!(ledger.calls(), 0, "and the accounting layer is never asked");
}

#[test]
fn a_publication_that_wins_first_stores_and_a_later_shutdown_is_still_terminal() {
    // The other order, so the no-op above is about the TERMINAL state rather than
    // about the callback form never storing. Publication first: it stamps, retires,
    // and stores. Shutdown after: terminal, ticket advanced past the fresh stamp.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = Arc::new(crate::router::probe_test_support::remote_router(provider));
    let identity = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&identity, ProbeValidator::CountTokens);
    assert_eq!(router.probe_scheduler_snapshot().queued, 1, "premise");

    let stored = Arc::new(AtomicUsize::new(0));
    let retired = {
        let stored = Arc::clone(&stored);
        router.publish_probe_incarnation_into(move |published| {
            assert!(
                published.is_current_publication(),
                "the stored router must already be the current publication",
            );
            stored.fetch_add(1, Ordering::SeqCst);
        })
    };

    assert_eq!(
        stored.load(Ordering::SeqCst),
        1,
        "the callback ran exactly once"
    );
    assert_eq!(
        retired, 1,
        "and the outgoing incarnation's work was retired"
    );
    assert!(router.is_current_publication());

    // Shutdown afterwards is still terminal, and supersedes the fresh stamp.
    let stamped = router.probe_incarnation();
    router.shutdown_probe_work();

    assert!(
        !router.is_current_publication(),
        "shutdown supersedes even a router that published a moment earlier",
    );
    assert_eq!(
        router.probe_incarnation(),
        stamped,
        "without restamping it -- the terminal generation is unstamped",
    );
    // And a further publication is now a no-op, which is what makes the shutdown
    // terminal rather than merely latest.
    let mut stored_again = false;
    assert_eq!(
        router.publish_probe_incarnation_into(|_p| {
            stored_again = true;
        }),
        0,
    );
    assert!(!stored_again, "no publication may follow a shutdown");
}

#[test]
fn the_plain_publication_is_also_a_no_op_after_shutdown() {
    // The compatibility surface takes the same terminal check, so a caller on the
    // established synchronous method is not the one hole in the protocol.
    let router = Fixture::default().router();
    let stamped = router.probe_incarnation();
    router.shutdown_probe_work();
    let ticket = router.probe_incarnation_ticket.load(Ordering::Acquire);

    let retired = router.publish_probe_incarnation();

    assert_eq!(
        retired, 0,
        "a plain publication after shutdown does nothing"
    );
    assert_eq!(router.probe_incarnation(), stamped, "no restamp");
    assert_eq!(
        router.probe_incarnation_ticket.load(Ordering::Acquire),
        ticket,
        "and no ticket draw",
    );
}

// ---------------------------------------------------------------------------
// Requeue versus the clear
// ---------------------------------------------------------------------------

/// Drive one requeue with a PARK installed at its first in-lock point, prove by
/// DIRECT LOCK EVIDENCE that the queue lock is held there, then barrier-drive the
/// clear and let both finish.
///
/// The lock evidence replaces an earlier timing inference. While the requeue is
/// parked, `try_lock` on the candidate queue must FAIL -- that is a direct
/// observation that the lock is held at the park point, so the clear provably
/// cannot be running concurrently. With liveness checked before the lock instead,
/// the parked requeue holds nothing, `try_lock` SUCCEEDS, and the test fails on
/// that assertion with no dependence on how long anything takes.
///
/// Returns the candidate count after both have finished. Zero is the only safe
/// answer in either lock order.
fn requeue_racing_a_clear(router: &Arc<Router>, clear: impl FnOnce() + Send + 'static) -> usize {
    let live = router.probe_incarnation();
    let parked = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));

    let requeue = {
        let router = Arc::clone(router);
        let parked = Arc::clone(&parked);
        let resume = Arc::clone(&resume);
        std::thread::spawn(move || {
            let announced = std::sync::atomic::AtomicBool::new(false);
            crate::router::paid_probe_authorize::with_requeue_park(
                move || {
                    // Once only: the hook fires on every requeue this thread
                    // makes, and a second park would wait on a barrier nobody
                    // answers.
                    if !announced.swap(true, Ordering::SeqCst) {
                        parked.wait();
                        resume.wait();
                    }
                },
                || {
                    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
                        key: key(),
                        incarnation: live,
                        validator: ProbeValidator::PaidCompletion,
                        payload: payload(),
                    });
                },
            );
        })
    };

    // The requeue is now provably AT the park point.
    parked.wait();

    // THE LOCK EVIDENCE. Direct, and it is what makes this test independent of
    // timing: at its first in-lock point the requeue HOLDS the candidate lock, so
    // no clear can be interleaved with the rest of its body.
    assert!(
        router.paid_probe_candidates.try_lock().is_none(),
        "the parked requeue does not hold the candidate queue lock -- liveness is \
         being checked before the lock is taken, so a clear can run between that \
         check and the push",
    );

    // Only now start the clear. It advances the ticket (no queue lock needed) and
    // then blocks on the lock the parked requeue holds.
    let clearing = std::thread::spawn(clear);
    // Wait for the ticket to have moved, which is observable without the queue
    // lock. Past this point the clear is provably past its own first step and
    // waiting on the lock, so releasing the requeue linearizes the two.
    while router.is_current_publication() {
        std::thread::yield_now();
    }

    resume.wait();
    requeue.join().expect("the requeue thread must not panic");
    clearing.join().expect("the clearing thread must not panic");
    router.all_recorded_paid_candidates_for_tests().len()
}

#[test]
fn a_requeue_parked_on_the_queue_lock_cannot_outlive_a_shutdown_clear() {
    // THE requeue-after-clear race, and the reason the queue lock is taken BEFORE
    // the liveness reads rather than after.
    //
    // Checking liveness FIRST leaves a window: the check passes, a shutdown then
    // advances the ticket AND clears the list, and this call afterwards pushes a
    // candidate -- with its retained payload -- onto a list that was just emptied.
    // Nothing later removes it, so it survives into a daemon that has stopped.
    //
    // With the lock first the two orders linearize safely: either the requeue
    // completes and the following clear removes it, or the clear goes first and the
    // requeue's liveness read (taken under the lock, after the ticket moved)
    // observes the supersession and drops.
    let router = Arc::new(Fixture::default().router());

    let survived = {
        let router = Arc::clone(&router);
        requeue_racing_a_clear(&Arc::clone(&router), move || {
            router.shutdown_probe_work();
        })
    };

    assert_eq!(
        survived, 0,
        "no candidate -- and no retained payload -- may survive the shutdown clear",
    );
    assert!(
        !router.is_current_publication(),
        "premise: the shutdown really did run",
    );
}

#[test]
fn a_requeue_parked_on_the_queue_lock_cannot_outlive_a_publication_clear() {
    // The same race against a PUBLICATION rather than a shutdown. Publication also
    // advances the ticket before taking the candidate lock, so the same two
    // linearizations apply and both are safe.
    let previous = Arc::new(Fixture::default().router());
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&previous);
    let next = Arc::new(next);

    let survived = {
        let next = Arc::clone(&next);
        requeue_racing_a_clear(&previous, move || {
            next.publish_probe_incarnation();
        })
    };

    assert_eq!(
        survived, 0,
        "an outgoing router's requeue must not survive the replacement's clear",
    );
    assert!(
        next.is_current_publication(),
        "premise: the replacement really did publish",
    );
}

#[test]
fn a_requeue_on_a_live_router_still_succeeds_under_the_same_lock_order() {
    // POSITIVE CONTROL for both races above: with no publication or shutdown
    // competing, the reordered requeue still does its job. Without this, moving
    // the liveness check under the lock could have broken the happy path and the
    // race tests would still pass -- they only assert absence.
    let router = Fixture::default().router();
    let live = router.probe_incarnation();

    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        incarnation: live,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    let queued = router.all_recorded_paid_candidates_for_tests();
    assert_eq!(queued.len(), 1, "a live requeue must still land");
    assert_eq!(queued[0].key, key());
    assert_eq!(
        queued[0].payload,
        payload(),
        "with its payload intact, or the retry would ask a different question",
    );
}

// ---------------------------------------------------------------------------
// Liveness is structural: no token, no stamp
// ---------------------------------------------------------------------------

#[test]
fn a_split_check_and_reacquire_cannot_publish() {
    // THE split check-reacquire defect, closed BY CONSTRUCTION rather than by a
    // runtime assertion. `publish_within` accepts only a `LiveLifecycleTransition`,
    // and the sole producer of one is `begin_publication`, which performs the
    // terminal check inside its own acquisition. So the defective shape -- check
    // liveness, drop the lock, re-acquire, publish -- cannot be written: the
    // re-acquisition has to go through `begin_publication` again and takes a fresh
    // check with it.
    //
    // Pinned two ways, because "cannot be written" needs evidence a reader can
    // check. First, source text: the module exposes no accessor that hands out the
    // terminal bit without the token, so there is nothing to read-then-drop.
    let state_src = include_str!("probe_lifecycle_state.rs");
    assert!(
        !state_src.contains("pub(super) fn is_shut_down"),
        "the terminal bit must not be readable without holding the token -- an \
         accessor is exactly what lets a caller check, drop, and act stale",
    );
    assert!(
        state_src.contains("pub(super) fn begin_publication"),
        "the only route to a live token must be the checking one",
    );
    let publication_src = include_str!("probe_publication.rs");
    assert!(
        publication_src.contains("LiveLifecycleTransition"),
        "the stamping path must be typed on the live token",
    );

    // Second, behaviourally: `begin_publication` REFUSES once terminal, which is
    // what any re-acquisition would run into.
    let router = Fixture::default().router();
    router.shutdown_probe_work();
    assert!(
        router.probe_lifecycle_state.begin_publication().is_none(),
        "a re-acquisition after shutdown must be refused, so a split check cannot \
         smuggle a publication through",
    );
    // And the whole-call behaviour that follows from it.
    assert_eq!(router.publish_probe_incarnation(), 0);
}

// ---------------------------------------------------------------------------
// Shutdown raises the retirement floor atomically with the clear
// ---------------------------------------------------------------------------

#[test]
fn shutdown_refuses_activation_that_races_it_on_an_unchanged_generation() {
    // THE reactivation window. A shutdown restamps NO router, so admitted traffic
    // racing it still carries the OLD generation. Clearing alone leaves the
    // scheduler's retirement floor where it was, so such an activation would be
    // ADMITTED into a table that was just swept -- re-arming work nothing will run.
    //
    // Raising the floor to the terminal generation inside the same critical section
    // as the clear is what refuses it, and the positive control below is what makes
    // this test about the shutdown rather than about activation never working.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = crate::router::probe_test_support::remote_router(provider);
    let identity = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");

    // POSITIVE CONTROL, before the shutdown: this exact activation is admitted.
    assert_eq!(
        router.activate_probe_lane(&identity, ProbeValidator::CountTokens),
        crate::probe_scheduler::ProbeActivation::Queued,
        "premise: the same activation must be admitted BEFORE shutdown, or the \
         refusal below says nothing",
    );
    assert_eq!(router.probe_scheduler_snapshot().queued, 1);
    let stamped = router.probe_incarnation();

    router.shutdown_probe_work();

    assert_eq!(
        router.probe_incarnation(),
        stamped,
        "premise: shutdown does NOT restamp, so racing traffic still carries this \
         generation -- which is why the floor has to move instead",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().queued,
        0,
        "the sweep happened",
    );

    // THE ASSERTION: the same activation, at the same unchanged generation, is now
    // refused as retired rather than re-arming the swept table.
    assert_eq!(
        router.activate_probe_lane(&identity, ProbeValidator::CountTokens),
        crate::probe_scheduler::ProbeActivation::Retired,
        "an activation racing the shutdown must be refused: the retirement floor \
         moved to the terminal generation atomically with the clear",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().queued,
        0,
        "and nothing may be queued after the sweep",
    );
}

// ---------------------------------------------------------------------------
// The callback contract: re-entry is refused, not deadlocked
// ---------------------------------------------------------------------------

#[test]
fn a_callback_that_re_enters_publication_is_refused_rather_than_deadlocking() {
    // A store callback runs while the lifecycle transition is held, so a callback
    // that called back in would deadlock on a non-reentrant mutex -- turning a
    // contract violation into a hang, which is the worse failure. The thread-local
    // marker is checked BEFORE the mutex, so re-entry returns a no-op instead.
    //
    // Driven on a worker thread under a HARD TIMEOUT, so the deadlock this closes
    // fails the test by name rather than stalling the suite.
    let router = Arc::new(Fixture::default().router());
    let outer_calls = Arc::new(AtomicUsize::new(0));
    let nested_retired = Arc::new(AtomicUsize::new(usize::MAX));

    let (tx, rx) = std::sync::mpsc::channel();
    {
        let router = Arc::clone(&router);
        let outer_calls = Arc::clone(&outer_calls);
        let nested_retired = Arc::clone(&nested_retired);
        std::thread::spawn(move || {
            let retired = router.publish_probe_incarnation_into(|published| {
                outer_calls.fetch_add(1, Ordering::SeqCst);
                // THE RE-ENTRY: both entry points, from inside the callback.
                let mut inner_calls = 0_usize;
                let nested = published.publish_probe_incarnation_into(|_p| {
                    inner_calls += 1;
                });
                assert_eq!(
                    inner_calls, 0,
                    "a nested publication must not invoke its own callback",
                );
                assert_eq!(
                    published.shutdown_probe_work(),
                    0,
                    "a nested shutdown must be a no-op too",
                );
                nested_retired.store(nested, Ordering::SeqCst);
            });
            let _ = tx.send(retired);
        });
    }

    let retired = rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
        "the publication never returned -- a re-entrant callback deadlocked on \
             the lifecycle mutex instead of being refused",
    );

    assert_eq!(
        outer_calls.load(Ordering::SeqCst),
        1,
        "the OUTER callback runs exactly once",
    );
    assert_eq!(
        nested_retired.load(Ordering::SeqCst),
        0,
        "and the nested publication reports doing nothing",
    );
    assert_eq!(retired, 0, "nothing was queued, so nothing was retired");
    // The outer publication still took effect, so the refusal was scoped to the
    // nested call rather than poisoning the whole transition.
    assert!(
        router.is_current_publication(),
        "the outer publication must still have published",
    );
    // And the marker is cleared afterwards, so later publications on this thread
    // are not silently no-ops.
    assert_eq!(
        router.publish_probe_incarnation_into(|_p| {}),
        0,
        "a later publication on the same thread must run normally",
    );
    assert!(router.is_current_publication());
}

// ---------------------------------------------------------------------------
// Recording a candidate takes the same lock order and liveness
// ---------------------------------------------------------------------------

#[test]
fn a_recording_parked_on_the_queue_lock_cannot_outlive_a_publication_clear() {
    // `record_paid_probe_candidate` is reached by a WORKER settling a probe, which
    // can race a publication exactly as a requeue can. Same lock order, same
    // liveness reads, same two safe linearizations.
    //
    // Driven through the production recording path with the same park instrument,
    // and proven by the same direct lock evidence.
    let previous = Arc::new(Fixture::default().router());
    let mut next = Fixture::default().bare_router();
    next.carry_over_learned_from(&previous);
    let next = Arc::new(next);
    let live = previous.probe_incarnation();

    let parked = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let recording = {
        let previous = Arc::clone(&previous);
        let parked = Arc::clone(&parked);
        let resume = Arc::clone(&resume);
        std::thread::spawn(move || {
            let announced = std::sync::atomic::AtomicBool::new(false);
            crate::router::probe_lifecycle::with_record_park(
                move || {
                    if !announced.swap(true, Ordering::SeqCst) {
                        parked.wait();
                        resume.wait();
                    }
                },
                || {
                    previous.record_paid_probe_candidate_for_tests(&key(), live, payload());
                },
            );
        })
    };

    parked.wait();
    assert!(
        previous.paid_probe_candidates.try_lock().is_none(),
        "the parked recording must HOLD the candidate lock, or a clear can run \
         between its liveness read and its push",
    );

    let publishing = {
        let next = Arc::clone(&next);
        std::thread::spawn(move || next.publish_probe_incarnation())
    };
    while previous.is_current_publication() {
        std::thread::yield_now();
    }
    resume.wait();
    recording
        .join()
        .expect("the recording thread must not panic");
    publishing
        .join()
        .expect("the publishing thread must not panic");

    assert!(
        previous.all_recorded_paid_candidates_for_tests().is_empty(),
        "no candidate -- and no retained payload -- may survive the clear",
    );
}

#[test]
fn a_recording_after_shutdown_records_nothing() {
    // The worker-settlement path after shutdown: a settlement that commits against
    // the terminal generation must record nothing, so no payload survives into a
    // daemon that has stopped.
    let router = Fixture::default().router();
    let live = router.probe_incarnation();

    // POSITIVE control first: the same call BEFORE shutdown does record.
    router.record_paid_probe_candidate_for_tests(&key(), live, payload());
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "premise: this recording lands while the router is current",
    );
    router.paid_probe_candidates.lock().clear();

    router.shutdown_probe_work();
    router.record_paid_probe_candidate_for_tests(&key(), live, payload());

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a settlement after shutdown must record nothing, payload included",
    );
}
