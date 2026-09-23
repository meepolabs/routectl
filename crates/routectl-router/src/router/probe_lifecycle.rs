//! Router-side lazy probe lifecycle: the scheduler incarnation a publication
//! advances, and the bounded worker that executes the FREE validators.
//!
//! The ADMITTED-REQUEST activation seam is NOT here -- it lives in
//! `probe_payload_capture.rs`, which owns what a probe carries. This module owns
//! when queued work runs and how it settles. `activate_probe_plan` is the narrow
//! seam between them: capture decides a payload and a plan, this schedules them.
//!
//! Nothing reachable from construction, config parsing, or the reload carry-over
//! reaches either module -- which is what makes "a lane activates only after its
//! first admitted real request" a structural property rather than a convention.
//!
//! # Why a scheduler-owned incarnation
//!
//! The registry generation only moves at a catalog/overlay boundary, so it
//! cannot express "this Router was republished": two successive
//! config-only reloads share one generation, and work queued by the first
//! would survive into the third as if it were live. The scheduler carries
//! its own incarnation, advanced by [`Router::publish_probe_incarnation`]
//! on EVERY successful publication, and that is what retirement compares
//! against.

use super::Router;
use crate::field_verdict::FieldVerdictKey;
#[cfg(test)]
use crate::probe_scheduler::validator_plan;
use crate::probe_scheduler::{
    FreeValidatorOutcome, ProbeActivation, ProbeLease, ProbeSchedulerSnapshot, ProbeSettlement,
    ProbeValidator,
};
use std::time::Instant;

/// Event name of the bounded probe-activation refusal diagnostic.
pub(super) const PROBE_ACTIVATION_REFUSED_EVENT: &str = "probe_activation_refused";

/// A paid-probe candidate: an identity whose FREE validation ran out of
/// steps without settling.
///
/// `pub` in a PRIVATE module and never re-exported, so it stays off the
/// crate's public surface: its fields carry two types that live in private
/// modules, and publishing it would leak both for a consumer that does not
/// exist yet. The visibility keyword is what the `pub(crate)`-in-private-
/// module lint requires; the module boundary is what bounds the surface.
///
/// A candidate is NOT permission to spend. It records that free
/// validation is exhausted for this identity, which is one of the two
/// conditions `paid_probe_permitted` requires;
/// the reservation that would actually authorize a call is not part of
/// this stage, and nothing here dials a paid endpoint.
///
/// It RETAINS the payload the exhausting job carried rather than leaving the
/// paid stage to rebuild one. Two reasons, and the second is the load-bearing
/// one: the payload is already bounded by every ceiling
/// `ProbePayload::new` enforces, so carrying it adds no unbounded state; and
/// the whole point of a probe is to ask about the exact field, value, and beta
/// context the admitted request was about to send, which a payload
/// reconstructed later from config could not reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidProbeCandidate {
    /// The identity whose free plan is exhausted.
    pub key: FieldVerdictKey,
    /// The scheduler incarnation the exhausting work ran under. A
    /// candidate from a retired incarnation is discarded rather than
    /// carried forward.
    pub incarnation: u64,
    /// Always the `PaidCompletion` class: the only class a
    /// candidate can name.
    pub validator: ProbeValidator,
    /// The already-bounded payload the exhausting free job carried, so the
    /// paid body asks the question the admitted request posed.
    pub payload: crate::probe_scheduler::ProbePayload,
}

