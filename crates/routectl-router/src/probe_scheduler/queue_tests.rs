//! Queue mechanics: activation, dedupe, the depth and concurrency bounds,
//! lease release, backoff, the attempt cap, and retirement/shutdown.

use std::time::{Duration, Instant};

use super::test_support::{key, key_with_path, payload};
use super::{
    PROBE_BACKOFF_BASE, PROBE_BACKOFF_CEILING, PROBE_MAX_ATTEMPTS, PROBE_MAX_CONCURRENCY,
    PROBE_QUEUE_DEPTH, ProbeActivation, ProbeScheduler, ProbeSettlement, ProbeValidator,
    backoff_for_attempt,
};

#[test]
fn a_fresh_scheduler_holds_no_work_and_has_activated_nothing() {
    // Arrange / Act: construction alone -- no startup, install, config
    // parse, or reload step may enqueue a probe.
    let scheduler = ProbeScheduler::new();

    // Assert
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 0, "construction must enqueue no probe");
    assert_eq!(snap.in_flight, 0);
    assert_eq!(
        snap.activations_total, 0,
        "no lane may activate before its first admitted real request"
    );
}

#[test]
fn the_first_activation_for_an_identity_queues_exactly_one_job() {
    // Arrange
    let scheduler = ProbeScheduler::new();

    // Act
    let outcome = scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());

    // Assert
    assert_eq!(outcome, ProbeActivation::Queued);
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 1);
    assert_eq!(snap.activations_total, 1);
}

#[test]
fn a_repeat_activation_for_the_same_lane_and_capability_dedupes() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    assert_eq!(
        scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );

    // Act: the second and third admitted requests on the same lane.
    let second = scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let third = scheduler.activate(
        &key(0),
        1,
        vec![ProbeValidator::ExpectedRejection],
        payload(),
    );

    // Assert: dedupe is per lane AND capability, so neither adds a job.
    assert_eq!(second, ProbeActivation::Deduped);
    assert_eq!(third, ProbeActivation::Deduped);
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 1, "one job per lane/capability, never more");
    assert_eq!(snap.deduped_total, 2);
}

#[test]
fn distinct_capabilities_on_one_lane_are_separate_jobs() {
    // Arrange: same lane (state key), two different capability paths.
    let scheduler = ProbeScheduler::new();

    // Act
    scheduler.activate(
        &key_with_path("thinking.enabled.display"),
        1,
        vec![ProbeValidator::CountTokens],
        payload(),
    );
    scheduler.activate(
        &key_with_path("tools.custom.schema"),
        1,
        vec![ProbeValidator::CountTokens],
        payload(),
    );

    // Assert: dedupe keys on the identity, so two capabilities coexist.
    assert_eq!(scheduler.snapshot().queued, 2);
}

#[test]
fn an_in_flight_identity_dedupes_rather_than_queueing_a_second_job() {
    // Arrange: one job leased out, so it is no longer in the queue.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("the queued job is due");

    // Act
    let repeat = scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());

    // Assert: a leased identity still occupies its single-job slot.
    assert_eq!(repeat, ProbeActivation::Deduped);
    assert_eq!(scheduler.snapshot().queued, 0);
    drop(lease);
}

#[test]
fn the_queue_refuses_past_its_fixed_depth_and_counts_the_refusal() {
    // Arrange: fill the queue with distinct identities.
    let scheduler = ProbeScheduler::new();
    for n in 0..PROBE_QUEUE_DEPTH {
        assert_eq!(
            scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload()),
            ProbeActivation::Queued,
            "identity {n} is within the depth bound"
        );
    }

    // Act: one past the bound.
    let overflow = scheduler.activate(
        &key(PROBE_QUEUE_DEPTH),
        1,
        vec![ProbeValidator::CountTokens],
        payload(),
    );

    // Assert
    assert_eq!(overflow, ProbeActivation::QueueFull);
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, PROBE_QUEUE_DEPTH, "the bound is not exceeded");
    assert_eq!(snap.queue_full_total, 1, "the refusal is diagnosable");
}

