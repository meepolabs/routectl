// The paid-probe concurrency slot and the ceiling it SHARES with free leases.
//
// An `include!`d FRAGMENT of `concurrency_tests.rs`, not a module of its own:
// the host's imports stay in scope and every test here keeps its original
// fully qualified name. Carries no top-level `use` for that reason.

/// A scheduler behind the `Arc` a paid acquisition needs, with `queued` free
/// jobs already activated -- the shape a worker meets when a paid stage runs
/// alongside live free validation.
fn scheduler_with_queued(queued: usize) -> Arc<ProbeScheduler> {
    let scheduler = Arc::new(ProbeScheduler::new());
    for n in 0..queued {
        assert_eq!(
            scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload()),
            ProbeActivation::Queued,
            "premise: lane {n} must queue, or the ceiling under test never binds"
        );
    }
    scheduler
}

/// Record one held slot against `live` and `peak`, holding it long enough for a
/// sibling thread to be scheduled.
///
/// The yield is what makes the measurement possible: a bump-and-drop window
/// closes inside one timeslice, so every thread observes a live count of one
/// whether or not the ceiling is shared.
fn observe_held(live: &AtomicUsize, peak: &AtomicUsize) {
    let now_live = live.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(now_live, Ordering::SeqCst);
    thread::yield_now();
    live.fetch_sub(1, Ordering::SeqCst);
}

#[test]
fn a_paid_slot_counts_toward_the_same_ceiling_free_leases_refuse_at() {
    // THE shared-ceiling assertion. A separate paid counter would let real
    // simultaneous background work reach the sum of two ceilings while each
    // counter read inside its own bound, so the paid slot has to displace a
    // free lease.
    let scheduler = scheduler_with_queued(PROBE_QUEUE_DEPTH);
    let now = Instant::now();

    // Arrange: hold every slot but one with FREE leases.
    let free: Vec<_> = (0..PROBE_MAX_CONCURRENCY - 1)
        .map(|n| {
            scheduler
                .lease_due(now)
                .unwrap_or_else(|| panic!("free lease {n} must be available"))
        })
        .collect();

    // Act: the paid slot takes the last one.
    let paid = scheduler
        .try_acquire_paid_slot()
        .expect("one slot remains, so the paid stage may take it");

    // Assert: the ceiling is now full FROM THE FREE SIDE TOO.
    assert_eq!(
        scheduler.snapshot().in_flight,
        PROBE_MAX_CONCURRENCY,
        "the held paid slot must be counted in the one in_flight reading"
    );
    assert!(
        scheduler.lease_due(now).is_none(),
        "a held paid slot must refuse the next FREE lease -- if it does not, \
         the two kinds are counted separately and the real ceiling is their sum"
    );
    assert!(
        scheduler.try_acquire_paid_slot().is_none(),
        "and it must refuse a second paid acquisition"
    );
    assert_eq!(
        scheduler.snapshot().paid_slot_refusals_total,
        1,
        "the refused acquisition must be counted"
    );

    // And the converse: free leases fill the ceiling against the PAID side.
    drop(paid);
    drop(free);
    let all_free: Vec<_> = (0..PROBE_MAX_CONCURRENCY)
        .map(|n| {
            scheduler
                .lease_due(now)
                .unwrap_or_else(|| panic!("free lease {n} must be available"))
        })
        .collect();
    assert!(
        scheduler.try_acquire_paid_slot().is_none(),
        "free leases at the ceiling must refuse a paid acquisition"
    );
    drop(all_free);
}

#[test]
fn dropping_a_paid_slot_returns_capacity_to_the_free_side() {
    // The release path, read from the side that must observe it: a slot that
    // released only its own counter would keep refusing free leases forever.
    let scheduler = scheduler_with_queued(PROBE_QUEUE_DEPTH);
    let now = Instant::now();
    let held: Vec<_> = (0..PROBE_MAX_CONCURRENCY)
        .map(|_| {
            scheduler
                .try_acquire_paid_slot()
                .expect("paid slots up to the ceiling")
        })
        .collect();
    assert!(
        scheduler.lease_due(now).is_none(),
        "premise: paid slots alone must saturate the ceiling"
    );

    drop(held);

    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "every dropped paid slot must be released"
    );
    let leases: Vec<_> = (0..PROBE_MAX_CONCURRENCY)
        .map(|n| {
            scheduler
                .lease_due(now)
                .unwrap_or_else(|| panic!("free lease {n} must be available again"))
        })
        .collect();
    assert_eq!(leases.len(), PROBE_MAX_CONCURRENCY);
}

