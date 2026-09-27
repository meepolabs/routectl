// The hostile-concurrency sweep against ConfirmationTracker::close_and_wait:
// hundreds of contenders racing one close, repeated across rounds. `include!`d
// into confirmation_advance_tests.rs; the fixtures and imports live there, so
// do not add `use` lines here.

// ---------------------------------------------------------------------------
// Hostile concurrency: 512 contenders against one close_and_wait
// ---------------------------------------------------------------------------
//
// # What this covers that the sequential tests cannot
//
// The tracker's whole reason for holding `{closed, in_flight}` under ONE mutex is a
// race the sequential tests cannot express: a claimant that checked `closed` and then
// incremented `in_flight` as two steps could increment AFTER `close_and_wait` had
// already observed zero -- and shutdown would drain the writer out from under work
// that still needed it. Two atomics would pass every test above.
//
// # Why 512 and why a barrier
//
// Spawning contenders does not make them contend: the first can finish before the last
// is scheduled. Every contender waits on one barrier and is released together, with the
// close racing them from its own task. 512 is well above any core count -- the
// workspace's own gate rules record a counter race that read 20/20 green at 128 threads
// and 7/20 red at 512 -- and the whole race is repeated, because one pass of a
// concurrency test is one sample of a distribution.

/// How many contenders race one close.
const HOSTILE_CLAIMERS: usize = 512;

