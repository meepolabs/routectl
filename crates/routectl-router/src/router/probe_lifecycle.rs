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
use crate::learned_capability::{FieldVerdictPresence, FieldVerdictStamp};
#[cfg(test)]
use crate::probe_scheduler::validator_plan;
use crate::probe_scheduler::{
    FreeValidatorOutcome, ProbeActivation, ProbeLease, ProbeSchedulerSnapshot, ProbeSettlement,
    ProbeValidator,
};
use std::time::Instant;

/// Event name of the bounded probe-activation refusal diagnostic.
pub(super) const PROBE_ACTIVATION_REFUSED_EVENT: &str = "probe_activation_refused";

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

    /// Activate `key` with a whole validator plan, which the scheduler walks in
    /// order.
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
    /// An identity whose plan runs out without settling is tombstoned for the
    /// incarnation by the scheduler and records nothing else.
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
    async fn execute_leased_probe(&self, lease: ProbeLease<'_>, validator: ProbeValidator) {
        // Read BEFORE the dial, so the answer is applied only to the version of
        // the negative the probe actually asked about.
        let asked = self.recheck_presence(lease.key()).stamp();
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
            Ok(FreeValidatorOutcome::Settled) => {
                self.settle_accepted_recheck(lease.key(), asked).await
            }
            Ok(FreeValidatorOutcome::FieldRejected { tier }) => {
                self.settle_reconfirmed_recheck(lease.key(), asked, tier)
            }
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
            // settlement to move it.
            Ok(FreeValidatorOutcome::Inconclusive) => ProbeSettlement::SpentFreeStep,
        };
        tracing::debug!(
            validator = validator.as_str(),
            settlement = ?settlement,
            "probe_validator_settled",
        );
        // The scheduler owns every consequence of the settlement, including the
        // tombstone an exhausted plan leaves, so the worker has nothing to act on.
        let _release = lease.settle(settlement, Instant::now());
    }

    /// Execute one FREE validator against `key`'s lane.
    ///
    /// Validates every external result rather than trusting it.
    async fn run_free_validator(
        &self,
        key: &FieldVerdictKey,
        payload: &crate::probe_scheduler::ProbePayload,
        validator: ProbeValidator,
    ) -> FreeValidatorOutcome {
        match validator {
            ProbeValidator::CountTokens => self.run_count_tokens_validator(key, payload).await,
        }
    }

    /// [`Self::run_free_validator`], for tests that must drive ONE validator
    /// and read its outcome directly.
    ///
    /// Placed BELOW the function it wraps, not above: a `cfg(test)` item
    /// sitting between a doc comment and its production function silently
    /// steals that doc, so the contract above would document the wrapper and
    /// the real function would ship undocumented.
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
        // resolved then; a reload can have replaced that entry with an
        // unclassifiable, loopback, or forwarded-credential one since, and a
        // probe must never dial a target whose rejection it could not
        // attribute.
        //
        // MIRRORS activation's refusal set, entry-for-entry. A recheck that
        // covered fewer cases than activation would let exactly the reload
        // that changed a lane's attributability slip a queued job through:
        // the job was admitted against the old entry and nothing else looks
        // again.
        let Some(lane) = self.probe_entry_lane(&seat.provider_name, &seat.upstream) else {
            return FreeValidatorOutcome::Unavailable;
        };
        // Capture is not lane-scoped, so the entry the seat names now must
        // still be the identity's kind and its view must carry the row: a
        // count on another lane is no evidence about this lane's negative.
        // Before the gate, so a refusal spends no rate token.
        if !super::probe_seat::probe_lane_carries(
            self.field_repair_table(),
            key,
            &lane,
            payload.field_path(),
        ) {
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
        let req = build_probe_request(self.field_repair_table(), &seat, payload);
        let outcome = match seat.provider.count_tokens(req).await {
            // A count came back for a body carrying the field under test: the
            // upstream accepts the field the negative says it refuses.
            Ok(count) if count.input_tokens > 0 => FreeValidatorOutcome::Settled,
            // A well-formed zero establishes nothing about the field.
            Ok(_) => FreeValidatorOutcome::Inconclusive,
            Err(err) => super::probe_failure_class::rechecked_field_rejection(
                self.field_repair_table(),
                &err,
                key.provider_kind(),
                Some(lane.egress),
                payload.field_path(),
            )
            .map_or_else(
                || super::probe_failure_class::classify_probe_failure(&err),
                |tier| FreeValidatorOutcome::FieldRejected { tier },
            ),
        };
        // NEVER credit or debit the breaker. It governs CLIENT traffic on a
        // lane and a background probe is not client traffic: a probe
        // success would close a breaker the operator's own requests have not
        // proven healthy, and a probe 429 or 5xx would open or re-trip a lane
        // serving clients fine (`count_tokens` is separately rate-limited
        // upstream, so its failures are not even evidence about the inference
        // lane). The scheduler's backoff and attempt cap are what bound a
        // failing probe.
        drop(probe_guard);
        outcome
    }

    /// What negative is resident for `key` right now.
    fn recheck_presence(&self, key: &FieldVerdictKey) -> FieldVerdictPresence {
        self.learned_capabilities
            .field_verdict_presence_in_generation(self.registry_generation(), key, Instant::now())
    }

    /// Settle a re-check the upstream ACCEPTED: clear the negative it asked
    /// about, durably first.
    ///
    /// With a negative resident, `Resolved` only once the acknowledgement
    /// reports the `cleared` row durable AND the in-memory removal of that same
    /// version applied; a clear that cannot be made durable leaves the negative
    /// acting and the job retryable, without a tombstone, so the next pass asks
    /// again. A negative replaced while the probe was out gets no clear: the
    /// answer is evidence about the version asked about, not its successor.
    /// With nothing resident (purged while the probe was out) there is nothing
    /// to retract, so the job resolves without a clear; with the key unreadable
    /// (a purge holds its lease and may yet be abandoned) it retries.
    async fn settle_accepted_recheck(
        &self,
        key: &FieldVerdictKey,
        asked: Option<FieldVerdictStamp>,
    ) -> ProbeSettlement {
        let current = match self.recheck_presence(key) {
            FieldVerdictPresence::Resident(current) => current,
            FieldVerdictPresence::Absent => return ProbeSettlement::Resolved,
            FieldVerdictPresence::Unreadable => return ProbeSettlement::Retryable,
        };
        if asked.map(|asked| asked.incarnation) != Some(current.incarnation) {
            tracing::debug!(
                event = "probe_clear_stale",
                capability_key = %key.capability_key(),
                "re-check accepted, but the negative it asked about moved while \
                 the probe was in flight; nothing cleared",
            );
            return ProbeSettlement::Retryable;
        }
        let cleared = super::CapabilityClearedEvent {
            persistence_generation: current.persistence_generation,
            incarnation: current.incarnation,
            state_key: key.registry_key().to_string(),
            capability_key: key.capability_key().to_string(),
            provider_kind: key.provider_kind().to_string(),
        };
        if !self.acknowledge_probe_clear(cleared).await {
            self.probe_scheduler.note_clear_persistence_failure();
            tracing::debug!(
                event = "probe_clear_not_durable",
                capability_key = %key.capability_key(),
                "re-check accepted, but its clear was not acknowledged durable; \
                 the negative stays acting and the probe will retry",
            );
            return ProbeSettlement::Retryable;
        }
        if !self
            .learned_capabilities
            .remove_field_verdict_at_incarnation(
                self.registry_generation(),
                key,
                current.incarnation,
            )
        {
            return ProbeSettlement::Retryable;
        }
        self.field_verdicts().canaries().reset(key);
        tracing::info!(
            event = "probe_clear",
            state_key = %routectl_core::sanitize_for_log(key.registry_key()),
            capability_key = %key.capability_key(),
            "envelope-field verdict cleared: a re-check probe was accepted",
        );
        ProbeSettlement::Resolved
    }

    /// Settle a re-check whose rejection named the field under test: refresh
    /// the negative it asked about. The free step is spent -- the negative
    /// stands, and the plan has nothing further to ask this incarnation.
    fn settle_reconfirmed_recheck(
        &self,
        key: &FieldVerdictKey,
        asked: Option<FieldVerdictStamp>,
        tier: routectl_core::capability::SignalTier,
    ) -> ProbeSettlement {
        let refreshed = asked.is_some_and(|asked| {
            self.learned_capabilities
                .refresh_field_verdict_at_incarnation(
                    self.registry_generation(),
                    key,
                    asked.incarnation,
                    tier,
                    Instant::now(),
                )
        });
        if refreshed {
            ProbeSettlement::SpentFreeStep
        } else {
            ProbeSettlement::Retryable
        }
    }

    /// [`Self::execute_leased_probe`] on a lease the test took itself, so a
    /// test can drive a job through its backoff without waiting it out.
    #[cfg(test)]
    pub(crate) async fn execute_leased_probe_for_tests(&self, lease: ProbeLease<'_>) {
        let validator = lease.validator();
        self.execute_leased_probe(lease, validator).await;
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

    /// Attach this Router to the outgoing Router's probe scheduler and inherit
    /// its incarnation counter.
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
/// `max_tokens` is left unset here and filled with the probe default by
/// `apply_probe_payload`.
pub(super) fn build_probe_request(
    table: super::field_repair::FieldRepairTable,
    seat: &super::probe_seat::ProbeSeat,
    payload: &crate::probe_scheduler::ProbePayload,
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
        ..Default::default()
    };
    // Each source back onto ITS OWN carrier. The egress leads the header with
    // `anthropic_beta` and unions `operator_betas` after the configured betas,
    // so crossing them over would reorder the header -- the probe's header
    // would differ from the one under test.
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
    super::field_repair::apply_probe_payload(table, &mut req, payload);
    req
}
