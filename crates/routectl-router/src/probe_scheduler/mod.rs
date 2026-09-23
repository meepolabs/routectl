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

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::field_verdict::FieldVerdictKey;

mod bounds;
mod lease;
mod paid_slot;
mod payload;
mod schedule;
mod vocab;

// The module's flat surface: every consumer names `probe_scheduler::X`, so the
// file split inside is an organizing choice rather than a new API shape.
pub use lease::{ProbeLease, ReleaseOutcome};
pub use paid_slot::PaidProbeSlot;

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
pub use vocab::PaidPassSettlement;
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
    /// Paid-probe slots held right now, counted in the SAME `in_flight`
    /// reading the free-lease ceiling and the snapshot read.
    ///
    /// A `usize` under the one scheduler lock rather than an atomic, so
    /// "check the ceiling and take the slot" is one critical section -- two
    /// concurrent acquisitions reading an atomic ceiling could both see room
    /// and both take the last slot.
    paid_slots_held: usize,
    counters: ProbeSchedulerSnapshot,
    /// The most recently settled free-probe outcome, process-wide.
    ///
    /// One slot rather than a per-lane map, for the reason spelled on
    /// `ProbeSchedulerSnapshot::last_settlement`: a per-lane history is a store
    /// with its own bound, eviction rule, and reload carry, and the status
    /// question it would answer is narrower than that.
    last_settlement: Option<ProbeSettlement>,
}

impl SchedulerInner {
    fn position(&self, key: &FieldVerdictKey) -> Option<usize> {
        self.jobs.iter().position(|job| &job.key == key)
    }

    fn count_phase(&self, matcher: impl Fn(JobPhase) -> bool) -> usize {
        self.jobs.iter().filter(|job| matcher(job.phase)).count()
    }

    /// Everything occupying a concurrency slot right now: leased free jobs
    /// PLUS held paid slots.
    ///
    /// ONE reading, deliberately. The free-lease ceiling, the snapshot's
    /// `in_flight`, and the paid acquisition all read this, so a paid slot
    /// displaces a free lease and vice versa -- which is what makes
    /// [`PROBE_MAX_CONCURRENCY`] a bound on real simultaneous background work
    /// rather than on one of its two kinds.
    fn in_flight(&self) -> usize {
        self.count_phase(|phase| matches!(phase, JobPhase::InFlight)) + self.paid_slots_held
    }

    /// Time until the EARLIEST backing-off job becomes leasable, or `None` when
    /// nothing is backing off.
    ///
    /// Derived from the deadlines the jobs already carry -- this adds no state and
    /// arms no timer. `saturating_duration_since` is what makes an ALREADY-elapsed
    /// backoff report zero rather than underflowing: zero means leasable on the
    /// next tick, which is a different answer from `None` (nothing to wait for).
    fn next_retry_in(&self, now: Instant) -> Option<std::time::Duration> {
        self.jobs
            .iter()
            .filter_map(|job| match job.phase {
                JobPhase::BackingOff { due_at } => Some(due_at.saturating_duration_since(now)),
                JobPhase::Queued | JobPhase::InFlight => None,
            })
            .min()
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
        Some(ProbeLease::new(
            self,
            job.key.clone(),
            job.payload.clone(),
            job.generation,
            validator,
            lease_seq,
        ))
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
        // RETIRE THE IDLE ROWS, KEEP THE IN-FLIGHT ONES.
        //
        // A queued or backing-off row holds nothing: dropping it frees its queue
        // slot and cancels work that had not started. An IN-FLIGHT row is
        // different -- a real upstream operation is running right now, and its
        // `ProbeLease` is what will end it. Dropping the row here would make
        // `in_flight()` stop counting an operation that is still consuming
        // upstream concurrency, so the very next `lease_due` or
        // `try_acquire_paid_slot` would admit work on top of it and the real
        // simultaneous load would exceed `PROBE_MAX_CONCURRENCY` with no counter
        // showing it. A reload is exactly when that happens, because retirement
        // and live probe work coincide.
        //
        // The retained row is NOT leasable: `lease_due`'s predicate already
        // refuses any job below `retired_below`, and this raised that floor
        // above it. So the row's only remaining function is to hold its slot
        // until its lease settles or drops, at which point `release` finds it
        // retired and sweeps it (see the stale arm there).
        inner
            .jobs
            .retain(|job| job.generation >= generation || matches!(job.phase, JobPhase::InFlight));
        let cancelled = before - inner.jobs.len();
        inner.counters.retired_total += cancelled as u64;
        cancelled
    }