#[test]
fn no_more_than_the_concurrency_bound_of_leases_are_handed_out_at_once() {
    // Arrange: more queued jobs than the concurrency bound.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    for n in 0..PROBE_MAX_CONCURRENCY + 3 {
        scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload());
    }

    // Act: drain leases while holding every one of them.
    let mut held = Vec::new();
    while let Some(lease) = scheduler.lease_due(now) {
        held.push(lease);
        assert!(
            held.len() <= PROBE_MAX_CONCURRENCY,
            "leased {} concurrently against a bound of {PROBE_MAX_CONCURRENCY}",
            held.len()
        );
    }

    // Assert
    assert_eq!(held.len(), PROBE_MAX_CONCURRENCY);
    assert_eq!(scheduler.snapshot().in_flight, PROBE_MAX_CONCURRENCY);
}

#[test]
fn releasing_a_lease_frees_the_slot_for_the_next_job() {
    // Arrange: saturate concurrency.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    for n in 0..=PROBE_MAX_CONCURRENCY {
        scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload());
    }
    let mut held: Vec<_> = std::iter::from_fn(|| scheduler.lease_due(now)).collect();
    assert!(scheduler.lease_due(now).is_none(), "bound holds while full");

    // Act
    let lease = held.pop().expect("a lease is held");
    let _committed = lease.settle(ProbeSettlement::Resolved, now);

    // Assert
    assert!(
        scheduler.lease_due(now).is_some(),
        "a settled lease must free its slot"
    );
}

#[test]
fn a_dropped_unsettled_lease_frees_its_slot() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    for n in 0..=PROBE_MAX_CONCURRENCY {
        scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload());
    }
    let held: Vec<_> = std::iter::from_fn(|| scheduler.lease_due(now)).collect();
    assert!(scheduler.lease_due(now).is_none());

    // Act: an early return, a `?`, or a cancelled future.
    drop(held);

    // Assert: the slots came back, so the still-queued job can lease.
    // Bound, not asserted inline: a lease dropped at the end of the
    // assertion statement would put `in_flight` back to zero and the
    // check below would read the slot as never taken.
    let next = scheduler.lease_due(now);
    assert!(
        next.is_some(),
        "an unsettled drop must free the slot exactly as a settlement does"
    );
    assert_eq!(scheduler.snapshot().in_flight, 1);
    drop(next);
}

#[test]
fn a_timed_out_settlement_releases_the_slot_and_counts_the_timeout() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let start = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(start).expect("job is due");

    // Act: the worker cancelled the operation at its deadline and says so.
    let _committed = lease.settle(ProbeSettlement::TimedOut, start);

    // Assert
    let snap = scheduler.snapshot();
    assert_eq!(snap.in_flight, 0, "the slot must come back");
    assert_eq!(snap.timeouts_total, 1, "the timeout is counted");
    assert_eq!(
        snap.backing_off, 1,
        "a timed-out job backs off rather than re-leasing immediately"
    );
}

#[test]
fn a_timed_out_job_is_abandoned_at_the_attempt_cap_like_any_retry() {
    // A timeout must not buy unbounded attempts: it is charged exactly as a
    // retryable settlement is.
    let scheduler = ProbeScheduler::new();
    let mut now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());

    for _ in 0..PROBE_MAX_ATTEMPTS {
        let lease = scheduler.lease_due(now).expect("due within the cap");
        let _committed = lease.settle(ProbeSettlement::TimedOut, now);
        now += PROBE_BACKOFF_CEILING + Duration::from_secs(1);
    }

    assert!(scheduler.lease_due(now).is_none());
    assert_eq!(scheduler.snapshot().abandoned_total, 1);
}

#[test]
fn a_retryable_settlement_backs_the_job_off_before_it_is_due_again() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let start = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(start).expect("job is due");

    // Act
    let _committed = lease.settle(ProbeSettlement::Retryable, start);

    // Assert: not immediately re-leasable, but due after the backoff.
    assert!(
        scheduler.lease_due(start).is_none(),
        "a retryable settlement must not re-lease in the same instant"
    );
    assert_eq!(scheduler.snapshot().backing_off, 1);
    assert!(
        scheduler
            .lease_due(start + PROBE_BACKOFF_BASE + Duration::from_secs(1))
            .is_some(),
        "the job becomes due once its backoff elapses"
    );
}

#[test]
fn backoff_grows_with_the_attempt_and_stops_at_the_ceiling() {
    // Arrange / Act / Assert: monotonic, and clamped.
    let first = backoff_for_attempt(1);
    let second = backoff_for_attempt(2);
    assert_eq!(first, PROBE_BACKOFF_BASE);
    assert!(second > first, "backoff must grow between attempts");
    for attempt in 1..64 {
        assert!(
            backoff_for_attempt(attempt) <= PROBE_BACKOFF_CEILING,
            "attempt {attempt} exceeded the backoff ceiling"
        );
    }
    assert_eq!(backoff_for_attempt(63), PROBE_BACKOFF_CEILING);
}

