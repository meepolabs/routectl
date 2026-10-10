//! The closed vocabularies a probe job is described in: which validator runs,
//! what one step answered, how one lease ended, and what an activation did.
//!
//! Every variant is a closed-set token safe to log, and the same words are
//! used by the scheduler, the worker, and the snapshot -- one vocabulary
//! rather than a per-layer translation that could drift.

/// One validator class a probe job can run.
///
/// The only class is the free one: an Anthropic-family `count_tokens` call is
/// documented free (and separately rate limited, which is one more reason it
/// obeys the bounds here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeValidator {
    /// Free remote count-token validator, where the lane supports one.
    CountTokens,
}

impl ProbeValidator {
    /// Closed-set log token for this validator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CountTokens => "count_tokens",
        }
    }
}

/// What free validation established for one identity.
///
/// ONE vocabulary for the per-step result a validator returns, shared by the
/// worker and the scheduler: a second enum saying the same things is how the
/// two come to disagree about whether a step counts as spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeValidatorOutcome {
    /// The question is answered; the job is done.
    Settled,
    /// Did not answer, and re-running the SAME step could. A transient
    /// upstream fault says nothing about the field under test, so the step
    /// is retried under the attempt cap rather than consumed -- spending a
    /// free step on a 503 would run the plan out for a reason unrelated to
    /// what the probe asks.
    Transient,
    /// The step ran and answered as much as it can. The plan advances to
    /// its next free step.
    Inconclusive,
    /// The step could not run on this lane AT ALL: no resolved seat, an
    /// entry this stage cannot attribute a rejection to, or an upstream that
    /// refuses the operation outright.
    ///
    /// NOT a spent step. The lane never took the question, so counting it
    /// would run the free plan out on the strength of a probe that could not
    /// execute -- with `count_tokens` the only validator in this build, one
    /// `Unavailable` is the whole plan.
    ///
    /// TERMINAL for the incarnation, like every other `Abandoned` cause, and
    /// for a reason specific to this one: seat resolution and provider-entry
    /// attributability are both read from state that is FIXED within a
    /// published Router incarnation. A lane with no resolved seat now has no
    /// resolved seat for every later request against this Router, so re-asking
    /// cannot produce a different answer -- it only re-queues and re-refuses,
    /// once per admitted request. Publication is exactly what can change that
    /// state, and publication clears the markers.
    Unavailable,
    /// ROUTECTL'S OWN probe request was refused (malformed, unauthorized,
    /// bad request). NOT a spent step: nothing about the capability was
    /// asked, so the job is abandoned rather than advanced -- and terminal for
    /// the incarnation, since what this build sends to this lane does not
    /// change between two requests under one published Router.
    ProbeRequestRefused,
    /// A runtime gate (breaker, RPM, or the half-open deferral) declined
    /// before any dial. NO QUESTION WAS ASKED, so this spends neither a free
    /// step NOR an attempt: the gate said "not now", not "no". The job backs
    /// off and asks again.
    ///
    /// Distinct from `Transient`, which DID ask and got no usable answer.
    /// Charging an attempt here would let three deferrals abandon and tombstone
    /// an identity over a breaker that simply had not finished recovering.
    GateDeferred,
}

/// The validator plan for a lane.
///
/// `supports_count_tokens` decides whether the count-token validator is in
/// the plan; a lane without it has NO executable step, and `activate` refuses
/// such a plan rather than queueing a job that can only no-op.
#[must_use]
pub fn validator_plan(supports_count_tokens: bool) -> Vec<ProbeValidator> {
    if supports_count_tokens {
        vec![ProbeValidator::CountTokens]
    } else {
        Vec::new()
    }
}

/// The outcome of one activation attempt. Every variant is a closed-set
/// token safe to log ([`Self::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeActivation {
    /// A job was queued for this identity.
    Queued,
    /// This identity already has a tracked job in some phase.
    Deduped,
    /// The queue is at [`super::PROBE_QUEUE_DEPTH`]; nothing was queued.
    QueueFull,
    /// The activating router generation is retired; nothing was queued.
    Retired,
    /// The plan carried no validator, so there is nothing this scheduler may
    /// run: a lane without `count_tokens` lands here rather than queueing a
    /// job that could only no-op.
    ///
    /// UNREACHABLE IN THIS BUILD, and deliberately kept anyway. Every
    /// production activation passes `validator_plan(true)`, which is always
    /// `[CountTokens]`, so no production plan is empty. It
    /// carries no snapshot counter for exactly that reason: a counter that can
    /// only ever read zero reports nothing, and its zero would be mistaken for
    /// evidence that the case was checked. The variant stays because the arm
    /// is what refuses an empty plan rather than queueing a job that could
    /// only no-op -- the change that gives a lane a plan without
    /// `count_tokens` makes it reachable, and that change adds the counter.
    NoFreeValidator,
    /// A terminal tombstone for this identity refuses re-activation until
    /// the router incarnation advances.
    Tombstoned,
}

