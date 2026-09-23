// Aggregate status fields: last settlement and next retry.
// An `include!`d FRAGMENT of `queue_tests.rs`.

// ---------------------------------------------------------------------------
// The aggregate status fields: last settlement and next retry
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_scheduler_reports_no_last_settlement_and_no_next_retry() {
    // The base case, and it is the one an operator most needs to be
    // unambiguous: a scheduler nothing has settled on is what a
    // never-activated lane looks like, which must not read as a settled
    // outcome or as a zero wait.
    let scheduler = ProbeScheduler::new();

    let snap = scheduler.snapshot(Instant::now());

    assert_eq!(snap.last_settlement, None);
    assert_eq!(snap.next_retry_in, None);
}

#[test]
fn the_snapshot_reports_the_most_recent_settled_outcome() {
    // Arrange: two settlements in sequence, of DIFFERENT kinds, so "most
    // recent" is distinguishable from "the first" or "any".
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("job is due");
    let _committed = lease.settle(ProbeSettlement::Retryable, now);
    assert_eq!(
        scheduler.snapshot(now).last_settlement,
        Some(ProbeSettlement::Retryable),
        "premise: the first settlement is reported, so the second is a CHANGE"
    );

    // Act: a second, different outcome on a second identity.
    scheduler.activate(&key(1), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("second job is due");
    let _committed = lease.settle(ProbeSettlement::Resolved, now);

    // Assert
    assert_eq!(
        scheduler.snapshot(now).last_settlement,
        Some(ProbeSettlement::Resolved),
        "the field reports the MOST RECENT settlement, not the first"
    );
}

#[test]
fn a_dropped_lease_does_not_overwrite_the_last_settled_outcome() {
    // A dropped lease settled NOTHING, so recording it would erase the last
    // real answer -- which is the signal an operator reads. The distinction
    // matters because a dropped future is the ordinary shutdown path.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("job is due");
    let _committed = lease.settle(ProbeSettlement::Resolved, now);

    // Act: a second lease that is DROPPED rather than settled.
    scheduler.activate(&key(1), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("second job is due");
    drop(lease);

    // Assert
    assert_eq!(
        scheduler.snapshot(now).last_settlement,
        Some(ProbeSettlement::Resolved),
        "a dropped lease reported no outcome, so the last real one stands"
    );
}

#[test]
fn the_snapshot_reports_the_earliest_backoff_as_a_bounded_wait() {
    // Arrange: one backing-off job, whose deadline is a known backoff away.
    let scheduler = ProbeScheduler::new();
    let start = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(start).expect("job is due");
    let _committed = lease.settle(ProbeSettlement::Retryable, start);

    // Act / Assert: measured against the SAME instant the backoff was stamped
    // from, so the expected value is exact rather than clock-dependent.
    let wait = scheduler
        .snapshot(start)
        .next_retry_in
        .expect("a backing-off job has a wait");
    assert_eq!(
        wait,
        backoff_for_attempt(1),
        "the reported wait is the job's own backoff deadline, not an invented interval"
    );
    assert!(
        wait <= PROBE_BACKOFF_CEILING,
        "and it is bounded by the same ceiling the backoff itself is"
    );
}

#[test]
fn an_already_elapsed_backoff_reports_a_zero_wait_rather_than_none() {
    // Zero and absent mean different things: zero is leasable on the next tick,
    // absent is nothing to wait for. Collapsing them would tell an operator a
    // due job is not coming.
    let scheduler = ProbeScheduler::new();
    let start = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(start).expect("job is due");
    let _committed = lease.settle(ProbeSettlement::Retryable, start);

    // Act: read WELL past the deadline.
    let wait = scheduler
        .snapshot(start + PROBE_BACKOFF_CEILING + Duration::from_mins(1))
        .next_retry_in;

    // Assert
    assert_eq!(
        wait,
        Some(Duration::ZERO),
        "an elapsed backoff reports zero -- the job is leasable now -- never None"
    );
}

#[test]
fn the_snapshot_reports_the_earliest_of_several_backoffs() {
    // The EARLIEST, because that is when the scheduler next has work it can
    // take. Reporting any other job's deadline would tell an operator to wait
    // longer than the scheduler will.
    let scheduler = ProbeScheduler::new();
    let start = Instant::now();
    // BOTH jobs are leased at `start` before either settles. Settling one and
    // then advancing the clock would instead re-lease the FIRST job once its
    // own backoff elapsed -- which is correct scheduler behavior and exactly
    // what made an earlier version of this test read a third attempt's longer
    // deadline as "the earliest". Holding both leases keeps the two jobs
    // distinct, and settling them at different instants is what orders their
    // deadlines.
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    scheduler.activate(&key(1), 1, vec![ProbeValidator::CountTokens], payload());
    let first = scheduler.lease_due(start).expect("first job is due");
    let second = scheduler.lease_due(start).expect("second job is due");
    let _committed = first.settle(ProbeSettlement::Retryable, start);
    // Half a base interval later, so the second deadline is strictly the later
    // of the two and the minimum is unambiguous.
    let _committed = second.settle(ProbeSettlement::Retryable, start + PROBE_BACKOFF_BASE / 2);

    // Act / Assert: read at `start`, where the FIRST job's deadline is nearer.
    assert_eq!(
        scheduler.snapshot(start).next_retry_in,
        Some(backoff_for_attempt(1)),
        "the earliest deadline is what the scheduler will act on next"
    );
    assert_eq!(
        scheduler.snapshot(start).backing_off,
        2,
        "premise: both jobs really are backing off, so a minimum was taken over two"
    );
}