    /// Cancel every tracked job, for shutdown. Returns how many were
    /// cancelled. Outstanding operations settle as stale.
    ///
    /// Clears IN-FLIGHT rows too, unlike [`Self::retire_before`], and the
    /// asymmetry is deliberate: a retirement is followed by a replacement router
    /// that keeps admitting work, so an uncounted live operation would let that
    /// router over-subscribe. At shutdown nothing will be admitted again, so
    /// there is no later admission for an under-count to mislead -- and the
    /// caller is on the way out and must not wait on an upstream.
    /// TEST-ONLY since the shutdown path began raising the floor atomically:
    /// production takes [`Self::cancel_all_and_retire_to`], and the tests that
    /// drive cancellation MECHANICS (an outstanding lease settling as stale, the
    /// in-flight-row asymmetry against retirement) want the sweep without a
    /// lifecycle generation around it. Gated rather than blanket-allowed, so a
    /// future production caller has to ungate it deliberately -- and would then
    /// have to justify sweeping without moving the floor.
    #[cfg(test)]
    pub fn cancel_all(&self) -> usize {
        self.cancel_all_and_retire_to(0)
    }

    /// Cancel every tracked job AND raise the retirement floor to `generation`,
    /// in ONE critical section.
    ///
    /// THE shutdown entry point, and the atomicity is the whole reason it exists
    /// as one call. Clearing alone leaves the floor where it was, so an activation
    /// racing the shutdown -- admitted traffic on a router still carrying the OLD
    /// generation, which no shutdown restamps -- would be accepted by a scheduler
    /// that had just been swept, re-arming work nothing will ever run. Raising the
    /// floor to the terminal generation inside the same lock refuses every such
    /// activation instead, because no router is or ever will be stamped with it.
    ///
    /// Two separate calls could not close that window from outside: between them
    /// an activation would observe a cleared table and an unraised floor, which is
    /// precisely the admitting state.
    ///
    /// `generation` of `0` leaves the floor untouched, which is what the
    /// test-only `cancel_all` passes -- the tests that drive cancellation
    /// mechanics without a lifecycle around them want the sweep and nothing more.
    /// A plain code span rather than an intra-doc link because that item is
    /// `cfg(test)` and so does not exist in a docs build.
    pub fn cancel_all_and_retire_to(&self, generation: u64) -> usize {
        let mut inner = self.inner.lock();
        let cancelled = inner.jobs.len();
        inner.jobs.clear();
        inner.tombstones.clear();
        inner.tombstone_saturated_at = None;
        if generation > inner.retired_below {
            inner.retired_below = generation;
        }
        inner.counters.retired_total += cancelled as u64;
        cancelled
    }

    /// Take one PAID concurrency slot, or `None` at the ceiling.
    ///
    /// Checked against the SAME [`PROBE_MAX_CONCURRENCY`] reading
    /// [`Self::lease_due`] refuses at, inside one critical section, so a paid
    /// slot and a free lease compete for one pool of background concurrency.
    /// See `paid_slot` for why that is the bound rather than a paid counter of
    /// its own.
    ///
    /// Acquired BEFORE the reservation await, not after: a slot taken after the
    /// commit would leave the awaiting window uncounted, and the ceiling would
    /// bound only the part of a paid call that is cheapest to bound.
    ///
    /// Takes `&Arc<Self>` because the returned guard OUTLIVES this call -- it
    /// travels inside the authorization -- so it owns a refcount rather than
    /// borrowing the scheduler the way a free lease does.
    pub fn try_acquire_paid_slot(self: &Arc<Self>) -> Option<PaidProbeSlot> {
        let mut inner = self.inner.lock();
        if inner.in_flight() >= PROBE_MAX_CONCURRENCY {
            inner.counters.paid_slot_refusals_total += 1;
            return None;
        }
        inner.paid_slots_held += 1;
        drop(inner);
        Some(PaidProbeSlot::new(Arc::clone(self)))
    }

    /// Give one paid slot back. Called ONLY by [`PaidProbeSlot`]'s `Drop`, so
    /// no path can release a slot it does not hold.
    ///
    /// Saturating rather than a bare decrement: an underflow here would wrap to
    /// `usize::MAX` and permanently refuse every later lease and acquisition,
    /// which is a worse failure than the impossible double release it would be
    /// reporting.
    ///
    /// MODULE-PRIVATE, which is the narrowest visibility that compiles: a
    /// private item is visible to its module's DESCENDANTS, and the only caller
    /// is `paid_slot`'s `Drop`. Anything wider would let a sibling module forge
    /// a release for a slot it never took, which reads as free capacity while
    /// the real work is still running.
    fn release_paid_slot(&self) {
        let mut inner = self.inner.lock();
        inner.paid_slots_held = inner.paid_slots_held.saturating_sub(1);
    }

    /// Raise the retirement floor WITHOUT sweeping the table.    ///
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

    /// Record that a paid-probe pass CLAIMED a candidate off the list.
    ///
    /// Called at the claim, not at a settlement. A claim is irreversible --
    /// the candidate is off the list whatever happens next -- so a pass
    /// cancelled at any later point must still report that it took one.
    pub fn note_paid_candidate_claimed(&self) {
        self.inner.lock().counters.paid_candidate_attempts_total += 1;
    }