/// How many times the race is repeated.
const HOSTILE_ROUNDS: usize = 6;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hostile_claimants_never_outlive_the_close_that_observed_them() {
    // THE INVARIANT SET, all four asserted per round, and each with an observable that
    // its own mutation moves -- which took two attempts to get right. The first version
    // asserted only end states, and two mutations survived it:
    //
    //   - splitting the claim's check and increment into two critical sections stayed
    //     GREEN, because a claim admitted after the close observed zero still releases
    //     before the end, so every end-state assertion held;
    //   - making `close_and_wait` return without waiting stayed GREEN, because the
    //     contenders finished so fast that `in_flight` was legitimately zero by the
    //     time it returned.
    //
    // Both are fixed by making the test control the TIMING rather than sample it: a
    // gate the contenders hold their claims behind (so a non-waiting close provably
    // returns with work outstanding), and a flag the closer sets on return (so a claim
    // admitted after it is observable as itself rather than inferred).
    //
    //   1. NO CLAIM IS ADMITTED AFTER THE CLOSE RETURNS. Each contender records the
    //      closer's returned-flag at the instant its claim succeeded; any `true` is the
    //      split-critical-section race, directly observed.
    //   2. EVERY PRE-CLOSE CLAIM IS COUNTED UNTIL RESOLVED -- `in_flight` back to zero
    //      once everything has joined, so none was dropped unaccounted or counted
    //      twice.
    //   3. `in_flight` NEVER EXCEEDS THE CLAIMS TAKEN, sampled throughout by a watcher.
    //      That is the underflow's observable mirror: a saturating decrement hides an
    //      underflow at zero, but a release without a matching claim lets a later claim
    //      read low and the peak drift above the admitted count.
    //   4. CLOSE CANNOT RETURN WHILE ADMITTED WORK REMAINS. The contenders hold their
    //      slots behind a gate this test opens only after the closer has had ample
    //      opportunity to return early, so a non-waiting close is caught by the count
    //      it reports at its own return.
    //
    // Mutation checks, each turning THIS test red (all three verified):
    //   - split the claim's check and increment into two critical sections;
    //   - drop the accounting guard's release;
    //   - make `close_and_wait` return without waiting.
    for round in 1..=HOSTILE_ROUNDS {
        let tracker = Arc::new(ConfirmationTracker::new());

        // Every contender and the closer gather here, so the close RACES the claims
        // rather than following them. `+ 1` for the closing task.
        let gate = Arc::new(tokio::sync::Barrier::new(HOSTILE_CLAIMERS + 1));
        // Opened by this test only after the closer has had its chance to return
        // early. Until then every admitted claim is HELD, which is what makes
        // assertion 4 deterministic.
        let hold = Arc::new(tokio::sync::Notify::new());
        // Set by the closer the moment `close_and_wait` returns. A claim admitted while
        // this reads true is assertion 1's violation, observed rather than inferred.
        let closed_returned = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The WATCHER: samples the count while the race runs, for assertion 3.
        let watch_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watch_max = Arc::new(AtomicUsize::new(0));
        let watcher = {
            let tracker = Arc::clone(&tracker);
            let stop = Arc::clone(&watch_stop);
            let max = Arc::clone(&watch_max);
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    max.fetch_max(tracker.in_flight(), Ordering::AcqRel);
                    tokio::task::yield_now().await;
                }
            })
        };

        let contenders: Vec<_> = (0..HOSTILE_CLAIMERS)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let gate = Arc::clone(&gate);
                let hold = Arc::clone(&hold);
                let closed_returned = Arc::clone(&closed_returned);
                tokio::spawn(async move {
                    gate.wait().await;
                    match tracker.claim_reporting_closed() {
                        Some((claim, was_closed)) => {
                            // The flag is read INSIDE the critical section the slot was
                            // taken in, which is what makes it exact. Two readings that
                            // do not work, both tried: `tracker.is_closed()` after
                            // `claim` returns is racy by construction -- a close landing
                            // in that gap wrongly accuses a legitimate pre-close claim,
                            // and it produced real failures; and "did close_and_wait
                            // already return" is too late a line -- the split-section
                            // race admits its slot while the close is still WAITING, so
                            // that reading stayed green under the mutation.
                            let after_close = was_closed || closed_returned.load(Ordering::Acquire);
                            // HELD until the test opens the gate, so the slot is
                            // genuinely outstanding while the closer decides whether
                            // to wait.
                            hold.notified().await;
                            drop(claim);
                            (true, after_close)
                        }
                        None => (false, false),
                    }
                })
            })
            .collect();

        // The CLOSE, racing them from its own task.
        let closer = {
            let tracker = Arc::clone(&tracker);
            let gate = Arc::clone(&gate);
            let closed_returned = Arc::clone(&closed_returned);
            tokio::spawn(async move {
                gate.wait().await;
                tracker.close_and_wait(WAIT).await;
                // Sampled BEFORE the flag is published, so assertion 4 reads the count
                // as it stood at the return rather than after any later release.
                let at_return = tracker.in_flight();
                closed_returned.store(true, Ordering::Release);
                at_return
            })
        };

        // Give the closer ample opportunity to return EARLY while every admitted slot
        // is still held. A correct close is still waiting after this; a non-waiting one
        // has already returned and recorded a nonzero count.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Open the gate: every held claim releases and a correct close can finish.
        hold.notify_waiters();
        // `notify_waiters` only wakes CURRENT waiters, so a contender that had not
        // reached its await yet would hang. Keep notifying until every one is through.
        let drain = {
            let hold = Arc::clone(&hold);
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move {
                while tracker.in_flight() > 0 {
                    hold.notify_waiters();
                    tokio::task::yield_now().await;
                }
            })
        };

        let mut admitted = 0usize;
        let mut refused = 0usize;
        let mut admitted_after_close = 0usize;
        for contender in contenders {
            let (ok, after_close) = contender.await.expect("no contender may panic");
            if ok {
                admitted += 1;
                if after_close {
                    admitted_after_close += 1;
                }
            } else {
                refused += 1;
            }
        }
        let in_flight_at_close = closer.await.expect("the closer must not panic");
        watch_stop.store(true, Ordering::Release);
        watcher.await.expect("the watcher must not panic");
        drain.await.expect("the drain helper must not panic");

        // (1) No claim admitted after the close RETURNED.
        assert_eq!(
            admitted_after_close, 0,
            "round {round}: {admitted_after_close} claim(s) were admitted after \
             close_and_wait had already returned -- shutdown goes on to drain the \
             writer, so such a claim's advancement can never commit. This is what a \
             claim whose check and increment are separate critical sections produces",
        );
        assert!(
            tracker.is_closed(),
            "round {round}: the close must have taken effect",
        );
        assert!(
            tracker.claim().is_none(),
            "round {round}: a closed tracker admits no further claim",
        );

        // (2) Every claim accounted for, exactly once.
        assert_eq!(
            admitted + refused,
            HOSTILE_CLAIMERS,
            "round {round}: every contender must be accounted for",
        );
        assert_eq!(
            tracker.in_flight(),
            0,
            "round {round}: every pre-close claim released exactly once -- a leaked \
             accounting guard would leave this above zero, and shutdown would then \
             wait out its deadline for work that had already stopped",
        );

        // (3) The count never exceeded the claims actually taken.
        let peak = watch_max.load(Ordering::Acquire);
        assert!(
            peak <= admitted,
            "round {round}: in_flight peaked at {peak} with only {admitted} claims \
             admitted -- a count above the claims taken means an increment without a \
             claim, whose mirror is the underflow a saturating decrement hides",
        );

        // (4) is NOT asserted here, deliberately, and the reason is worth recording
        // rather than leaving as an omission: the closer can legitimately win the
        // barrier before any contender claims, so `in_flight` at its return is
        // honestly zero and the observation carries no information -- measured, under
        // the non-waiting mutation, at zero in all six rounds. It needs a claim that is
        // provably outstanding when the close begins, which is a DETERMINISTIC fixture
        // rather than a race: see
        // `close_cannot_return_while_an_admitted_slot_is_still_held` below.
        let _ = in_flight_at_close;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_cannot_return_while_an_admitted_slot_is_still_held() {
    // ASSERTION 4, deterministically. One claim is admitted BEFORE the close starts and
    // held across it, so "admitted work remains" is a state this test establishes
    // rather than races for -- which is what the hostile sweep above could not do.
    //
    // Mutation check: make `close_and_wait` return without waiting -> red here, where
    // it stayed green on the sweep.
    let tracker = Arc::new(ConfirmationTracker::new());
    let claim = tracker.claim().expect("an open tracker admits a claim");
    assert_eq!(tracker.in_flight(), 1, "premise: one slot is outstanding");

    // The close runs on its own task and reports whether it returned. Held in an
    // `Option` so the test can check "still running" without consuming it.
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closer = {
        let tracker = Arc::clone(&tracker);
        let closed = Arc::clone(&closed);
        tokio::spawn(async move {
            tracker.close_and_wait(WAIT).await;
            closed.store(true, Ordering::Release);
        })
    };

    // Ample opportunity to return early, while the slot is still held.
    //
    // A MULTI-THREAD runtime plus a real sleep, and both halves are load-bearing --
    // this took two wrong attempts. On a single-threaded runtime the spawned closer
    // never ran at all during the wait (measured: `closed=false` even under the
    // non-waiting mutation), so the test passed for the wrong reason and the mutation
    // survived. Yielding did not fix that: a spawned task on the current-thread
    // scheduler is not guaranteed to be polled by `yield_now` from the task that
    // spawned it. With two workers the closer runs concurrently, and the sleep gives a
    // mutated closer -- which awaits nothing -- ample wall-clock time to finish while a
    // correct one stays parked on the notify.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        !closed.load(Ordering::Acquire),
        "close_and_wait must NOT have returned: one advancement is still counted, and \
         it is waiting on a commit -- returning here lets shutdown drain the writer \
         out from under it",
    );
    assert_eq!(
        tracker.in_flight(),
        1,
        "premise: the slot really was still outstanding for that whole window",
    );

    // Release it: the close must then finish.
    drop(claim);
    tokio::time::timeout(WAIT, closer)
        .await
        .expect("close_and_wait must return once the last slot releases")
        .expect("the closer must not panic");
    assert!(closed.load(Ordering::Acquire));
    assert_eq!(tracker.in_flight(), 0);
}