impl Router {
    /// The scheduler incarnation this Router publishes work under.
    pub(crate) fn probe_incarnation(&self) -> u64 {
        self.probe_incarnation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Ask for a probe of `key` on behalf of an admitted real request.
    ///
    /// Refused outright while the learned-capability kill switch is off:
    /// probe evidence exists to mint and clear learned verdicts, so a
    /// deployment with that machinery off must not spend upstream calls
    /// gathering evidence nothing may act on.
    ///
    /// The queue-full diagnostic is EDGE-TRIGGERED, following the same
    /// bounded warn-once pattern as `volatile_prefix_warned`: a saturated
    /// queue refuses every subsequent request, so one line per refusal
    /// would put a WARN on every request of every lane precisely when the
    /// daemon is busiest. The occurrence counter
    /// (`queue_full_total`) carries the suppressed volume, so the line is
    /// an existence proof and the counter is the rate.
    #[cfg(test)]
    pub(crate) fn activate_probe_lane(
        &self,
        key: &FieldVerdictKey,
        validator: ProbeValidator,
    ) -> ProbeActivation {
        self.activate_probe_plan(
            key,
            vec![validator],
            crate::probe_scheduler::ProbePayload::new(
                "thinking.enabled.display",
                "summarized".to_string(),
                &[],
                &[],
                false,
            )
            .expect("a modeled display token is within the retention bound"),
        )
    }

    /// [`Self::activate_probe_plan`] with a caller-supplied payload, for the
    /// memory-bound test that needs to drive max-length values directly.
    #[cfg(test)]
    pub(crate) fn activate_probe_plan_for_tests(
        &self,
        key: &FieldVerdictKey,
        payload: crate::probe_scheduler::ProbePayload,
    ) -> ProbeActivation {
        self.activate_probe_plan(key, validator_plan(true), payload)
    }

    /// Activate `key` with a whole FREE validator plan. The scheduler
    /// filters the plan to its free steps and walks them in order.
    pub(super) fn activate_probe_plan(
        &self,
        key: &FieldVerdictKey,
        plan: Vec<ProbeValidator>,
        payload: crate::probe_scheduler::ProbePayload,
    ) -> ProbeActivation {
        if !self.config.capability.enabled {
            return ProbeActivation::Retired;
        }
        let outcome = self
            .probe_scheduler
            .activate(key, self.probe_incarnation(), plan, payload);
        if outcome == ProbeActivation::QueueFull {
            self.warn_probe_queue_full_once(outcome);
        }
        outcome
    }

    /// Emit the queue-full WARN at most once per ROUTER INCARNATION, keeping
    /// the counter as the volume signal.
    ///
    /// Per incarnation rather than per process because the latch is a field on
    /// `Router`: a reload publishes a new one with the latch clear, so the
    /// next saturation after a config change is reported again. That is the
    /// intended scope -- saturation under a new configuration is new
    /// information, while a single incarnation saturating repeatedly is the
    /// same fact and belongs in the counter.
    ///
    /// The latch is a plain bool behind the existing per-Router mutex rather
    /// than a keyed set: the diagnostic has exactly one condition, so there is
    /// nothing to key on.
    fn warn_probe_queue_full_once(&self, outcome: ProbeActivation) {
        let mut warned = self.probe_queue_full_warned.lock();
        if *warned {
            return;
        }
        *warned = true;
        drop(warned);
        let snapshot = self.probe_scheduler_snapshot();
        tracing::warn!(
            outcome = outcome.as_str(),
            queued = snapshot.queued,
            in_flight = snapshot.in_flight,
            backing_off = snapshot.backing_off,
            queue_full_total = snapshot.queue_full_total,
            "{PROBE_ACTIVATION_REFUSED_EVENT}",
        );
    }

    /// Run every currently-due probe job to completion, returning how many
    /// validators were executed.
    ///
    /// THE production worker. One pass leases up to the concurrency bound
    /// (the scheduler enforces it; this loop just keeps asking), runs each
    /// leased job's FREE validator under the per-operation timeout, and
    /// settles it. Bounded on every axis by the scheduler's own
    /// constants -- queue depth, concurrency, timeout, backoff, attempt
    /// cap -- so a caller cannot widen any of them by calling more often.
    ///
    /// Never dials a paid endpoint. An identity whose free plan runs out
    /// becomes a paid CANDIDATE, which records that free validation is spent
    /// and is never itself permission to spend.
    pub async fn run_due_probes(&self) -> usize {
        self.run_due_probes_inner(None).await
    }

    /// [`Self::run_due_probes`], recording which validators actually ran.
    /// The recording variant exists so a test can assert the worker's
    /// progression through the free plan by OBSERVED execution rather than
    /// by inferring it from counters.
    #[cfg(test)]
    pub(crate) async fn run_due_probes_recording(&self) -> Vec<ProbeValidator> {
        let mut executed = Vec::new();
        self.run_due_probes_inner(Some(&mut executed)).await;
        executed
    }

    async fn run_due_probes_inner(&self, record: Option<&mut Vec<ProbeValidator>>) -> usize {
        // Lease UP TO THE CONCURRENCY BOUND first, then run those concurrently.
        // `lease_due` returns `None` at the ceiling as well as at "nothing due",
        // so a pass takes at most that many jobs and a due job not taken waits
        // for the next pass.
        //
        // Batching up front is what makes the bound mean what it says. A
        // sequential lease-and-await loop would let one pass run far more
        // operations than the ceiling: each settle frees its slot before the next
        // lease, so the ceiling would cap a simultaneity that never occurs while
        // the pass drained the whole queue one job at a time.
        let now = Instant::now();
        let mut leases = Vec::new();
        while let Some(lease) = self.probe_scheduler.lease_due(now) {
            leases.push(lease);
        }
        if leases.is_empty() {
            return 0;
        }
        let ran = leases.len();
        if let Some(sink) = record {
            sink.extend(leases.iter().map(ProbeLease::validator));
        }
        let executions: Vec<_> = leases
            .into_iter()
            .map(|lease| {
                let validator = lease.validator();
                self.execute_leased_probe(lease, validator)
            })
            .collect();
        futures::future::join_all(executions).await;
        ran
    }

    /// Run one leased job's validator under the per-operation timeout and
    /// settle the lease.
    ///
    /// The timeout WRAPS the operation future, so expiry DROPS that future
    /// rather than merely marking the slot free. That distinction is the
    /// whole point: a slot released while its operation kept running would
    /// let real in-flight work exceed the concurrency bound with no counter
    /// showing it.
    ///
    /// A paid candidate is published ONLY after the settlement commits
    /// against current state, and only when the SCHEDULER reports that the
    /// settlement spent the plan's last free step. A lease settling into a
    /// scheduler that no longer tracks it -- retired mid-flight, or cancelled
    /// at shutdown -- commits nothing, and a candidate recorded from it would
    /// describe router state that has already gone away.
    async fn execute_leased_probe(&self, lease: ProbeLease<'_>, validator: ProbeValidator) {
        let outcome = tokio::time::timeout(
            crate::probe_scheduler::PROBE_OPERATION_TIMEOUT,
            self.run_free_validator(lease.key(), lease.payload(), validator),
        )
        .await;
        let settlement = match outcome {
            // Timed out. `tokio::time::timeout` DROPS the wrapped future on
            // expiry, so the operation is genuinely cancelled rather than
            // abandoned while it keeps running and holds real upstream
            // concurrency. Settled EXPLICITLY rather than through a
            // wall-clock reap: the worker is the only party that knows the
            // cancellation happened, and a reap keyed on a clock this caller
            // may not share would silently count nothing.
            Err(_elapsed) => ProbeSettlement::TimedOut,
            Ok(FreeValidatorOutcome::Settled) => ProbeSettlement::Resolved,
            Ok(FreeValidatorOutcome::Transient) => ProbeSettlement::Retryable,
            // Neither of these spends a free step. A refused probe REQUEST is
            // a defect in what routectl sent (or a class this build does not
            // model) rather than evidence about the capability; a lane that
            // could not run the step never took the question.
            Ok(FreeValidatorOutcome::ProbeRequestRefused | FreeValidatorOutcome::Unavailable) => {
                ProbeSettlement::Abandoned
            }
            // The gate declined before any dial. Charges neither a free step
            // NOR an attempt -- see `ProbeSettlement::Deferred` for why a
            // non-answer must not walk the attempt budget.
            Ok(FreeValidatorOutcome::GateDeferred) => ProbeSettlement::Deferred,
            // A spent step. Whether spending it ADVANCES the plan or runs it
            // out is the scheduler's call, taken inside the same critical
            // section that moves the cursor -- the worker cannot read the
            // cursor and then act on it without a window for a concurrent
            // settlement to move it. `Exhausted` is a plan-level state a
            // single step never returns, so it is folded in rather than given
            // an unreachable arm.
            Ok(FreeValidatorOutcome::Inconclusive | FreeValidatorOutcome::Exhausted) => {
                ProbeSettlement::SpentFreeStep
            }
        };
        tracing::debug!(
            validator = validator.as_str(),
            settlement = ?settlement,
            "probe_validator_settled",
        );
        let key = lease.key().clone();
        let incarnation = lease.generation();
        // Cloned BEFORE the settle consumes the lease, because the payload is
        // what a paid body would be built from and the lease is the only thing
        // that carries it. Cloning is bounded by the same ceilings
        // `ProbePayload::new` enforced at capture.
        let payload = lease.payload().clone();
        let release = lease.settle(settlement, Instant::now());
        // Order is load-bearing: a candidate is published only after a
        // settlement that actually COMMITTED, only when that settlement spent
        // the plan's last free step, and only when the shared predicate agrees
        // (it also reads the provider's UTC-day cap, which defaults to zero).
        if release.exhausted_free_plan()
            && crate::probe_scheduler::paid_probe_permitted(
                FreeValidatorOutcome::Exhausted,
                self.paid_probe_daily_cap(&key),
            )
        {
            self.record_paid_probe_candidate(&key, incarnation, payload);
        }
    }

    /// Execute one FREE validator against `key`'s lane.
    ///
    /// Validates every external result rather than trusting it. Only the
    /// count-token class is executable in this build; the expected-rejection
    /// class is in no plan (see `validator_plan`) and the `PaidCompletion`
    /// arm refuses rather than dialing, so even a future mis-wiring cannot
    /// spend money from here.
    async fn run_free_validator(
        &self,
        key: &FieldVerdictKey,
        payload: &crate::probe_scheduler::ProbePayload,
        validator: ProbeValidator,
    ) -> FreeValidatorOutcome {
        match validator {
            ProbeValidator::CountTokens => self.run_count_tokens_validator(key, payload).await,
            // Neither class may execute here. Reaching `ExpectedRejection`
            // means a plan was built by some path other than `validator_plan`
            // (which excludes it, since it has no grounded template and can
            // perform no operation); `PaidCompletion` must never dial from
            // the worker. Both report a REFUSAL rather than a spent step, so
            // neither can advance a lane toward paid eligibility by being
            // unreachable-but-counted.
            ProbeValidator::ExpectedRejection | ProbeValidator::PaidCompletion => {
                FreeValidatorOutcome::ProbeRequestRefused
            }
        }
    }

    /// [`Self::run_free_validator`], for tests that must drive ONE validator
    /// and read its outcome directly.
    ///
    /// Placed BELOW the function it wraps, not above: a `cfg(test)` item
    /// sitting between a doc comment and its production function silently
    /// steals that doc, so the paid-boundary contract above would document the
    /// wrapper and the real function would ship undocumented.
    ///
    /// The worker folds every outcome into a settlement, so a test asserting
    /// "this pass was a gate DEFERRAL and not some other non-answer" cannot get
    /// that from `run_due_probes`. Calls the production function, so a mutation
    /// to any of its arms is observable here.
    #[cfg(test)]
    pub(crate) async fn run_free_validator_for_tests(
        &self,
        key: &FieldVerdictKey,
        payload: &crate::probe_scheduler::ProbePayload,
        validator: ProbeValidator,
    ) -> FreeValidatorOutcome {
        self.run_free_validator(key, payload, validator).await
    }

    /// The free remote count-token validator: a `count_tokens` call built
    /// from the leased payload, on the leased identity's OWN resolved seat.
    ///
    /// Documented free and separately rate-limited upstream, which is one
    /// more reason it obeys the scheduler's bounds.
    async fn run_count_tokens_validator(
        &self,
        key: &FieldVerdictKey,
        payload: &crate::probe_scheduler::ProbePayload,
    ) -> FreeValidatorOutcome {
        let Some(seat) = self.probe_seat_for(key) else {
            // No resolved seat for this identity: nothing to ask, and asking
            // again resolves nothing.
            return FreeValidatorOutcome::Unavailable;
        };
        // Re-check attributability against the ACTUALLY SELECTED provider
        // entry, immediately before the dial. Activation checked the entry it
        // resolved then; a reload can have replaced that entry with a Mantle,
        // loopback, or forwarded-credential one since, and a probe must never
        // dial a target whose rejection it could not attribute.
        //
        // MIRRORS activation's refusal set, entry-for-entry. A recheck that
        // covered fewer cases than activation would let exactly the reload
        // that changed a lane's attributability slip a queued job through:
        // the job was admitted against the old entry and nothing else looks
        // again.
        if !self.probe_entry_is_attributable(&seat.provider_name) {
            return FreeValidatorOutcome::Unavailable;
        }
        // The SAME per-attempt gate every real dispatch passes, in its PROBE
        // mode: a probe must not bypass an operator rate limit, must not dial
        // into an open breaker, and must not take the breaker's single
        // half-open recovery attempt.
        //
        // Recovery belongs to REAL traffic. That slot is how the breaker asks
        // "is this lane healthy for clients again?", and a background probe
        // cannot answer it: this validator deliberately neither credits nor
        // debits the breaker, so a probe holding the slot produces no recovery
        // signal at all while displacing the next real request that would have
        // produced one. Worse, `count_tokens` is a separately rate-limited
        // upstream endpoint, so its verdict is not even evidence about the
        // inference lane the breaker guards.
        //
        // The deferral is decided INSIDE the state's critical section
        // (`admit_probe_dispatch`), before the half-open claim and before the
        // RPM debit. Deciding it out here instead would already have spent a
        // rate token -- no path refunds it -- so every declined probe would
        // quietly consume a slice of the operator's budget for a request it
        // never sent.
        let (refusal, probe_guard) =
            self.admit_probe_dispatch(&seat.state_key, &seat.provider_name);
        if refusal.is_some() {
            // The guard is inert on a refusal by construction, but drop it
            // explicitly so the release path is the same on every exit.
            drop(probe_guard);
            return FreeValidatorOutcome::GateDeferred;
        }
        // Admitted, which for a probe means the breaker was CLOSED: the probe
        // mode refuses every half-open-ready lane, so this call provably holds
        // no claim. The inert guard is still carried to the end rather than
        // dropped early, so the release path is the guard's `Drop` on every
        // exit -- including a cancelled future, since this whole call runs
        // inside a `tokio::time::timeout`.
        let req = build_probe_request(&seat, payload, None);
        let outcome = match seat.provider.count_tokens(req).await {
            // A count came back for a body carrying the field under test.
            // This stage draws no verdict from it -- minting is the reactive
            // arm's job -- but the question was asked and answered.
            Ok(count) if count.input_tokens > 0 => FreeValidatorOutcome::Settled,
            // A well-formed zero establishes nothing about the field.
            Ok(_) => FreeValidatorOutcome::Inconclusive,
            Err(err) => super::probe_failure_class::classify_probe_failure(&err),
        };
        // NEVER credit or debit the breaker. It governs CLIENT traffic on a
        // paid lane and a background probe is not client traffic: a probe
        // success would close a breaker the operator's own requests have not
        // proven healthy, and a probe 429 or 5xx would open or re-trip a lane
        // serving clients fine (`count_tokens` is separately rate-limited
        // upstream, so its failures are not even evidence about the inference
        // lane). The scheduler's backoff and attempt cap are what bound a
        // failing probe.
        drop(probe_guard);
        outcome
    }

    /// Record that `key`'s free validation is exhausted, making the paid
    /// class a candidate. Bounded by the queue depth, since at most that
    /// many identities can be tracked at once.
    ///
    /// The list is a WORK RECORD, not an observability surface: it names which
    /// identities a paid probe would be warranted for, and it has no reader
    /// outside the tests. `free_exhausted_total` on the scheduler snapshot is
    /// the operator-readable counter for the same event, so the two are not
    /// redundant -- one identifies WHICH lanes, the other counts HOW MANY.
    fn record_paid_probe_candidate(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
        payload: crate::probe_scheduler::ProbePayload,
    ) {
        // THE QUEUE LOCK FIRST, then liveness -- the same order, and for the same
        // reason, as `Router::requeue_paid_probe_candidate`. A worker settling a
        // probe can reach this concurrently with a publication or shutdown; both of
        // those advance the ticket BEFORE taking this lock to clear, so either this
        // recording completes and the clear removes it, or the clear goes first and
        // the read below observes the supersession and drops. Reading liveness
        // before the lock would leave the window where a record lands on a list
        // that was just emptied and nothing removes it.
        let mut candidates = self.paid_probe_candidates.lock();
        // TEST PARK POINT, at the first in-lock position -- the only place that
        // discriminates the two lock orders. See the requeue path's hook for the
        // full reasoning; inert outside tests and in tests that install no hook.
        #[cfg(test)]
        Self::record_park_hook();
        // A settlement that commits against a superseded generation records
        // nothing. The incarnation comparison catches work from a generation this
        // router no longer publishes under; the shared-ticket read catches this
        // ROUTER being superseded or a shutdown having advanced the ticket to its
        // terminal generation -- neither of which rewrites this router's own stamp,
        // so a post-shutdown worker settlement would otherwise still record.
        if incarnation != self.probe_incarnation() || !self.is_current_publication() {
            return;
        }
        if candidates
            .iter()
            .any(|c: &PaidProbeCandidate| &c.key == key && c.incarnation == incarnation)
        {
            return;
        }
        if candidates.len() >= crate::probe_scheduler::PROBE_QUEUE_DEPTH {
            // OBSERVABLE rather than a silent drop. Dropping the record costs
            // no upstream call -- a candidate is not permission to spend -- but
            // it does lose the only signal an exhausted lane produces, so the
            // refusal is counted on the same snapshot every other bounded
            // refusal reports through.
            self.probe_scheduler.note_paid_candidate_capacity_refusal();
            return;
        }
        candidates.push_back(PaidProbeCandidate {
            key: key.clone(),
            incarnation,
            validator: ProbeValidator::PaidCompletion,
            payload,
        });
    }

    /// The park hook the recording lock-order test installs, called once inside
    /// `record_paid_probe_candidate`.
    ///
    /// Thread-local for the same reason the requeue's is: the racing clear runs on
    /// another thread and must NOT be parked, or the test would arrange a deadlock
    /// rather than a race.
    #[cfg(test)]
    fn record_park_hook() {
        RECORD_PARK.with(|hook| {
            if let Some(park) = hook.borrow().as_ref() {
                park();
            }
        });
    }

    /// `record_paid_probe_candidate`, for the lock-order and post-shutdown tests.
    ///
    /// The production path is reached only from a worker settling a probe, which a
    /// test cannot park mid-settlement; this drives the same function directly so a
    /// mutation to its lock order or liveness reads is observable.
    #[cfg(test)]
    pub(crate) fn record_paid_probe_candidate_for_tests(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
        payload: crate::probe_scheduler::ProbePayload,
    ) {
        self.record_paid_probe_candidate(key, incarnation, payload);
    }

    /// Every RECORDED candidate, live incarnation or not.
    ///
    /// Test-only, and it exists because the live-incarnation filter on
    /// [`Self::paid_probe_candidates`] would otherwise MASK a recording bug:
    /// a candidate wrongly recorded from an uncommitted settlement carries a
    /// retired incarnation, so the filtered reader hides it and the guard
    /// under test becomes unobservable. Asserting on the raw list is what
    /// makes "recorded only after a committed settlement" falsifiable.
    #[cfg(test)]
    pub(crate) fn all_recorded_paid_candidates_for_tests(&self) -> Vec<PaidProbeCandidate> {
        self.paid_probe_candidates.lock().iter().cloned().collect()
    }

    /// The paid-probe candidates free validation has exhausted, for the
    /// LIVE incarnation only. A candidate is not permission to spend --
    /// see `PaidProbeCandidate`.
    ///
    /// `cfg(test)` rather than blanket-allowed: no production reader exists,
    /// and the gate is what makes adding one a deliberate edit.
    #[cfg(test)]
    pub(crate) fn paid_probe_candidates(&self) -> Vec<PaidProbeCandidate> {
        let live = self.probe_incarnation();
        self.paid_probe_candidates
            .lock()
            .iter()
            .filter(|c| c.incarnation == live)
            .cloned()
            .collect()
    }

    /// Lease the next due probe job. Test-only: production leasing goes
    /// through [`Self::run_due_probes`], which owns the timeout and the
    /// settlement, so a caller cannot lease a slot and forget either.
    #[cfg(test)]
    pub(crate) fn lease_due_probe(&self, now: Instant) -> Option<ProbeLease<'_>> {
        self.probe_scheduler.lease_due(now)
    }