    /// Record that the ledger ACKNOWLEDGED a committed reservation.
    ///
    /// Called on the acknowledgement itself rather than on whatever the pass
    /// goes on to do, because that is where the unit becomes spent and there is
    /// no refund: a supersession, gate deferral, timeout, or cancellation after
    /// this point changes nothing about the spend. Inferring it from a pass's
    /// final outcome would undercount exactly the irreversible cases.
    pub fn note_paid_reservation_committed(&self) {
        self.inner.lock().counters.paid_reservations_committed_total += 1;
    }

    /// Record that a paid-probe call was DISPATCHED to a provider.
    ///
    /// Called immediately before the call rather than after it answers: once
    /// the request is handed to the provider the upstream may have received it,
    /// so a cancelled or timed-out call has still started.
    pub fn note_paid_provider_call_started(&self) {
        self.inner.lock().counters.paid_provider_calls_started_total += 1;
    }

    /// Record how one paid-probe pass SETTLED.
    ///
    /// Terminal counters only. The irreversible milestones have their own
    /// recorders above, called where they occur, so this adds nothing a
    /// cancellation could skip -- and one exhaustive match still forces a build
    /// that adds a settlement shape to decide what it counts.
    pub fn record_paid_settlement(&self, settlement: PaidPassSettlement) {
        let mut inner = self.inner.lock();
        let counters = &mut inner.counters;
        match settlement {
            PaidPassSettlement::Refused => counters.paid_authorization_refusals_total += 1,
            PaidPassSettlement::GateDeferred => counters.paid_gate_deferrals_total += 1,
            PaidPassSettlement::Completed => counters.paid_completed_total += 1,
            PaidPassSettlement::ProviderFailed => counters.paid_provider_failed_total += 1,
            PaidPassSettlement::TimedOut => counters.paid_timeouts_total += 1,
        }
    }

    /// Current queue state plus the lifetime counters.
    #[must_use]
    pub fn snapshot(&self, now: Instant) -> ProbeSchedulerSnapshot {
        let inner = self.inner.lock();
        ProbeSchedulerSnapshot {
            queued: inner.count_phase(|phase| matches!(phase, JobPhase::Queued)),
            in_flight: inner.in_flight(),
            backing_off: inner.count_phase(|phase| matches!(phase, JobPhase::BackingOff { .. })),
            tombstoned: inner.tombstones.len(),
            tombstone_saturated: inner.tombstone_saturated_at.is_some(),
            last_settlement: inner.last_settlement,
            // Sampled against the SAME clock reading the caller passes, under the
            // one lock this snapshot already holds, so the queue counts above and
            // this deadline describe one consistent moment.
            next_retry_in: inner.next_retry_in(now),
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
    ///
    /// MODULE-PRIVATE, like `release_paid_slot`: the only callers are `lease`'s
    /// `settle` and its `Drop`, both descendants of this module, so a private
    /// item already reaches them. Wider visibility would let a sibling settle a
    /// lease it does not hold, freeing a slot whose operation is still running.
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
        // A RETIRED ROW REACHING ITS SETTLEMENT: swept here, and this is the only
        // place that can sweep it. `retire_before` deliberately KEEPS an
        // in-flight row so its live upstream operation keeps occupying a
        // concurrency slot across a reload; that row's remaining function ends
        // exactly now, when the lease it was holding the slot for settles or
        // drops.
        //
        // Reported STALE rather than settled, which is the same answer a caller
        // would have got had the row been dropped at retirement: the settlement
        // must not reschedule a retired job, must not tombstone into a retired
        // incarnation, and must not report a spent free step that could publish a
        // paid candidate describing router state that no longer serves.
        let retired_below = inner.retired_below;
        if let Some(index) = inner.position(key)
            && inner.jobs[index].generation < retired_below
        {
            inner.jobs.swap_remove(index);
            inner.counters.retired_total += 1;
            inner.counters.stale_settlements_total += 1;
            return ReleaseOutcome::Stale;
        }
        let index = inner.position(key).expect("checked above");
        let owner = inner.jobs[index].generation;
        // Recorded BEFORE the per-outcome arms, so every settled outcome is
        // reported whatever that arm goes on to do -- several of them return
        // early. A dropped lease (`None`) records nothing: it settled no outcome,
        // and overwriting the last real answer with "a future was dropped" would
        // erase the signal an operator reads. Recorded here rather than in the
        // arms because one site cannot disagree with itself about which outcomes
        // count.
        if let Some(settled) = settlement {
            inner.last_settlement = Some(settled);
        }
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

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod queue_tests;

#[cfg(test)]
mod plan_tests;

#[cfg(test)]
mod concurrency_tests;
