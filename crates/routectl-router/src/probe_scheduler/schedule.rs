//! How one tracked job's schedule moves: backing off, charging an attempt, and
//! the terminal markers that stop an identity being re-asked.
//!
//! Split from the scheduler module for file size. These are `SchedulerInner`
//! methods, so they run under the scheduler's single lock exactly as before --
//! the split is an organizing choice and changes no locking or surface.

use std::time::Instant;

use super::{
    JobPhase, PROBE_MAX_ATTEMPTS, PROBE_MAX_DEFERRALS, PROBE_TOMBSTONE_CAPACITY,
    PROBE_TOMBSTONE_SATURATED_EVENT, SchedulerInner, backoff_for_attempt,
};
use crate::field_verdict::FieldVerdictKey;

impl SchedulerInner {
    // `pub(super)` rather than private: these are called from the scheduler
    // module's own `release`, and a child module's private items are not visible
    // to its PARENT. The surface is unchanged -- `SchedulerInner` itself is
    // private to `probe_scheduler`, so nothing outside that module can name these
    // however they are marked.

    /// Back the job off WITHOUT charging an attempt.
    ///
    /// For an outcome where no question was asked: the gate deferred before any
    /// dial. Charging an attempt would make a lane that is merely unavailable
    /// right now indistinguishable from one that is failing, and with
    /// [`PROBE_MAX_ATTEMPTS`] at three, three deferrals would ABANDON and
    /// TOMBSTONE the identity -- permanently, for the incarnation -- over a
    /// breaker that had simply not finished recovering. The attempt budget
    /// exists to bound repeated ANSWERS, not repeated non-answers.
    ///
    /// Still bounded on every other axis: the job holds one queue slot it
    /// already held, releases its concurrency slot, and cannot re-lease until
    /// the backoff elapses. The backoff is computed from the attempt count as
    /// usual, so a job deferred at attempt zero waits the base interval rather
    /// than spinning. Retirement and shutdown remain authoritative -- both drop
    /// the job outright regardless of phase.
    ///
    /// OCCUPANCY is bounded separately: at [`PROBE_MAX_DEFERRALS`] the job is
    /// EVICTED and its queue slot released, so no single activation can hold a
    /// slot for a whole incarnation. Eviction does NOT tombstone -- the
    /// identity was never answered,
    /// so later real traffic on a recovered lane is free to activate it again.
    /// Returning capacity is not the same as granting priority: continuous
    /// reactivation of an unprobeable lane can keep re-acquiring a slot.
    ///
    /// The eviction is observable through `deferral_evictions_total` rather
    /// than returned: no caller varies its behavior on it.
    pub(super) fn defer_without_charging_attempt(&mut self, index: usize, now: Instant) {
        let job = &mut self.jobs[index];
        job.deferrals = job.deferrals.saturating_add(1);
        if job.deferrals >= PROBE_MAX_DEFERRALS {
            self.jobs.swap_remove(index);
            self.counters.deferral_evictions_total += 1;
            return;
        }
        job.phase = JobPhase::BackingOff {
            due_at: now + backoff_for_attempt(job.attempts.saturating_add(1)),
        };
    }

    /// Bump `attempts` and either back the job off or abandon it at the
    /// attempt cap. The single place a retry is scheduled, so no path can
    /// schedule an unbounded one.
    pub(super) fn reschedule_or_abandon(&mut self, index: usize, now: Instant) {
        let job = &mut self.jobs[index];
        job.attempts = job.attempts.saturating_add(1);
        if job.attempts >= PROBE_MAX_ATTEMPTS {
            let key = job.key.clone();
            let generation = job.generation;
            self.jobs.swap_remove(index);
            self.counters.abandoned_total += 1;
            // Terminal for this incarnation: an identity that spent its
            // whole attempt budget must not be re-queued by the next
            // admitted request, or a failing lane re-enters the queue on
            // every burst of traffic.
            self.tombstone(key, generation);
            return;
        }
        job.phase = JobPhase::BackingOff {
            due_at: now + backoff_for_attempt(job.attempts),
        };
    }

    /// Record a terminal marker for `key` at `generation`.
    ///
    /// Bounded by [`PROBE_TOMBSTONE_CAPACITY`] and idempotent. When capacity
    /// is exhausted the marker cannot
    /// be stored, and DROPPING it would fail OPEN: the identity would be
    /// re-activatable on the next admitted request, which is exactly the
    /// re-ask loop tombstones exist to stop. So an overflow raises an
    /// incarnation-level SATURATION marker instead, and while that is set no
    /// identity may activate until retirement clears it. Refusing work is
    /// the safe direction; re-probing a settled question on every burst of
    /// traffic is not.
    pub(super) fn tombstone(&mut self, key: FieldVerdictKey, generation: u64) {
        if let Some(existing) = self.tombstones.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = existing.1.max(generation);
            return;
        }
        if self.tombstones.len() >= PROBE_TOMBSTONE_CAPACITY {
            // ONE line per saturation EPISODE: once saturated, every later
            // terminal settlement overflows too, so a line each would flood
            // exactly when the daemon is busiest. The counter carries the
            // volume; the line is the existence proof.
            //
            // Keyed on the marker being ABSENT rather than on the incarnation
            // differing. The two are currently equivalent -- activation
            // refuses every generation at or below a set marker, so no new job
            // queues and every queued job carries the saturating generation,
            // while a publication always draws a greater one and clears the
            // marker -- but absence states the intent directly and cannot
            // diverge if those surrounding invariants change.
            let first_of_episode = self.tombstone_saturated_at.is_none();
            self.tombstone_saturated_at = Some(
                self.tombstone_saturated_at
                    .map_or(generation, |existing: u64| existing.max(generation)),
            );
            self.counters.tombstone_saturations_total += 1;
            if first_of_episode {
                tracing::warn!(
                    capacity = PROBE_TOMBSTONE_CAPACITY,
                    tombstone_saturations_total = self.counters.tombstone_saturations_total,
                    "{PROBE_TOMBSTONE_SATURATED_EVENT}",
                );
            }
            return;
        }
        self.tombstones.push((key, generation));
    }
}
