//! Bounded, lazily-activated probe scheduling: the router-owned queue a
//! lane enters only after its first admitted real request.
//!
//! # Why lazy, and why activation never touches the request
//!
//! Startup, install, config parsing, and hot reload make no probe call:
//! nothing in this module is reachable from a construction or reload path,
//! and [`ProbeScheduler::activate`] is a pure bookkeeping call that
//! enqueues a job and returns -- it dials nothing, awaits nothing, and
//! holds its lock only for the duration of a bounded in-memory update. A
//! lane therefore costs nothing until real traffic proves it is in use,
//! and the request that proves it is not made slower by the discovery.
//!
//! # Every bound is fixed in code
//!
//! [`PROBE_QUEUE_DEPTH`], [`PROBE_MAX_CONCURRENCY`],
//! [`PROBE_OPERATION_TIMEOUT`], [`PROBE_BACKOFF_BASE`],
//! [`PROBE_BACKOFF_CEILING`] and [`PROBE_MAX_ATTEMPTS`] are constants, not
//! operator knobs and never read from the environment: they are the
//! safety envelope that keeps background validation from competing with
//! served traffic, and a parameter an operator (or a process's own
//! environment) can widen is an exemption that leaves no diff.
//!
//! One job per identity is tracked at a time, in any phase, so a burst of
//! first-admitted requests on one lane produces exactly one job rather
//! than one per request. The queue refuses past its depth with a counted,
//! closed-set diagnostic instead of growing. A leased job's hold on a
//! concurrency slot is bounded by the WORKER, which wraps the operation in
//! [`PROBE_OPERATION_TIMEOUT`] and settles the lease explicitly on expiry;
//! this module stores no deadline and expires no lease on a clock. That is
//! deliberate: the worker's timeout DROPS the operation future, so the slot
//! and the real upstream work are released together, whereas a sweep keyed
//! on a clock the worker may not share would free the slot while the
//! operation kept running. A retryable outcome backs off geometrically to a
//! ceiling and is abandoned at the attempt cap, so an upstream that always
//! fails is retried a bounded number of times rather than forever.
//!
//! # Free validation before any paid path
//!
//! [`validator_plan`] orders the free validators ahead of the paid one for
//! every lane, and [`paid_probe_permitted`] opens the paid path only when
//! free validation actually failed to answer AND the provider carries a
//! non-zero daily cap. Free validators have no spend cap because they bill
//! no inference, which is exactly why they must still obey every bound
//! above -- "free" bounds the bill, not the load.
//!
//! # Generations, retirement, and stale settlement
//!
//! Every job carries the router generation that activated it. Retirement
//! ([`ProbeScheduler::retire_before`]) drops every job from a superseded
//! generation, an activation arriving from a retired router is refused,
//! and a settlement for a job this scheduler no longer tracks releases its
//! slot and schedules nothing. Nothing is retried on retired state.
//!
//! This module holds no request bytes and no upstream text. Its identities
//! come from [`FieldVerdictKey`], whose permanent field keys are minted
//! only by the existing normalized constructor, and its diagnostics are
//! closed-set tokens and counters.
//!
//! # The paid class is never a queued job
//!
//! [`ProbeScheduler::activate`] filters the submitted plan to its FREE
//! steps, so no job in this table can name a paid validator and no lease
//! can hand one to a worker. An identity whose free steps all run without
//! settling leaves the scheduler on the
//! [`ProbeSettlement::SpentFreeStep`] that runs its plan out, reported by
//! [`ReleaseOutcome::exhausted_free_plan`]. That is the input the router draws
//! a paid CANDIDATE from -- necessary but not sufficient, since
//! [`paid_probe_permitted`] must also agree on the provider's daily cap -- and
//! a candidate records that free validation is spent rather than granting
//! permission to spend money. Nothing in this module dials a paid endpoint.
//!
//! # Two bounds live OUTSIDE this module
//!
//! [`PROBE_OPERATION_TIMEOUT`] is a constant here but is ENFORCED by the
//! worker, which wraps each operation future in it; expiry drops that future
//! and settles the lease, and this module expires nothing on a clock. And the
//! probe driver's tick interval -- how often anything here is asked to run at
//! all -- lives with the daemon that owns the driver. Neither can be read off
//! the code in this module, so both are named here rather than left to be
//! discovered.

