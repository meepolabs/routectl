// Retirement across a live free operation: a retired IN-FLIGHT row keeps its
// concurrency slot until its lease ends, while idle rows go immediately.
//
// An `include!`d FRAGMENT of `concurrency_tests.rs`, not a module of its own:
// the host's imports stay in scope and every test here keeps its original fully
// qualified name. Carries no top-level `use` for that reason.

#[test]
fn retirement_keeps_an_in_flight_row_counted_until_its_lease_settles() {
    // THE reload undercount. A retirement that dropped the in-flight row would
    // make `in_flight()` stop counting an operation that is STILL RUNNING against
    // the upstream -- so the replacement router's next lease or paid acquisition
    // would admit work on top of it, and real simultaneous background load would
    // exceed PROBE_MAX_CONCURRENCY with no counter showing it. A reload is
    // exactly when this happens, because retirement and live probe work coincide.
    let scheduler = Arc::new(ProbeScheduler::new());
    let now = Instant::now();
    // Two lanes at generation 1: one will be LEASED (in flight), the other left
    // queued so the idle-row half of the contract is observable in the same test.
    for n in 0..2 {
        assert_eq!(
            scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload()),
            ProbeActivation::Queued
        );
    }
    let lease = scheduler.lease_due(now).expect("a queued job is due");
    assert_eq!(
        scheduler.snapshot().in_flight,
        1,
        "premise: one operation must be in flight before retirement"
    );
    assert_eq!(
        scheduler.snapshot().queued,
        1,
        "premise: one row still idle"
    );

    // Act: publish a new incarnation, retiring generation 1.
    let cancelled = scheduler.retire_before(2);

    // The IDLE row went; the IN-FLIGHT row stayed, still counted.
    assert_eq!(
        cancelled, 1,
        "only the idle row is cancelled -- the in-flight one holds a live \
         operation and cannot be cancelled by bookkeeping alone"
    );
    assert_eq!(
        scheduler.snapshot().queued,
        0,
        "the queued row from the retired incarnation is gone immediately"
    );
    assert_eq!(
        scheduler.snapshot().in_flight,
        1,
        "the retired in-flight row must STILL be counted: its upstream call is \
         running, and an uncounted live call lets the replacement router \
         over-subscribe the shared ceiling"
    );

    // And the retained row is NOT leasable -- it holds a slot, it does not get
    // more work. A new-incarnation job is what may still be leased.
    assert_eq!(
        scheduler.activate(&key(9), 2, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );
    let fresh = scheduler
        .lease_due(now)
        .expect("a live-incarnation job is leasable alongside the retained row");
    assert_ne!(
        *fresh.key(),
        key(0),
        "the retained retired row must never be handed to a worker again"
    );
    assert_eq!(
        scheduler.snapshot().in_flight,
        PROBE_MAX_CONCURRENCY,
        "the retained row plus the fresh lease fill the SHARED ceiling"
    );

    // THE ADMISSION CONSEQUENCE: with the ceiling full, paid work is refused --
    // which is the whole reason the retained row must keep counting.
    assert!(
        scheduler.try_acquire_paid_slot().is_none(),
        "a live old free operation must block paid admission across a reload"
    );
    drop(fresh);

    // Settling the retained lease is what finally sweeps it.
    let release = lease.settle(ProbeSettlement::Resolved, now);
    assert_eq!(
        release,
        ReleaseOutcome::Stale,
        "a settlement for a retired row must not apply: it may not reschedule, \
         tombstone into a retired incarnation, or report a spent free step"
    );
    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "and the slot comes back only now"
    );
    assert!(
        scheduler.try_acquire_paid_slot().is_some(),
        "with the old operation genuinely finished, paid work may proceed"
    );
}

#[test]
fn a_dropped_lease_on_a_retired_row_also_returns_its_slot() {
    // The cancellation path for the same retention: the lease is DROPPED rather
    // than settled (a timeout, a cancelled worker future, a panic-free early
    // return). The slot must come back on that path too, or a reload during a
    // probe timeout leaks a slot for the process's life.
    let scheduler = Arc::new(ProbeScheduler::new());
    let now = Instant::now();
    assert_eq!(
        scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );
    let lease = scheduler.lease_due(now).expect("the queued job is due");
    scheduler.retire_before(2);
    assert_eq!(
        scheduler.snapshot().in_flight,
        1,
        "premise: the retired in-flight row is still counted"
    );

    drop(lease);

    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "a dropped lease must release the retained row's slot"
    );
    assert_eq!(
        scheduler.snapshot().queued,
        0,
        "and must leave nothing rescheduled on retired state"
    );
}

#[test]
fn shutdown_clears_in_flight_rows_unlike_retirement() {
    // The deliberate asymmetry, stated as its own case so the retention above
    // cannot be mistaken for a rule about all cancellation. A retirement is
    // followed by a replacement router that keeps ADMITTING work, so an
    // uncounted live operation would let it over-subscribe. At shutdown nothing
    // will be admitted again, and the caller is on the way out and must not wait
    // on an upstream.
    let scheduler = Arc::new(ProbeScheduler::new());
    let now = Instant::now();
    assert_eq!(
        scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );
    let lease = scheduler.lease_due(now).expect("the queued job is due");
    assert_eq!(scheduler.snapshot().in_flight, 1, "premise");

    let cancelled = scheduler.cancel_all();

    assert_eq!(cancelled, 1, "shutdown cancels the in-flight row too");
    assert_eq!(
        scheduler.snapshot().in_flight,
        0,
        "and stops counting it: no later admission can be misled by the \
         under-count, because there is no later admission"
    );
    let release = lease.settle(ProbeSettlement::Resolved, now);
    assert_eq!(
        release,
        ReleaseOutcome::Stale,
        "the outstanding lease still settles as stale"
    );
}