    /// The scheduler's diagnostic snapshot. PURE: reading it never activates a
    /// lane, leases a slot, or schedules work, so a status or doctor read
    /// cannot cause the traffic it is reporting on.
    ///
    /// `pub` because the reload boundary tests consume it from
    /// `routectl-cli`, across the crate boundary; it is also the seam a
    /// status/doctor render would read. In production the queue-full WARN reads
    /// it, reporting the queue state it refused an activation against.
    ///
    /// The daemon's probe driver does NOT consult it: the driver ticks
    /// unconditionally and `run_due_probes` returns zero when nothing is due,
    /// so there is no snapshot-then-act window for the queue to change inside.
    pub fn probe_scheduler_snapshot(&self) -> ProbeSchedulerSnapshot {
        // The clock is read HERE and passed in, so the snapshot's queue counts and
        // its next-retry duration describe one moment under one lock acquisition.
        // Reading a second clock inside would let the two disagree by however long
        // the lock wait took.
        self.probe_scheduler.snapshot(Instant::now())
    }

    /// Attach this Router to the outgoing Router's probe scheduler and
    /// candidate list, and inherit its incarnation counter.
    ///
    /// ATTACH, not copy, for the same reason the learned registry and the
    /// field-verdict facade attach: a probe leased through the pre-swap
    /// Router settles against the table the published Router leases from,
    /// and a fresh empty table would both lose that settlement and admit a
    /// duplicate job for a lane whose probe is still outstanding. The
    /// incarnation is inherited rather than reset so
    /// [`Self::publish_probe_incarnation`] advances PAST the outgoing
    /// Router's value -- a per-Router counter starting at zero would make
    /// every reload's "new" incarnation equal to the first one's.
    pub(crate) fn carry_over_probe_scheduler_from(&mut self, previous: &Self) {
        self.probe_scheduler = std::sync::Arc::clone(&previous.probe_scheduler);
        self.paid_probe_candidates = std::sync::Arc::clone(&previous.paid_probe_candidates);
        // The TICKET is shared so this Router's publication draws a value
        // past the outgoing one's; the incarnation itself is not, so the
        // outgoing Router keeps publishing under its own until it is
        // retired. Until this Router publishes it inherits the outgoing
        // value, so a request that lands mid-swap activates onto live work
        // rather than being refused by an unpublished incarnation.
        self.probe_incarnation_ticket = std::sync::Arc::clone(&previous.probe_incarnation_ticket);
        // Same sharing rationale as the ticket: the publishing router and the
        // shutting-down router are different objects, so a per-router lifecycle
        // state would serialize nothing between them.
        self.probe_lifecycle_state = std::sync::Arc::clone(&previous.probe_lifecycle_state);
        self.probe_incarnation = std::sync::atomic::AtomicU64::new(previous.probe_incarnation());
    }
}

