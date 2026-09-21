//! Behavior under real parallelism, and the two occupancy bounds that keep a
//! never-answerable lane from holding a queue slot forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::test_support::{key, payload};
use super::{
    PROBE_BACKOFF_CEILING, PROBE_MAX_CONCURRENCY, PROBE_MAX_DEFERRALS, PROBE_QUEUE_DEPTH,
    ProbeActivation, ProbeScheduler, ProbeSettlement, ProbeValidator, ReleaseOutcome,
};

#[test]
fn concurrent_activation_of_one_identity_queues_exactly_one_job() {
    // Arrange: many threads racing the same lane/capability, the shape a
    // burst of first admitted requests takes.
    let scheduler = Arc::new(ProbeScheduler::new());
    let identity = key(0);
    let queued = Arc::new(AtomicUsize::new(0));

    // Act
    let handles: Vec<_> = (0..64)
        .map(|_| {
            let scheduler = Arc::clone(&scheduler);
            let identity = identity.clone();
            let queued = Arc::clone(&queued);
            thread::spawn(move || {
                if scheduler.activate(&identity, 1, vec![ProbeValidator::CountTokens], payload())
                    == ProbeActivation::Queued
                {
                    queued.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("activation thread must not panic");
    }

    // Assert
    assert_eq!(
        queued.load(Ordering::Relaxed),
        1,
        "exactly one racing activation may queue the job"
    );
    assert_eq!(scheduler.snapshot().queued, 1);
}

#[test]
fn concurrent_leasing_never_exceeds_the_concurrency_bound() {
    // Arrange
    let scheduler = Arc::new(ProbeScheduler::new());
    let now = Instant::now();
    for n in 0..PROBE_QUEUE_DEPTH {
        scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload());
    }
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    // Act
    let handles: Vec<_> = (0..64)
        .map(|_| {
            let scheduler = Arc::clone(&scheduler);
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            thread::spawn(move || {
                for _ in 0..32 {
                    if let Some(lease) = scheduler.lease_due(now) {
                        let now_live = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now_live, Ordering::SeqCst);
                        live.fetch_sub(1, Ordering::SeqCst);
                        let _committed = lease.settle(ProbeSettlement::Resolved, now);
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("lease thread must not panic");
    }

    // Assert
    assert!(
        peak.load(Ordering::SeqCst) <= PROBE_MAX_CONCURRENCY,
        "peak concurrent leases {} exceeded the bound {PROBE_MAX_CONCURRENCY}",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(scheduler.snapshot().in_flight, 0);
}

#[test]
fn spending_a_step_with_another_free_step_left_advances_rather_than_exhausting() {
    // A TWO-free-step plan, which is the only shape that distinguishes an
    // advance from an exhaustion. The scheduler decides which it is, inside
    // the settling critical section, so the settlement itself reports the
    // answer -- the caller never reads the cursor separately.
    //
    // Arrange: two free steps. Built DIRECTLY rather than through
    // `validator_plan`, which excludes `ExpectedRejection` because it has no
    // grounded template -- so no plan that function returns has two free steps.
    // The scheduler's advance logic is general over plan length, and a
    // two-step plan is what exercises it.
    let scheduler = ProbeScheduler::default();
    let plan = vec![
        ProbeValidator::CountTokens,
        ProbeValidator::ExpectedRejection,
    ];
    assert_eq!(
        scheduler.activate(&key(0), 1, plan, payload()),
        ProbeActivation::Queued
    );
    let now = Instant::now();
    let lease = scheduler.lease_due(now).expect("the queued job is due");
    assert_eq!(
        lease.validator(),
        ProbeValidator::CountTokens,
        "premise: the plan must start on its FIRST free step"
    );

    // Act
    let release = lease.settle(ProbeSettlement::SpentFreeStep, now);

    // Assert: committed, but the plan is NOT spent -- so no paid candidate.
    assert!(release.committed());
    assert!(
        !release.exhausted_free_plan(),
        "a plan with a free step left must not report itself spent, or a lane \
         reaches the paid class having never run step two"
    );
    let snap = scheduler.snapshot();
    assert_eq!(snap.free_exhausted_total, 0);
    assert_eq!(
        snap.backing_off, 1,
        "the advanced job backs off before running its next step"
    );
    assert_eq!(snap.tombstoned, 0, "an advanced job is not terminal");

    // And the NEXT lease runs the SECOND step, which is what proves the
    // cursor actually moved rather than the job merely being rescheduled.
    let later = now + PROBE_BACKOFF_CEILING;
    let second = scheduler
        .lease_due(later)
        .expect("the advanced job is due again");
    assert_eq!(
        second.validator(),
        ProbeValidator::ExpectedRejection,
        "the advance must move the cursor to the next free step"
    );

    // Act again: spending the LAST step runs the plan out.
    let release = second.settle(ProbeSettlement::SpentFreeStep, later);

    // Assert: this one IS the exhaustion, and it is terminal.
    assert!(release.committed());
    assert!(
        release.exhausted_free_plan(),
        "spending the last free step is what makes the paid class a candidate"
    );
    let snap = scheduler.snapshot();
    assert_eq!(snap.free_exhausted_total, 1);
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.backing_off, 0);
    assert_eq!(snap.in_flight, 0);
    assert_eq!(
        snap.tombstoned, 1,
        "an exhausted identity is terminal for this incarnation"
    );
}

#[test]
fn a_queue_of_deferred_jobs_frees_capacity_for_a_healthy_lane() {
    // LIVENESS. A deferral charges no attempt, which is right -- no question
    // was asked -- but "charges nothing" must not mean "holds a queue slot
    // forever". Lanes whose breakers stay open would otherwise occupy all
    // PROBE_QUEUE_DEPTH slots for the whole incarnation, leaving no capacity
    // for a HEALTHY lane arriving later.
    //
    // Occupancy is therefore bounded separately: at PROBE_MAX_DEFERRALS the job
    // is EVICTED and its slot released. That returns capacity; it does not
    // grant priority, which continuous reactivation could still contend for.
    let scheduler = ProbeScheduler::default();

    // Fill the queue completely.
    for n in 0..PROBE_QUEUE_DEPTH {
        assert_eq!(
            scheduler.activate(&key(n), 1, vec![ProbeValidator::CountTokens], payload()),
            ProbeActivation::Queued
        );
    }
    // A healthy lane arriving now is refused: the queue is full.
    let healthy = key(PROBE_QUEUE_DEPTH + 1);
    assert_eq!(
        scheduler.activate(&healthy, 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::QueueFull,
        "premise: a full queue must refuse, or this test proves no liveness"
    );

    // Every queued job is deferred to its ceiling. Each lease is settled before
    // the next is taken, so only one is ever outstanding and the concurrency
    // bound never binds -- this loop drains the whole due set in one round.
    let mut now = Instant::now();
    for _ in 0..PROBE_MAX_DEFERRALS {
        while let Some(lease) = scheduler.lease_due(now) {
            let release = lease.settle(ProbeSettlement::Deferred, now);
            assert!(release.committed(), "a deferral on live state must commit");
            assert!(
                !release.exhausted_free_plan(),
                "a deferral spends no free step"
            );
        }
        now += PROBE_BACKOFF_CEILING + Duration::from_secs(1);
    }

    let snap = scheduler.snapshot();
    assert_eq!(
        snap.deferral_evictions_total, PROBE_QUEUE_DEPTH as u64,
        "every deferred job must have been evicted at the ceiling"
    );
    assert_eq!(snap.abandoned_total, 0, "eviction is not abandonment");
    assert_eq!(
        snap.tombstoned, 0,
        "and must NOT tombstone: the identity was never answered, so later \
         real traffic must be free to activate it again"
    );

    // THE LIVENESS ASSERTION: the healthy lane now gets in.
    assert_eq!(
        scheduler.activate(&healthy, 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued,
        "evicting deferred jobs must free capacity for a probeable lane"
    );
}

#[test]
fn an_evicted_identity_may_be_reactivated_and_settle() {
    // The other half of "evict, do not tombstone": the identity must be able
    // to come back and finish. A tombstone would refuse it for the whole
    // incarnation, which is wrong for a question that was never asked.
    let scheduler = ProbeScheduler::default();
    let id = key(0);
    assert_eq!(
        scheduler.activate(&id, 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );

    let mut now = Instant::now();
    for round in 0..PROBE_MAX_DEFERRALS {
        // Exactly ONE job is queued, so a lease must exist on every round up to
        // the ceiling. `expect` rather than `if let`: a silently-absent lease
        // would make the loop a no-op and the eviction assertion below vacuous.
        let lease = scheduler
            .lease_due(now)
            .unwrap_or_else(|| panic!("the single queued job must be leasable on round {round}"));
        let release = lease.settle(ProbeSettlement::Deferred, now);
        assert!(release.committed(), "a deferral on live state must commit");
        now += PROBE_BACKOFF_CEILING + Duration::from_secs(1);
    }
    assert_eq!(scheduler.snapshot().deferral_evictions_total, 1, "premise");
    assert!(
        scheduler.lease_due(now).is_none(),
        "premise: the evicted job is gone from the queue"
    );

    // Reactivation: accepted, not tombstone-refused.
    assert_eq!(
        scheduler.activate(&id, 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued,
        "an evicted identity must be reactivatable by later real traffic"
    );

    // And it can now settle its question normally, with a FRESH deferral
    // budget -- the eviction did not leave the identity poisoned.
    let lease = scheduler
        .lease_due(now)
        .expect("the reactivated job must be leasable");
    let release = lease.settle(ProbeSettlement::Resolved, now);
    assert!(release.committed());
    let snap = scheduler.snapshot();
    assert_eq!(snap.resolved_total, 1, "the reactivated job answered");
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.in_flight, 0);
}

#[test]
fn deferrals_below_the_ceiling_keep_the_job_queued() {
    // POSITIVE CONTROL for the eviction above: one deferral short of the
    // ceiling the job is still tracked and still backing off, so the eviction
    // is about the CEILING rather than about any deferral dropping the job.
    let scheduler = ProbeScheduler::default();
    assert_eq!(
        scheduler.activate(&key(0), 1, vec![ProbeValidator::CountTokens], payload()),
        ProbeActivation::Queued
    );

    let mut now = Instant::now();
    for _ in 0..PROBE_MAX_DEFERRALS - 1 {
        let lease = scheduler.lease_due(now).expect("still leasable");
        let release = lease.settle(ProbeSettlement::Deferred, now);
        assert!(release.committed(), "a deferral on live state must commit");
        now += PROBE_BACKOFF_CEILING + Duration::from_secs(1);
    }

    let snap = scheduler.snapshot();
    assert_eq!(
        snap.deferral_evictions_total, 0,
        "below the ceiling nothing is evicted"
    );
    assert_eq!(snap.backing_off, 1, "the job is still queued to retry");
    assert_eq!(snap.deferrals_total, u64::from(PROBE_MAX_DEFERRALS) - 1);
    assert_eq!(snap.abandoned_total, 0, "and no attempt was ever charged");
}

// The paid-slot group lives in a sibling file to keep every file under the size
// ceiling. It compiles into THIS module via `include!`, so the imports above
// stay in scope and no test's module path changes.
include!("paid_slot_tests.rs");
include!("retirement_tests.rs");