use std::time::Instant;

use parking_lot::Mutex;

use crate::field_verdict::FieldVerdictKey;

mod bounds;
mod payload;
mod vocab;

// The module's flat surface: every consumer names `probe_scheduler::X`, so the
// three-file split inside is an organizing choice rather than a new API shape.
pub use bounds::{
    PROBE_MAX_ATTEMPTS, PROBE_MAX_CONCURRENCY, PROBE_MAX_DEFERRALS, PROBE_OPERATION_TIMEOUT,
    PROBE_PAYLOAD_REFUSED_EVENT, PROBE_QUEUE_DEPTH, PROBE_TOMBSTONE_CAPACITY,
    PROBE_TOMBSTONE_SATURATED_EVENT, backoff_for_attempt,
};
// Read only by the test sidecars. Exported alongside the rest rather than
// cfg-gated, so the module's surface does not differ per build. Each allow
// covers ONE exact-name block, so a genuinely dead export anywhere else in the
// surface still warns.
#[allow(unused_imports)]
pub use bounds::{PROBE_BACKOFF_BASE, PROBE_BACKOFF_CEILING};
pub use payload::ProbePayload;
#[allow(unused_imports)]
pub use payload::{
    PROBE_BETA_MAX_COUNT_PER_SOURCE, PROBE_BETA_MAX_TOKEN_BYTES, PROBE_BETA_MAX_TOTAL_BYTES,
    PROBE_MODELED_DISPLAY_VALUES,
};
pub use vocab::{
    FreeValidatorOutcome, ProbeActivation, ProbeSchedulerSnapshot, ProbeSettlement, ProbeValidator,
    paid_probe_permitted, validator_plan,
};

/// Where one job is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobPhase {
    /// Due to be leased.
    Queued,
    /// Leased. Carries no deadline: the WORKER bounds the operation by
    /// wrapping it in [`PROBE_OPERATION_TIMEOUT`] and settling the lease on
    /// expiry, and nothing in this module expires a lease on a clock. A
    /// deadline stored here would be a value no code reads.
    InFlight,
    /// Waiting until `due_at` before it may be leased again.
    BackingOff { due_at: Instant },
}

#[derive(Debug)]
struct Job {
    key: FieldVerdictKey,
    generation: u64,
    payload: ProbePayload,
    /// The FREE plan this job walks, and the cursor into it. Held on the
    /// job rather than recomputed per lease so a plan that changes shape
    /// (a lane losing count_tokens support across a reload) cannot make a
    /// half-walked job skip or repeat a step.
    plan: Vec<ProbeValidator>,
    plan_cursor: usize,
    attempts: u32,
    /// Deferrals accumulated, bounding QUEUE OCCUPANCY rather than answers.
    /// Counted separately from `attempts` because a deferral charges no
    /// attempt, yet an indefinitely-deferred job must not hold a queue slot
    /// forever, leaving no capacity for probeable lanes. See
    /// [`PROBE_MAX_DEFERRALS`], which bounds occupancy per activation rather
    /// than guaranteeing priority.
    deferrals: u32,
    phase: JobPhase,
    /// Bumped on every lease. A release whose sequence does not match the
    /// job's current one is from a superseded lease -- an operation its
    /// worker timed out that later returned anyway -- and must not move a
    /// job that has since been re-leased.
    lease_seq: u64,
}

impl Job {
    /// The validator this job runs next, or `None` when its free plan is
    /// exhausted.
    fn validator(&self) -> Option<ProbeValidator> {
        self.plan.get(self.plan_cursor).copied()
    }
}