#[tokio::test]
async fn a_paid_slot_held_by_a_cancelled_future_still_releases() {
    // CANCELLATION, in the exact shape production produces it: the paid stage
    // runs inside a `tokio::time::timeout`, and expiry DROPS the wrapped future
    // along with everything it owns. A release written as an explicit call on
    // the success path leaks here, and nothing else in the suite would notice.
    //
    // Not `catch_unwind`: the release profile this repo gates on sets
    // `panic = "abort"`, so an unwinding variant of this test would be inert in
    // exactly the build that ships.
    let scheduler = scheduler_with_queued(1);

    let expired = tokio::time::timeout(Duration::from_millis(1), async {
        let _slot = scheduler
            .try_acquire_paid_slot()
            .expect("a slot is available");
        assert_eq!(
            scheduler.snapshot().in_flight,
            1,
            "premise: the slot must be held inside the cancelled future"
        );
        // Never completes, so the timeout is what ends this future -- which is
        // what makes the drop a cancellation rather than a normal exit.
        std::future::pending::<()>().await;
    })
    .await;

    assert!(
        expired.is_err(),
        "premise: the future must have been cancelled"
    );
    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "a cancelled future must still release the slot it held"
    );
}

#[test]
fn concurrent_paid_acquisition_never_exceeds_the_shared_ceiling() {
    // The ceiling under real parallelism, with both kinds contending. A
    // check-then-take across two locks would let two threads each read room and
    // both take the last slot.
    //
    // The oracle is an EXTERNAL live counter, not the scheduler's own snapshot:
    // a snapshot that omitted paid slots would under-report exactly the
    // over-subscription this test exists to catch, so reading it here would
    // make the assertion true for the reason it is meant to refute.
    //
    // The held window is a real yield rather than three back-to-back atomic
    // ops, because a window that closes before any sibling can run observes no
    // overlap at all -- measured: with a bump-and-drop window the mutated build
    // peaks at 1, and with the yield it peaks at 3 (past the bound of 2).
    let scheduler = scheduler_with_queued(PROBE_QUEUE_DEPTH);
    let now = Instant::now();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..64)
        .map(|n| {
            let scheduler = Arc::clone(&scheduler);
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            thread::spawn(move || {
                for _ in 0..32 {
                    // Alternating kinds, so the contention is between them and
                    // not merely within one.
                    if n % 2 == 0 {
                        if let Some(slot) = scheduler.try_acquire_paid_slot() {
                            observe_held(&live, &peak);
                            drop(slot);
                        }
                    } else if let Some(lease) = scheduler.lease_due(now) {
                        observe_held(&live, &peak);
                        let _committed = lease.settle(ProbeSettlement::Deferred, now);
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("no contending thread may panic");
    }

    assert!(
        peak.load(Ordering::SeqCst) <= PROBE_MAX_CONCURRENCY,
        "peak background concurrency {} exceeded the shared bound \
         {PROBE_MAX_CONCURRENCY}",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "every slot and lease must have been released"
    );
}

#[test]
fn the_release_paths_stay_module_private() {
    // SOURCE-TEXT guard over the VISIBILITY of the two release paths. A sibling
    // module that could call either would free a slot (or settle a lease) it never
    // took, which reads as capacity while the real operation is still running.
    //
    // NON-VACUOUS, measured both ways rather than assumed: widening
    // `release_paid_slot` back to `pub(super)` makes a planted sibling-module call
    // COMPILE (0 errors), while module-private makes the same call `E0624`. So the
    // keyword is load-bearing and this guard is asserting a real property.
    //
    // A guard rather than a compile-fail test because the workspace carries no
    // compile-fail harness, and adding one for two lines would be the larger
    // change. The scanned region is each function's own signature, located by
    // content.
    let source = include_str!("mod.rs");

    for name in ["fn release_paid_slot(&self)", "fn release("] {
        let at = source
            .find(name)
            .unwrap_or_else(|| panic!("{name} must exist in the scheduler module"));
        // The signature's own line, taken back to its start so any visibility
        // keyword in front of `fn` is included.
        let line_start = source[..at].rfind('\n').map_or(0, |n| n + 1);
        let signature = &source[line_start..at + name.len()];
        assert!(
            !signature.contains("pub"),
            "{name} must stay module-private -- a wider visibility lets a sibling \
             forge a release for work it does not hold: {signature}",
        );
    }
}