#[test]
fn a_job_is_abandoned_once_it_exhausts_the_attempt_cap() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let mut now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());

    // Act: settle retryable until the cap is spent.
    for _ in 0..PROBE_MAX_ATTEMPTS {
        let lease = scheduler
            .lease_due(now)
            .expect("the job is due within the attempt cap");
        let _committed = lease.settle(ProbeSettlement::Retryable, now);
        now += PROBE_BACKOFF_CEILING + Duration::from_secs(1);
    }

    // Assert: bounded retry, not an endless one.
    assert!(
        scheduler.lease_due(now).is_none(),
        "a job past the attempt cap must never lease again"
    );
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.backing_off, 0);
    assert_eq!(snap.abandoned_total, 1);
}

#[test]
fn retirement_cancels_queued_work_from_a_superseded_generation() {
    // Arrange: two generations of queued work.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    scheduler.activate(&key(1), 2, vec![ProbeValidator::CountTokens], payload());

    // Act: the router state at generation 1 is retired.
    let cancelled = scheduler.retire_before(2);

    // Assert
    assert_eq!(cancelled, 1);
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 1, "only live-generation work survives");
    assert_eq!(snap.retired_total, 1);
    let lease = scheduler.lease_due(now).expect("the live job is still due");
    assert_eq!(lease.generation(), 2);
}

#[test]
fn activation_against_a_retired_generation_schedules_no_work() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 5, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("job is due");
    let _committed = lease.settle(ProbeSettlement::Resolved, now);
    scheduler.retire_before(5);

    // Act: a request still holding the pre-swap router activates.
    let outcome = scheduler.activate(&key(1), 4, vec![ProbeValidator::CountTokens], payload());

    // Assert
    assert_eq!(outcome, ProbeActivation::Retired);
    assert_eq!(scheduler.snapshot().queued, 0);
}

#[test]
fn a_job_whose_incarnation_was_retired_cannot_be_leased_before_the_sweep_removes_it() {
    // Retirement sweeps the table AND raises the floor, but a lease can be
    // attempted in between -- and a caller that only raised the floor would
    // still hand out work belonging to retired router state. This pins the
    // per-lease generation check independently of the sweep, by re-inserting
    // a stale job after the sweep has run.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    scheduler.retire_before(5);
    assert_eq!(scheduler.snapshot().queued, 0, "the sweep removed it");

    // A request still holding generation 1 cannot queue (activation
    // refuses), so drive the window directly: generation 4 is below the
    // floor of 5 but was queued before it moved.
    let scheduler2 = ProbeScheduler::new();
    scheduler2.activate(&key(0), 4, vec![ProbeValidator::CountTokens], payload());
    scheduler2.raise_retirement_floor_without_sweeping_for_tests(5);

    assert!(
        scheduler2.lease_due(now).is_none(),
        "a job below the retirement floor must not lease even while still tracked"
    );
}

#[test]
fn a_stale_settlement_releases_its_slot_and_schedules_no_retry() {
    // Arrange: a lease taken before the retirement boundary.
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload());
    let lease = scheduler.lease_due(now).expect("job is due");
    scheduler.retire_before(2);

    // Act: the outstanding probe settles retryable against retired state.
    let _committed = lease.settle(ProbeSettlement::Retryable, now);

    // Assert: slot freed, nothing rescheduled on retired state.
    let snap = scheduler.snapshot();
    assert_eq!(snap.in_flight, 0, "a stale settlement releases its slot");
    assert_eq!(snap.queued, 0);
    assert_eq!(
        snap.backing_off, 0,
        "a stale settlement must schedule no follow-up work"
    );
    assert_eq!(snap.stale_settlements_total, 1);
}

#[test]
fn shutdown_cancels_every_queued_job() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let now = Instant::now();
    for n in 0..3 {
        scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload());
    }

    // Act
    let cancelled = scheduler.cancel_all();

    // Assert
    assert_eq!(cancelled, 3);
    let snap = scheduler.snapshot();
    assert_eq!(snap.queued, 0);
    assert!(
        scheduler.lease_due(now).is_none(),
        "no work may be leased after shutdown cancellation"
    );
}