/// The bounded router-owned probe scheduler.
///
/// One lock guards the whole job table, and every operation takes it for
/// its entire critical section, so a dedupe check, a lease, a settlement,
/// and a retirement are each atomic against concurrent callers. That is
/// what makes "one job per lane and capability" and the concurrency
/// ceiling true under real parallel traffic rather than only on one
/// thread.
#[derive(Debug, Default)]
pub struct ProbeScheduler {
    inner: Mutex<SchedulerInner>,
}

#[derive(Debug, Default)]
struct SchedulerInner {
    /// Bounded by [`PROBE_QUEUE_DEPTH`], so the linear scans below are
    /// over at most a handful of entries and need no index.
    jobs: Vec<Job>,
    /// Terminal `(identity, incarnation)` markers: identities that resolved,
    /// exhausted their free plan, or spent their attempt cap. Bounded by
    /// [`PROBE_TOMBSTONE_CAPACITY`] -- its OWN bound, deliberately larger
    /// than the queue's -- and cleared by retirement (every marker STRICTLY
    /// BELOW the incoming generation) and wholesale by shutdown, so this
    /// never grows with traffic or with time.
    tombstones: Vec<(FieldVerdictKey, u64)>,
    /// Set to the incarnation at which terminal-marker capacity was
    /// exhausted. While set, EVERY activation at or below that incarnation is
    /// refused: a terminal marker that could not be stored would otherwise
    /// fail open and let its identity re-enter the queue on the next admitted
    /// request. Cleared by retirement, like the markers themselves.
    tombstone_saturated_at: Option<u64>,
    /// Generations strictly below this are retired: no job from one may be
    /// tracked, activated, or retried.
    retired_below: u64,
    next_lease_seq: u64,
    counters: ProbeSchedulerSnapshot,
}

impl SchedulerInner {
    fn position(&self, key: &FieldVerdictKey) -> Option<usize> {
        self.jobs.iter().position(|job| &job.key == key)
    }

    fn count_phase(&self, matcher: impl Fn(JobPhase) -> bool) -> usize {
        self.jobs.iter().filter(|job| matcher(job.phase)).count()
    }