impl ProbeActivation {
    /// Closed-set snake_case token, safe in logs and status output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Deduped => "deduped",
            Self::QueueFull => "queue_full",
            Self::Retired => "retired",
            Self::NoFreeValidator => "no_free_validator",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// How one leased probe ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSettlement {
    /// The probe answered; the job is done.
    Resolved,
    /// The probe did not answer and the condition may pass; the job backs
    /// off, bounded by [`super::PROBE_MAX_ATTEMPTS`].
    Retryable,
    /// A gate declined before any dial, so NO QUESTION WAS ASKED. The job backs
    /// off WITHOUT its attempt budget being charged.
    ///
    /// Separate from `Retryable` because the attempt cap bounds repeated
    /// answers, not repeated non-answers: at three attempts, charging a
    /// deferral would abandon and tombstone an identity after three declines
    /// from a breaker that had simply not finished recovering, taking the lane
    /// out for the whole incarnation. Every other bound still applies -- the
    /// job keeps the one queue slot it already held, releases its concurrency
    /// slot, and cannot re-lease until its backoff elapses -- and retirement
    /// and shutdown remain authoritative over it.
    ///
    /// OCCUPANCY is bounded separately by [`super::PROBE_MAX_DEFERRALS`]: at
    /// that ceiling the job is evicted and its queue slot released, without a
    /// tombstone, so an unanswerable lane cannot hold a slot for a whole
    /// incarnation.
    ///
    /// That bounds occupancy PER ACTIVATION and returns capacity; it is not a
    /// priority guarantee. Traffic that keeps reactivating an unprobeable lane
    /// can keep re-acquiring a slot, so a healthy lane competes for capacity
    /// rather than being assured of it. What the ceiling rules out is the
    /// unbounded case: a single activation occupying a slot forever.
    Deferred,
    /// This free validator did not settle the question. The SCHEDULER then
    /// decides, inside the settling critical section, whether the plan has
    /// another step (advance and back off) or is spent (leave the scheduler
    /// and tombstone the identity for the incarnation).
    ///
    /// That decision is deliberately NOT the caller's. It reads the job's
    /// `plan_cursor`, and a caller that read the cursor through a separate
    /// call would be deciding from a value the job can move between the two
    /// locks: a concurrent timeout settlement and re-lease shifts the cursor,
    /// so the caller's "there is another step" can describe a plan position
    /// that no longer exists. Deciding here makes the read and the write one
    /// atomic step. [`super::ProbeLease::settle`] reports which way it went.
    ///
    /// Separate from `Retryable` because the two mean different things to the
    /// plan: a retry re-runs the SAME validator against a condition that may
    /// pass, while this one moves on because the validator answered as much
    /// as it can. Collapsing them would let a lane spend its whole attempt
    /// budget on step one and be abandoned having never run step two.
    SpentFreeStep,
    /// The operation did not finish inside the per-operation timeout and
    /// its future was DROPPED. Counted as a timeout and rescheduled like a
    /// retry.
    ///
    /// Reported by the worker rather than inferred from a wall-clock
    /// sweep: the worker is the only party that knows the future was
    /// actually cancelled, and a sweep keyed on a clock the caller does
    /// not share silently counts nothing.
    TimedOut,
    /// ROUTECTL'S OWN probe request was malformed, unauthorized, or
    /// otherwise refused as a bad request. The job is abandoned: no free
    /// step is spent.
    ///
    /// A 400 or a 401 on a body routectl built is a defect in the PROBE, not
    /// evidence about the capability under test -- counting it as a spent
    /// free step would run a lane's plan out on the strength of requests that
    /// never asked the question.
    ///
    /// It IS terminal for the incarnation. A probe now travels under the
    /// admitted request's own beta context and Claude-Code classification, so a
    /// refusal of routectl's body is no longer plausibly a header artifact a
    /// later identical request would avoid -- it is a persistent property of
    /// what this build sends to this lane. Without the marker, reactivation is
    /// unbounded: every admitted request re-queues the identity and each one
    /// spends a dial and an RPM token to be refused again. The marker lifts at
    /// the next incarnation, so a reload may ask again.
    Abandoned,
}

impl ProbeSettlement {
    /// Stable, closed-set token for the operator-facing status line.
    ///
    /// Owned here rather than derived, so a new settlement variant is a compile
    /// error on this surface instead of a silent change to what a status reader
    /// sees -- the same discipline the status health panel applies to the breaker
    /// phases.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Retryable => "retryable",
            Self::Deferred => "deferred",
            Self::SpentFreeStep => "spent_free_step",
            Self::TimedOut => "timed_out",
            Self::Abandoned => "abandoned",
        }
    }
}