// Per-thread park hook for the recording lock-order test. Test-only, and
// thread-local by design: the racing clear runs on another thread and must not be
// parked. A line comment rather than a doc comment because rustdoc generates
// nothing for a macro invocation and rejects one attached to it.
#[cfg(test)]
thread_local! {
    static RECORD_PARK: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Install `park` as this thread's recording hook for the duration of `body`.
#[cfg(test)]
pub(super) fn with_record_park<R>(park: impl Fn() + 'static, body: impl FnOnce() -> R) -> R {
    RECORD_PARK.with(|hook| *hook.borrow_mut() = Some(Box::new(park)));
    let out = body();
    RECORD_PARK.with(|hook| *hook.borrow_mut() = None);
    out
}

/// The single short user turn a count-token probe carries.
///
/// `count_tokens` needs a non-empty message set to answer at all, and the
/// probe's subject is the ENVELOPE FIELD rather than the content, so this is
/// the smallest body that makes the call well-formed. A constant so no probe
/// can carry caller text.
const PROBE_MESSAGE_TEXT: &str = "ping";

/// Build a probe body: the seat's UPSTREAM wire model, one minimal user
/// message, the closed-table field under test, and the captured beta context.
///
/// THE one builder for both probe classes -- the free count-token validator and
/// the paid completion -- because the two must ask the SAME question of the same
/// lane. Two builders would let one class drift into a different envelope, and
/// the paid class's whole value is that its rejection is comparable to the free
/// class's.
///
/// The field is what makes this a probe rather than a token count. A body
/// without it is accepted by any healthy lane regardless of the capability, so
/// its absence would make a validator answer `Settled` for a question it never
/// asked. The wire id rather than the nickname because this request goes
/// straight to the provider, past the alias/model resolution that would
/// otherwise translate it.
///
/// The betas are REAPPLIED to both carriers the egress reads, so the probe
/// travels under the same effective `anthropic-beta` header the admitted
/// request was about to send. Without them a beta-gated field would be rejected
/// for the missing flag, and that rejection would be read as evidence about the
/// field.
///
/// `max_tokens` is the one axis the two classes differ on, so it is a
/// PARAMETER rather than a second function. `Some(n)` is the paid class's
/// catalog-derived allowance, and it is set BEFORE `apply_probe_payload`
/// deliberately: that call fills a free-path default wherever `max_tokens` is
/// absent, so a derived allowance applied afterwards would be overwritten and
/// the whole derivation would be inert. `None` is the free class, which has no
/// derivation and takes that default.
pub(super) fn build_probe_request(
    seat: &super::probe_seat::ProbeSeat,
    payload: &crate::probe_scheduler::ProbePayload,
    max_tokens: Option<u32>,
) -> routectl_core::ChatRequest {
    let mut req = routectl_core::ChatRequest {
        model: seat.upstream.clone(),
        messages: vec![routectl_core::Message {
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text(PROBE_MESSAGE_TEXT.to_string()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        anthropic_beta: payload.client_betas().to_vec(),
        max_tokens,
        ..Default::default()
    };
    // Each source back onto ITS OWN carrier. The egress filters
    // `anthropic_beta` through `allowed_betas` and exempts `operator_betas`, so
    // crossing them over would either smuggle a filtered client flag past the
    // allowlist or subject an operator-pinned flag to it -- either way the
    // probe's header would differ from the one under test.
    req.routectl_internal.operator_betas = payload.operator_betas().to_vec();
    // The originating Claude-Code classification, so the egress makes the same
    // `is_non_cc` call (and so applies or suppresses the CC beta floor
    // identically) for a body that carries no session capture of its own.
    req.routectl_internal.originating_claude_code_session =
        Some(payload.originating_claude_code_session());
    // Marks this as routectl's own background traffic. It changes nothing about
    // the bytes sent -- the classification above still drives the beta floor and
    // the cloak transform -- and excludes the probe from the egress's
    // CLIENT-traffic classification census, which exists to report what clients
    // send.
    req.routectl_internal.background_probe = true;
    // LAST, and after `max_tokens` is already set: this fills the free path's
    // default only where the field is absent, so a caller-supplied allowance
    // survives it.
    super::field_repair::apply_probe_payload(&mut req, payload);
    req
}