    fn in_flight(&self) -> usize {
        self.count_phase(|phase| matches!(phase, JobPhase::InFlight))
    }

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
    fn defer_without_charging_attempt(&mut self, index: usize, now: Instant) {
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
    fn reschedule_or_abandon(&mut self, index: usize, now: Instant) {
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
    fn tombstone(&mut self, key: FieldVerdictKey, generation: u64) {
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

impl ProbeScheduler {
    /// A fresh scheduler with no work. Construction is the only thing a
    /// startup or reload path does here, and it schedules nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a probe for `key` at `generation`, on behalf of an admitted
    /// real request. Non-blocking bookkeeping: never dials, never awaits,
    /// and never delays its caller.
    ///
    /// Takes no clock: a newly queued job is immediately due, so there is
    /// no deadline or backoff to compute here. Only leasing and settling
    /// need one.
    ///
    /// Refuses -- without queueing -- when `generation` is retired, when
    /// this identity already has a tracked job (dedupe by lane and
    /// capability, since that is exactly what the identity is), or when
    /// the queue is at [`PROBE_QUEUE_DEPTH`]. Each refusal is counted and
    /// returns its own closed-set token.
    pub fn activate(
        &self,
        key: &FieldVerdictKey,
        generation: u64,
        plan: Vec<ProbeValidator>,
        payload: ProbePayload,
    ) -> ProbeActivation {
        let mut inner = self.inner.lock();
        if generation < inner.retired_below {
            return ProbeActivation::Retired;
        }
        // A terminal tombstone refuses re-activation for the REST of this
        // incarnation: the identity has already resolved, exhausted its free
        // plan, or spent its attempt cap, so re-queueing it on the next
        // admitted request would re-ask a settled question on every burst of
        // traffic. Retirement clears the tombstones, so a republished router
        // asks again -- the answer can legitimately have changed by then.
        // Fail CLOSED once terminal-marker capacity is exhausted: some
        // identity's marker could not be stored, so this scheduler can no
        // longer tell a settled identity from a fresh one.
        if inner
            .tombstone_saturated_at
            .is_some_and(|saturated| generation <= saturated)
        {
            inner.counters.tombstone_refusals_total += 1;
            return ProbeActivation::Tombstoned;
        }
        if inner
            .tombstones
            .iter()
            .any(|(k, inc)| k == key && *inc >= generation)
        {
            inner.counters.tombstone_refusals_total += 1;
            return ProbeActivation::Tombstoned;
        }
        if inner.position(key).is_some() {
            inner.counters.deduped_total += 1;
            return ProbeActivation::Deduped;
        }
        if inner.jobs.len() >= PROBE_QUEUE_DEPTH {
            inner.counters.queue_full_total += 1;
            return ProbeActivation::QueueFull;
        }
        // Filtered to FREE steps here, so no path in this module can lease a paid
        // validator into execution. What makes an exhausted plan a paid
        // CANDIDATE is `paid_probe_permitted`, not this filter.
        let plan: Vec<ProbeValidator> = plan.into_iter().filter(|v| v.is_free()).collect();
        if plan.is_empty() {
            return ProbeActivation::NoFreeValidator;
        }
        inner.jobs.push(Job {
            key: key.clone(),
            generation,
            payload,
            plan,
            plan_cursor: 0,
            attempts: 0,
            deferrals: 0,
            phase: JobPhase::Queued,
            lease_seq: 0,
        });
        inner.counters.activations_total += 1;
        ProbeActivation::Queued
    }

    /// Lease one job that is due at `now`, or `None` when nothing is due or
    /// the concurrency ceiling is reached.
    ///
    /// Returns the FIRST due job in table order -- which is not arrival order,
    /// since settlements use `swap_remove`. The choice is unspecified on purpose:
    /// each job carries its own backoff deadline and attempt budget, so nothing
    /// depends on it. A worker pass leases until this returns `None`, which
    /// happens at the CONCURRENCY ceiling as well as at "nothing due", so a pass
    /// takes up to that many jobs and a job not taken waits for the next pass.
    /// Neither "oldest" (a fairness guarantee) nor "the whole due batch" (a
    /// completeness one) is offered.
    ///
    /// The returned guard owns the slot: settling or dropping it releases
    /// the slot, so no caller path can strand one.
    pub fn lease_due(&self, now: Instant) -> Option<ProbeLease<'_>> {
        let mut inner = self.inner.lock();
        if inner.in_flight() >= PROBE_MAX_CONCURRENCY {
            return None;
        }
        let retired_below = inner.retired_below;
        let index = inner.jobs.iter().position(|job| {
            // A job from a retired incarnation is never leasable, even if
            // retirement has not yet swept it: the sweep and the lease are
            // separate calls, and the window between them must not run work.
            if job.generation < retired_below || job.validator().is_none() {
                return false;
            }
            match job.phase {
                JobPhase::Queued => true,
                JobPhase::BackingOff { due_at } => due_at <= now,
                JobPhase::InFlight => false,
            }
        })?;
        inner.next_lease_seq += 1;
        let lease_seq = inner.next_lease_seq;
        let job = &mut inner.jobs[index];
        let validator = job.validator().expect("checked in the predicate above");
        job.phase = JobPhase::InFlight;
        job.lease_seq = lease_seq;
        Some(ProbeLease {
            scheduler: self,
            key: job.key.clone(),
            payload: job.payload.clone(),
            generation: job.generation,
            validator,
            lease_seq,
            settled: false,
        })
    }

    /// Retire every generation below `generation`: drop each job from a
    /// superseded router state, freeing any slot it held, and refuse
    /// later activations from those generations. Returns how many jobs
    /// were cancelled.
    ///
    /// An outstanding operation whose job is dropped here settles as
    /// stale (see [`ProbeLease::settle`]) -- it releases nothing it does
    /// not hold and schedules no follow-up work.
    ///
    /// Also clears terminal tombstones STRICTLY BELOW `generation` -- a marker
    /// stamped at `generation` itself belongs to the incoming incarnation and
    /// survives -- and any tombstone saturation raised below it, so a
    /// republished scheduler may ask again.
    pub fn retire_before(&self, generation: u64) -> usize {
        let mut inner = self.inner.lock();
        if generation > inner.retired_below {
            inner.retired_below = generation;
        }
        // RECLAMATION, not the barrier -- pinned by DIFFERENT tests, which is why
        // the distinction is written down.
        //
        // Not the barrier: `activate` refuses only on a marker whose own
        // incarnation is `>= generation`, so a stale marker left here could not
        // refuse a newer one. Measured -- deleting this line leaves the
        // re-activation tests green.
        //
        // What it buys is bounded memory, and the tests that read the marker set
        // directly are the ones that catch its loss: measured, deleting this line
        // REDS `retirement_clears_tombstone_saturation` and
        // `a_resolved_identity_is_tombstoned_until_the_incarnation_advances`,
        // which both assert `tombstoned == 0` after a publication. Without it the
        // set grows across every reload until it saturates capacity and the
        // scheduler fails closed for a reason unrelated to any lane.
        //
        // Keeps `>= generation`, so a marker stamped at the incoming generation
        // belongs to it and survives.
        inner.tombstones.retain(|(_, inc)| *inc >= generation);
        if inner
            .tombstone_saturated_at
            .is_some_and(|saturated| saturated < generation)
        {
            inner.tombstone_saturated_at = None;
        }
        let before = inner.jobs.len();
        inner.jobs.retain(|job| job.generation >= generation);
        let cancelled = before - inner.jobs.len();
        inner.counters.retired_total += cancelled as u64;
        cancelled
    }

    /// Cancel every tracked job, for shutdown. Returns how many were
    /// cancelled. Outstanding operations settle as stale.
    pub fn cancel_all(&self) -> usize {
        let mut inner = self.inner.lock();
        let cancelled = inner.jobs.len();
        inner.jobs.clear();
        inner.tombstones.clear();
        inner.tombstone_saturated_at = None;
        inner.counters.retired_total += cancelled as u64;
        cancelled
    }

    /// Raise the retirement floor WITHOUT sweeping the table.
    ///
    /// TEST-ONLY. Reproduces the window between raising the floor and
    /// sweeping, which is the state the per-lease generation check exists
    /// for: `retire_before` does both, so nothing else can construct a
    /// tracked-but-retired job to prove that check is load-bearing.
    #[cfg(test)]
    pub fn raise_retirement_floor_without_sweeping_for_tests(&self, generation: u64) {
        self.inner.lock().retired_below = generation;
    }

    /// Record that a paid-probe candidate could not be stored because the
    /// candidate list was at capacity.
    ///
    /// Lives on the SCHEDULER although the list lives on `Router`, because the
    /// scheduler owns the operator-readable counter set: a refusal recorded
    /// anywhere else would need its own reader, and a bounded refusal with no
    /// reader is the silent drop this exists to remove.
    pub fn note_paid_candidate_capacity_refusal(&self) {
        self.inner
            .lock()
            .counters
            .paid_candidate_capacity_refusals_total += 1;
    }

    /// Record that a lane's beta context breached a payload retention bound, so
    /// no job could be activated for it.
    ///
    /// Counted on the SCHEDULER's snapshot alongside every other bounded
    /// refusal: the alternative is a bare `continue` in the activation loop,
    /// which leaves an un-probed lane indistinguishable from one that was never
    /// admitted.
    pub fn note_payload_refusal(&self) {
        self.inner.lock().counters.payload_refusals_total += 1;
    }

    /// Current queue state plus the lifetime counters.
    #[must_use]
    pub fn snapshot(&self) -> ProbeSchedulerSnapshot {
        let inner = self.inner.lock();
        ProbeSchedulerSnapshot {
            queued: inner.count_phase(|phase| matches!(phase, JobPhase::Queued)),
            in_flight: inner.in_flight(),
            backing_off: inner.count_phase(|phase| matches!(phase, JobPhase::BackingOff { .. })),
            tombstoned: inner.tombstones.len(),
            tombstone_saturated: inner.tombstone_saturated_at.is_some(),
            ..inner.counters
        }
    }

    /// Release one lease. `settlement` is `None` for a lease dropped
    /// without settling.
    ///
    /// A release for a job this scheduler no longer tracks -- retired,
    /// cancelled at shutdown, or already settled and re-leased -- is
    /// STALE: it is counted, releases nothing, and schedules nothing, so
    /// no work is ever queued against retired state.
    fn release(
        &self,
        key: &FieldVerdictKey,
        generation: u64,
        lease_seq: u64,
        settlement: Option<ProbeSettlement>,
        now: Instant,
    ) -> ReleaseOutcome {
        let mut inner = self.inner.lock();
        let stale = match inner.position(key) {
            None => true,
            Some(index) => {
                let job = &inner.jobs[index];
                job.generation != generation
                    || job.lease_seq != lease_seq
                    || !matches!(job.phase, JobPhase::InFlight)
            }
        };
        if stale {
            inner.counters.stale_settlements_total += 1;
            return ReleaseOutcome::Stale;
        }
        let index = inner.position(key).expect("checked above");
        let owner = inner.jobs[index].generation;
        match settlement {
            Some(ProbeSettlement::Resolved) => {
                inner.jobs.swap_remove(index);
                inner.counters.resolved_total += 1;
                inner.tombstone(key.clone(), owner);
            }
            Some(ProbeSettlement::TimedOut) => {
                inner.counters.timeouts_total += 1;
                inner.reschedule_or_abandon(index, now);
            }
            Some(ProbeSettlement::Abandoned) => {
                // No question was asked, so NOT a spent free step -- yet terminal
                // for the incarnation, because every cause is fixed within one
                // published Router. See `ProbeSettlement::Abandoned` for why
                // leaving it non-terminal makes reactivation unbounded.
                inner.jobs.swap_remove(index);
                inner.counters.abandoned_probe_requests_total += 1;
                inner.tombstone(key.clone(), owner);
            }
            Some(ProbeSettlement::SpentFreeStep) => {
                // Cursor advance and the spent/remaining decision are ONE
                // critical section -- see `ProbeSettlement::SpentFreeStep` for
                // why a caller must not read the cursor and then act on it.
                inner.jobs[index].plan_cursor += 1;
                if inner.jobs[index].validator().is_none() {
                    inner.jobs.swap_remove(index);
                    inner.counters.free_exhausted_total += 1;
                    inner.tombstone(key.clone(), owner);
                    return ReleaseOutcome::Committed { free_spent: true };
                }
                inner.reschedule_or_abandon(index, now);
                return ReleaseOutcome::Committed { free_spent: false };
            }
            // A gate declined before any dial. Backed off, but NOT charged an
            // attempt: no question was asked, so there is nothing for the
            // attempt budget to bound.
            Some(ProbeSettlement::Deferred) => {
                inner.counters.deferrals_total += 1;
                inner.defer_without_charging_attempt(index, now);
            }
            // A dropped lease is charged an attempt for the same reason a
            // timeout is: the operation produced no answer, and an
            // immediate re-lease of a repeatedly-cancelled job would spin.
            Some(ProbeSettlement::Retryable) | None => {
                inner.reschedule_or_abandon(index, now);
            }
        }
        ReleaseOutcome::Committed { free_spent: false }
    }
}

/// What one [`ProbeScheduler::release`] did.
///
/// `free_spent` is reported by the scheduler rather than decided by the
/// caller because only the settling critical section can know it: it is the
/// answer to "did advancing the cursor run the plan out", and the cursor is
/// shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The settlement did not apply: a retired or cancelled job, or one
    /// already settled and re-leased. Nothing was mutated or scheduled.
    Stale,
    /// The settlement applied.
    Committed {
        /// `true` when this settlement spent the LAST free step, so the paid
        /// class is now a candidate for this identity. Only a
        /// [`ProbeSettlement::SpentFreeStep`] that ran the plan out sets it.
        free_spent: bool,
    },
}

impl ReleaseOutcome {
    /// Whether the settlement applied against current state.
    ///
    /// Test-only: the production worker keys on
    /// [`Self::exhausted_free_plan`], which already implies commitment, so a
    /// separate commitment read has no production caller. The tests need it
    /// to tell "did not commit" from "committed but spent no step" -- two
    /// outcomes the stronger predicate cannot distinguish.
    #[cfg(test)]
    #[must_use]
    pub const fn committed(self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Whether this settlement spent the last free step in the plan.
    #[must_use]
    pub const fn exhausted_free_plan(self) -> bool {
        matches!(self, Self::Committed { free_spent: true })
    }
}

/// RAII lease over one in-flight probe slot.
///
/// [`settle`](Self::settle) is the only way to record an outcome. Dropping
/// the guard unsettled -- an early return, a `?`, a cancelled future, a
/// shutdown -- releases the slot on the same terms, so there is no call
/// site that can forget to.
#[derive(Debug)]
pub struct ProbeLease<'a> {
    scheduler: &'a ProbeScheduler,
    key: FieldVerdictKey,
    payload: ProbePayload,
    generation: u64,
    validator: ProbeValidator,
    lease_seq: u64,
    settled: bool,
}

impl ProbeLease<'_> {
    /// The identity this lease runs a probe for.
    #[must_use]
    pub const fn key(&self) -> &FieldVerdictKey {
        &self.key
    }

    /// The router generation the leased job was activated at. Carried so
    /// every observation a probe produces is stamped with the generation
    /// that selected it, never a freshly sampled one.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The validator class this lease runs.
    #[must_use]
    pub const fn validator(&self) -> ProbeValidator {
        self.validator
    }

    /// The bounded capability payload this job was activated with.
    #[must_use]
    pub const fn payload(&self) -> &ProbePayload {
        &self.payload
    }

    /// Settle the lease at `now`, releasing the slot.
    ///
    /// The returned [`ReleaseOutcome`] carries both facts a caller needs and
    /// neither can derive itself: whether the settlement COMMITTED against
    /// current state, and whether it spent the plan's last free step. A
    /// caller may only publish a consequence of the settlement (a paid
    /// candidate, say) after it commits, or a lease settling into a scheduler
    /// that no longer tracks it would leave a candidate describing retired
    /// router state.
    #[must_use]
    pub fn settle(mut self, settlement: ProbeSettlement, now: Instant) -> ReleaseOutcome {
        self.settled = true;
        self.scheduler.release(
            &self.key,
            self.generation,
            self.lease_seq,
            Some(settlement),
            now,
        )
    }
}

impl Drop for ProbeLease<'_> {
    fn drop(&mut self) {
        if !self.settled {
            // No clock is available on a drop path, so the backoff is
            // measured from now. A drop is a cancellation, not a result:
            // the only thing that matters is that the slot comes back and
            // the job cannot immediately re-lease.
            let _outcome = self.scheduler.release(
                &self.key,
                self.generation,
                self.lease_seq,
                None,
                Instant::now(),
            );
        }
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod queue_tests;

#[cfg(test)]
mod plan_tests;

#[cfg(test)]
mod concurrency_tests;