/// Operator-facing scheduler diagnostics. Counters only -- no identity,
/// no upstream text.
///
/// `#[non_exhaustive]` because this set exists to GROW, and two separate facts
/// make the attribute the right tool. Without it, adding a `pub` field breaks a
/// foreign struct literal and a foreign exhaustive pattern -- but NOT a
/// functional update, which absorbs the new field from its base. With it, the
/// attribute is stricter than that: it rejects ANY foreign struct expression,
/// literal or functional update alike (`E0639`), and forces a `..` rest pattern
/// in foreign patterns. So a new counter is additive for every foreign use that
/// remains legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ProbeSchedulerSnapshot {
    /// Jobs waiting to be leased right now.
    pub queued: usize,
    /// Leased jobs occupying a concurrency slot right now.
    pub in_flight: usize,
    /// Jobs waiting out a backoff.
    pub backing_off: usize,
    /// Lifetime jobs queued.
    pub activations_total: u64,
    /// Lifetime activations refused because the identity was already
    /// tracked.
    pub deduped_total: u64,
    /// Lifetime activations refused because the queue was full.
    pub queue_full_total: u64,
    /// Lifetime leases settled as timed out by their worker.
    pub timeouts_total: u64,
    /// Lifetime jobs cancelled by retirement or shutdown.
    pub retired_total: u64,
    /// Lifetime settlements arriving for a job no longer tracked.
    pub stale_settlements_total: u64,
    /// Lifetime jobs that answered.
    pub resolved_total: u64,
    /// Lifetime jobs abandoned at the attempt cap.
    pub abandoned_total: u64,
    /// Lifetime jobs whose free plan ran out of steps without settling. Each
    /// one tombstoned its identity for the incarnation.
    pub free_exhausted_total: u64,
    /// Lifetime leases settled as deferred by a gate, which charge no attempt.
    pub deferrals_total: u64,
    /// Lifetime jobs EVICTED at the deferral ceiling. Not tombstoned: the
    /// identity was never answered, so later real traffic may reactivate it.
    /// Non-zero means some lane was unprobeable long enough to give its queue
    /// slot back.
    pub deferral_evictions_total: u64,
    /// Lifetime jobs abandoned without a question having been asked: the
    /// probe request routectl itself built was refused (malformed / auth /
    /// bad request), the failure class is one this build has not audited, or
    /// the step could not run on the lane at all (no resolved seat, an
    /// unattributable provider entry). No free step spent.
    pub abandoned_probe_requests_total: u64,
    /// Identities holding a terminal tombstone for the live incarnation.
    pub tombstoned: usize,
    /// Lifetime activations refused by a terminal tombstone, including those
    /// refused because terminal-marker capacity was saturated.
    pub tombstone_refusals_total: u64,
    /// Lifetime terminal markers that could not be stored because capacity
    /// was exhausted. Non-zero means the scheduler is failing CLOSED for the
    /// affected incarnation -- see `tombstone_saturated`.
    pub tombstone_saturations_total: u64,
    /// Whether terminal-marker capacity is currently exhausted, so every
    /// activation at or below the saturating incarnation is refused.
    pub tombstone_saturated: bool,
    /// Times a lane could not be activated because its beta context breached a
    /// payload retention bound or validity rule.
    ///
    /// The refusal is CORRECT -- a probe under a reduced or rewritten beta
    /// context asks a different question than the admitted request posed -- but
    /// it means that lane is not being probed, and without a counter that is
    /// invisible: there is no queued job, no settlement, and no tombstone to
    /// read. An operator seeing this non-zero alongside a lane that never
    /// produces evidence has the explanation.
    pub payload_refusals_total: u64,
    /// The most recently settled FREE probe outcome, process-wide, or `None`
    /// before any free probe has settled.
    ///
    /// AGGREGATE, not per-lane, and the distinction is the design rather than a
    /// simplification. A per-lane outcome history would be a store: it grows with
    /// the number of identities a deployment has probed, needs its own bound and
    /// eviction rule, and has to be carried across reload alongside the job
    /// table. What an operator needs from a status line is narrower -- "is
    /// anything answering, and what did the last answer say" -- and one aggregate
    /// value answers it from state the scheduler already holds.
    ///
    /// Read the limitation with it: on a multi-lane deployment this names the last
    /// settlement of whichever lane settled last, so it is an existence-and-kind
    /// signal rather than an attribution. The per-kind lifetime counters above are
    /// what carry the distribution.
    pub last_settlement: Option<ProbeSettlement>,
    /// Time until the EARLIEST backing-off job may be leased again, or `None`
    /// when nothing is backing off.
    ///
    /// Derived from the existing per-job backoff deadlines -- no new state, no
    /// timer, no scheduled work. A DURATION rather than an instant, because the
    /// caller renders it into a log line and a monotonic `Instant` has no
    /// meaningful rendering; and the EARLIEST rather than a per-job list for the
    /// same reason the settlement above is aggregate.
    ///
    /// Bounded by construction: every deadline comes from `backoff_for_attempt`,
    /// which is itself capped, so this can never report an unbounded wait. Zero
    /// means a job's backoff has already elapsed and it is leasable on the next
    /// tick -- distinct from `None`, which means there is nothing to wait for.
    pub next_retry_in: Option<std::time::Duration>,
}