/// What one contender saw: refused, or admitted and then released, and whether its
/// slot was still held at an instant the tracker itself reported closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContenderOutcome {
    Refused,
    Admitted { held_while_closed: bool },
}

/// The closer's half of the coordination: do not close until at least one contender
/// holds a slot. `notify_one` stores a permit, so an admission that lands before this
/// starts waiting is not lost.
async fn await_first_admission(first_admitted: &tokio::sync::Notify) {
    tokio::time::timeout(WAIT, first_admitted.notified())
        .await
        .expect("at least one contender must be admitted before the close");
}

/// The claimant's half of the coordination: keep the slot until the tracker is
/// ACTUALLY closed, read from the tracker rather than from a message the closer sent
/// before closing. Polled, because the tracker publishes no close event.
async fn hold_until_closed(tracker: &ConfirmationTracker) {
    tokio::time::timeout(WAIT, async {
        while !tracker.is_closed() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the close must take effect while an admitted claim is held");
}

/// One contender: claim, and if admitted, announce it and hold the slot across the
/// close. `held_while_closed` is read with the claim still alive, so `true` is direct
/// evidence that an admitted slot overlapped the closed state.
async fn contend(
    tracker: &ConfirmationTracker,
    first_admitted: &tokio::sync::Notify,
) -> ContenderOutcome {
    let Some(claim) = tracker.claim() else {
        return ContenderOutcome::Refused;
    };
    first_admitted.notify_one();
    hold_until_closed(tracker).await;
    let held_while_closed = tracker.is_closed();
    drop(claim);
    ContenderOutcome::Admitted { held_while_closed }
}

/// The closer: wait for an admission, then close. Returns the in-flight count at the
/// instant `close_and_wait` returned.
async fn close_after_first_admission(
    tracker: &ConfirmationTracker,
    first_admitted: &tokio::sync::Notify,
) -> usize {
    await_first_admission(first_admitted).await;
    tracker.close_and_wait(WAIT).await;
    tracker.in_flight()
}

#[tokio::test(flavor = "current_thread")]
async fn an_admitted_claim_is_held_across_the_close_it_races() {
    // The single-source pin for the coordination the hostile sweep below relies on.
    // On a current-thread runtime spawned tasks run in spawn order and only at await
    // points, so each half of the coordination is the ONLY thing standing between this
    // test and a deterministic failure:
    //
    //   - without `await_first_admission`, the closer (spawned first) closes before the
    //     contender is polled, the contender is refused, and nothing is admitted;
    //   - without `hold_until_closed`, the contender claims and releases without
    //     yielding, so the close begins with nothing outstanding.
    //
    // Either way the assertions below fail rather than hang: every wait is bounded.
    let tracker = Arc::new(ConfirmationTracker::new());
    let first_admitted = Arc::new(tokio::sync::Notify::new());

    let closer = {
        let tracker = Arc::clone(&tracker);
        let first_admitted = Arc::clone(&first_admitted);
        tokio::spawn(async move { close_after_first_admission(&tracker, &first_admitted).await })
    };
    let contender = {
        let tracker = Arc::clone(&tracker);
        let first_admitted = Arc::clone(&first_admitted);
        tokio::spawn(async move { contend(&tracker, &first_admitted).await })
    };

    let outcome = tokio::time::timeout(WAIT, contender)
        .await
        .expect("the contender must finish")
        .expect("the contender must not panic");
    let in_flight_at_return = tokio::time::timeout(WAIT, closer)
        .await
        .expect("the close must finish")
        .expect("the closer must not panic");

    assert_eq!(
        outcome,
        ContenderOutcome::Admitted {
            held_while_closed: true
        },
        "the contender must be admitted and still hold its slot when the tracker reports \
         closed",
    );
    assert_eq!(
        in_flight_at_return, 0,
        "close_and_wait returned while the claim was still held"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_close_racing_claims_admits_each_claimant_exactly_once_or_not_at_all() {
    // The DISCRIMINATING half of the invariant above, and the reason the sweep is not
    // enough on its own: every assertion there holds vacuously if NO claim is ever
    // admitted (a tracker that refused everything passes all four). This asserts, in
    // EVERY round, that at least one admitted claim was still held while the tracker
    // reported closed, and that each contender still resolved exactly once, so the
    // sweep is measuring a contended tracker rather than a closed one.
    //
    // Admission is COORDINATED, not sampled. Left to the barrier alone, the closer can
    // be released and close before any contender is polled, in every round, leaving
    // the oracle nothing to measure. So the closer waits for the first admission
    // before closing, and every admitted claimant holds its slot until the tracker
    // itself reads closed -- not until the closer merely announces it is about to
    // close. The remaining contenders still land on whichever side of the close the
    // scheduler puts them. `an_admitted_claim_is_held_across_the_close_it_races` pins
    // each half of that coordination deterministically.
    for round in 1..=HOSTILE_ROUNDS {
        let tracker = Arc::new(ConfirmationTracker::new());
        let gate = Arc::new(tokio::sync::Barrier::new(HOSTILE_CLAIMERS + 1));
        let first_admitted = Arc::new(tokio::sync::Notify::new());

        let contenders: Vec<_> = (0..HOSTILE_CLAIMERS)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let gate = Arc::clone(&gate);
                let first_admitted = Arc::clone(&first_admitted);
                tokio::spawn(async move {
                    gate.wait().await;
                    contend(&tracker, &first_admitted).await
                })
            })
            .collect();

        let closer = {
            let tracker = Arc::clone(&tracker);
            let gate = Arc::clone(&gate);
            let first_admitted = Arc::clone(&first_admitted);
            tokio::spawn(async move {
                gate.wait().await;
                close_after_first_admission(&tracker, &first_admitted).await
            })
        };

        let mut refused = 0usize;
        let mut admitted = 0usize;
        let mut held_while_closed = 0usize;
        for contender in contenders {
            let outcome = tokio::time::timeout(WAIT, contender)
                .await
                .expect("every contender must finish")
                .expect("no contender may panic");
            match outcome {
                ContenderOutcome::Refused => refused += 1,
                ContenderOutcome::Admitted {
                    held_while_closed: held,
                } => {
                    admitted += 1;
                    held_while_closed += usize::from(held);
                }
            }
        }
        let in_flight_at_return = tokio::time::timeout(WAIT, closer)
            .await
            .expect("the close must finish once every held claim releases")
            .expect("the closer must not panic");

        assert!(
            held_while_closed >= 1,
            "round {round}: no admitted claim was still held while the tracker reported \
             closed -- with none, every invariant here holds vacuously on a tracker that \
             refused everything",
        );
        assert_eq!(
            held_while_closed, admitted,
            "round {round}: every admitted claimant holds its slot until the tracker \
             reads closed, so each must have observed the close while holding",
        );
        assert_eq!(
            admitted + refused,
            HOSTILE_CLAIMERS,
            "round {round}: every contender is either admitted or refused, exactly once",
        );
        assert_eq!(
            in_flight_at_return, 0,
            "round {round}: close_and_wait returned while admitted claims were still held",
        );
        assert_eq!(
            tracker.in_flight(),
            0,
            "round {round}: every admitted claim released exactly once",
        );
        assert!(
            tracker.claim().is_none(),
            "round {round}: a closed tracker admits no further claim",
        );
    }
}
