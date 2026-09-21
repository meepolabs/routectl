//! Claiming a paid-probe candidate and turning it into an authorization: the
//! atomic claim, the preconditions rechecked against current state, the shared
//! concurrency slot, and the single reservation call.
//!
//! # The claim is atomic, and it happens before any await
//!
//! [`Router::claim_paid_probe_candidate`] removes at most one candidate from
//! the shared list inside the list's own critical section, so two concurrent
//! claimers can never hold the same candidate -- and therefore can never both
//! reserve a unit for one exhausted lane. Nothing between taking the lock and
//! releasing it awaits, so the claim cannot be interleaved by the executor
//! either: a claim spanning an await would let the second claimer read a list
//! the first had not yet mutated.
//!
//! # Every precondition is RECHECKED, none inherited
//!
//! A candidate records what was true when free validation ran out. By the time
//! a paid stage reaches it a reload may have retired the incarnation, removed
//! the seat, made the entry unattributable, lowered the cap to zero, or
//! un-priced the catalog cell. So this stage re-derives all of them rather than
//! trusting the record, and every refusal is CLOSED: no ledger call, no dial.
//!
//! The refusals that CANNOT change without a new publication are also the ones
//! whose candidate is DISCARDED rather than requeued -- see
//! [`PaidProbeRefusal::requeues`].
//!
//! # Publication is never blocked; the AUTHORIZATION abandons itself
//!
//! The one await here is the ledger call, and a publication or shutdown can land
//! inside it. Rather than excluding them -- which would let a slow, saturated, or
//! wedged accounting layer hold a reload or a shutdown open -- this stage checks
//! `Router::is_current_publication` TWICE: immediately before the ledger call, and
//! again on the acknowledgement before any authorization is constructed. The
//! shared monotonic ticket is the authority and only ever increases, so a router
//! that reads itself superseded can never read itself current again, and a single
//! read needs nothing held.
//!
//! Three interleavings, and the protocol answers each without waiting:
//!
//! - PUBLICATION FIRST: the pre-call check fails, so there is no ledger call at
//!   all. Nothing spent.
//! - COMMIT FIRST: both checks pass, the authorization is constructed, and a
//!   publication arriving afterwards does NOT invalidate it -- the unit is spent,
//!   so refusing to use it would pay for nothing.
//! - PUBLICATION INSIDE THE AWAIT: the unit may well be committed, and the
//!   post-acknowledgement check refuses anyway with
//!   [`PaidProbeRefusal::Superseded`]. The reservation stays consumed. That is
//!   deliberate UNDERUSE: the ledger has no release method, and spending one unit
//!   of a day's cap is the cheap side of this trade against dialing on behalf of
//!   router state that no longer serves.
//!
//! # The ledger await is bounded
//!
//! A ledger that never answers must not strand a claimed candidate, a held
//! concurrency slot, and a task forever. The call is wrapped in
//! [`PAID_PROBE_RESERVATION_TIMEOUT`], a code constant, and expiry is TERMINAL
//! UNKNOWN: no authorization, no requeue, no retry, and the slot released. The
//! actor may still commit the unit afterwards -- which is why no call may follow a
//! timeout, and why the outcome is underuse rather than a retry that could pair a
//! second unit with the first.
//!
//! # No outbound call lives here
//!
//! This stage ends at an authorization value. It performs no provider call,
//! builds no body, and renders no status: the value it returns is what the next
//! stage needs to dial, and keeping the dial out means a refusal here cannot
//! half-spend anything.

use super::Router;
use super::paid_probe_ledger::PaidProbeReservation;
use super::paid_probe_profile::PaidProbeProfile;
use super::probe_lifecycle::PaidProbeCandidate;
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{PaidProbeSlot, ProbePayload};

/// How long the one ledger call may take before the attempt is abandoned.
///
/// A CODE CONSTANT, not an operator knob and never read from the environment, for
/// the same reason every scheduler bound is: a parameter a process can widen is an
/// exemption that leaves no diff, and this one bounds how long a claimed
/// candidate and a held concurrency slot can be tied up.
///
/// Sized well above a durable local write and well below anything an operator
/// would call a stall. The reservation is one small append to accounting the
/// daemon already owns, so a healthy answer is milliseconds; this is generous
/// enough that expiry means the accounting layer is genuinely not answering
/// rather than merely busy, and short enough that a wedged actor does not hold a
/// slot for a noticeable fraction of a probe cycle.
pub(super) const PAID_PROBE_RESERVATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Permission to make exactly one paid probe call.
///
/// Its EXISTENCE is the authorization: it cannot be built except by
/// [`Router::authorize_paid_probe`], past every local precondition and a
/// committed ledger reservation, so a holder needs to re-derive nothing.
///
/// NOT `Clone`, and that is the whole design rather than an omission. One
/// reservation commits one unit, so a cloned authorization would be two dials
/// against one committed unit -- and since the ledger has no refund or release
/// method, the overspend would be permanent. It also owns the paid concurrency
/// slot, which a clone would double-release.
///
/// A COMMIT IS FINAL. A later reload, a lowered cap, or an un-priced catalog
/// cell does not invalidate a value already built: the unit is spent, and
/// refusing to use it would pay for nothing. Shutdown or cancellation may DROP
/// it, which releases the slot and never refunds the unit.
///
/// `pub` in a private module and never re-exported, like
/// [`PaidProbeCandidate`]: several fields name private types, and the paid
/// dispatch arm that consumes it lives in this crate.
///
/// `Debug` is hand-written below and prints NO request content -- see that impl.
pub struct PaidProbeAuthorization {
    /// The claimed identity.
    key: FieldVerdictKey,
    /// The scheduler incarnation the claim was rechecked as live against.
    incarnation: u64,
    /// The candidate's retained payload, so the paid body asks the question the
    /// admitted request posed rather than one rebuilt from config.
    payload: ProbePayload,
    /// The EXACT seat the recheck resolved, provider `Arc` included.
    ///
    /// Carried rather than re-resolved by the dial stage: re-resolving would
    /// reopen the ambiguity this stage refused on, and a second resolution could
    /// legitimately answer a different seat than the one the reservation was
    /// committed for.
    seat: super::probe_seat::ProbeSeat,
    /// The catalog-confirmed output profile the body is sized from.
    profile: PaidProbeProfile,
    /// Units committed for this provider-day INCLUDING this one, and the cap
    /// they were checked against, as the ledger counted them.
    ///
    /// The COMMIT PROOF: it is the ledger's own account of what it wrote, so a
    /// dial stage logging the budget state reports what the accounting layer
    /// said rather than a number the router re-derived.
    committed: CommittedUnits,
    /// The shared background-concurrency slot this authorization holds.
    ///
    /// Held rather than released at commit time, because the concurrency the
    /// ceiling bounds is the CALL, which has not happened yet. Released by
    /// `Drop` on every exit -- a completed dial, a cancelled future, a
    /// shutdown -- and releasing it is never a refund: the slot bounds load,
    /// the ledger bounds spend.
    ///
    /// Never read BY NAME, and it must not be: the field's entire contract is
    /// that dropping the authorization drops it. An accessor handing the slot
    /// out would let a caller release it while the call it bounds was still to
    /// come, which is the over-subscription the shared ceiling exists to stop.
    #[allow(dead_code)]
    slot: PaidProbeSlot,
}

/// What the ledger committed, as it counted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedUnits {
    /// Units committed for this provider's accounting day, including this one.
    used: u32,
    /// The cap the reservation was checked against.
    cap: u32,
}

impl CommittedUnits {
    /// Units committed for the provider-day, including this reservation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn used(self) -> u32 {
        self.used
    }

    /// The cap this reservation was checked against.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn cap(self) -> u32 {
        self.cap
    }
}

impl PaidProbeAuthorization {
    /// The authorized identity.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn key(&self) -> &FieldVerdictKey {
        &self.key
    }

    /// The incarnation this authorization was rechecked as live against.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The payload a paid body would carry.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn payload(&self) -> &ProbePayload {
        &self.payload
    }

    /// The exact seat the reservation was committed for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn seat(&self) -> &super::probe_seat::ProbeSeat {
        &self.seat
    }

    /// The catalog-confirmed output profile the body is sized from.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn profile(&self) -> PaidProbeProfile {
        self.profile
    }

    /// The ledger's own account of what it committed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn committed(&self) -> CommittedUnits {
        self.committed
    }
}

/// HAND-WRITTEN, because a derived one would print the request.
///
/// An authorization owns the captured `ProbePayload` -- the probed field VALUE
/// and both retained beta-token sets, which are client-supplied -- plus the seat
/// and its provider object. This value exists on a money-spending path, so it is
/// exactly what a diagnostic reaches for, and one `{:?}` would put a client's
/// captured request context and the lane's provider identity into a log line
/// that asked only whether a call was authorized.
///
/// What is printed is deliberately the ACCOUNTING shape and nothing else: the
/// committed counts, the incarnation, and the sized allowance. Each is a number
/// this stage owns, none is caller text, and together they are what an operator
/// reading a paid-path line actually needs. The identity, the payload, the seat,
/// and the slot are named as elided rather than omitted silently, so a reader
/// can tell the fields exist and were withheld on purpose.
impl std::fmt::Debug for PaidProbeAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaidProbeAuthorization")
            .field("incarnation", &self.incarnation)
            .field("committed_used", &self.committed.used)
            .field("committed_cap", &self.committed.cap)
            .field("max_tokens", &self.profile.max_tokens())
            // Withheld, and named so: the identity and the seat carry operator
            // and account identifiers, the payload carries client-supplied text,
            // and the slot would recurse into the whole job table.
            .field("key", &"<elided>")
            .field("seat", &"<elided>")
            .field("payload", &"<elided>")
            .field("slot", &"<elided>")
            .finish()
    }
}

/// Why no paid call may be made for a claimed candidate. Closed set, and every
/// variant has a crate-internal log token ([`Self::as_str`]).
///
/// The refusals stay DISTINCT rather than collapsing into one "no" because they
/// differ in the one way a caller acts on: whether the condition can change
/// WITHOUT a new router generation. See [`Self::requeues`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaidProbeRefusal {
    /// No candidate was available to claim.
    NoCandidate,
    /// The claimed candidate names an incarnation this Router no longer
    /// publishes under.
    StaleIncarnation,
    /// The identity resolves no seat, or resolves AMBIGUOUSLY -- two accounts a
    /// key names alike. Both are the seat resolver answering `None`.
    NoSeat,
    /// The resolved seat's current `[providers]` entry is one whose rejection
    /// this stage cannot attribute: removed, forwarded-credential, Mantle, or
    /// loopback.
    Unattributable,
    /// The provider's configured UTC-day cap is zero right now, so nothing may
    /// be spent even though the candidate was recorded under a non-zero one.
    CapZero,
    /// The catalog confirms no sizeable output profile for this identity, so no
    /// paid body can be built at all.
    NoProfile,
    /// Background concurrency is at `PROBE_MAX_CONCURRENCY`,
    /// so this stage may not add load right now.
    NoConcurrencySlot,
    /// A publication or shutdown overtook this attempt.
    ///
    /// Raised at EITHER of the two generation checks: before the ledger call (so
    /// nothing was spent) or on its acknowledgement (so a unit may well be
    /// committed and stays consumed). The two are one refusal deliberately --
    /// both mean "this router no longer holds the current publication", which is
    /// terminal for the attempt either way, and a caller has nothing different to
    /// do about them. Which side it fired on is not recoverable from the value,
    /// and nothing needs it: the candidate is discarded in both cases, because a
    /// publication clears the list and a shutdown is not followed by anything.
    Superseded,
    /// The one ledger call did not answer inside
    /// [`PAID_PROBE_RESERVATION_TIMEOUT`].
    ///
    /// TERMINAL UNKNOWN, which is stronger than a refusal: the actor may commit
    /// the unit after this returns, so the attempt cannot be retried without
    /// risking a second unit paired with the first, and no call may follow. The
    /// slot is released and the candidate discarded -- deliberate underuse, for the
    /// same reason a committed-then-superseded unit stays consumed.
    ReservationTimeout,
    /// NO accounting is installed on this Router, so there is nothing to ask.
    ///
    /// Distinct from an installed ledger answering
    /// [`PaidProbeReservation::Unavailable`] even though both mean "no
    /// accounting reachable", because only this one is knowable without a call:
    /// the installation is a consuming boot-time builder, so a Router without
    /// one never gains one.
    NoLedger,
    /// The accounting layer refused the reservation. Carries which refusal, so
    /// the requeue decision reads the ledger's own vocabulary rather than a
    /// flattened one.
    Accounting(PaidProbeReservation),
}

impl PaidProbeRefusal {
    /// Whether the claimed candidate is put BACK on the list.
    ///
    /// The axis is whether the condition can change without a new router
    /// generation, because a generation change clears the candidate list
    /// anyway -- so requeueing for a condition only a publication can fix
    /// would hold a slot until that publication threw it away.
    ///
    /// REQUEUED: the four RECOVERABLE accounting outcomes plus the concurrency
    /// refusal. An exhausted cap turns over at the next UTC day, an overloaded
    /// layer sheds load, a failed write may land next time, malformed state may
    /// be repaired externally, and a busy concurrency pool empties -- each
    /// without anything being republished, so a later pass can legitimately
    /// reach a different answer.
    ///
    /// DISCARDED: stale, no-seat, unattributable, cap-zero, no-profile,
    /// no-ledger, and an installed ledger's `Unavailable`. The first five are
    /// properties of THIS published Router's config or resolved table, both
    /// fixed within an incarnation, so their correction publishes a new
    /// generation (which clears the list) or arrives as new traffic (which
    /// records a fresh candidate).
    ///
    /// THE TWO ABSENCE CASES ARE TERMINAL FOR THE PROCESS, which is why
    /// `Unavailable` sits with them rather than with the recoverable accounting
    /// outcomes it superficially resembles: it means no accounting is reachable
    /// -- no ledger installed, or the accounting layer shutting down. An
    /// installation is a consuming boot-time builder and a shutdown does not
    /// reverse, so neither can become reachable later in this process. A requeue
    /// would put the candidate back to be re-refused on every pass for the
    /// daemon's life, holding a bounded slot a probeable lane could use.
    /// `Overloaded` is the one to contrast it with: that is LOAD, and load
    /// passes.
    ///
    /// `NoCandidate` requeues nothing because it claimed nothing.
    pub(super) const fn requeues(&self) -> bool {
        match self {
            Self::NoConcurrencySlot => true,
            // Enumerated per variant rather than as `Accounting(_)`, so a new
            // reservation outcome cannot inherit a retry decision by default --
            // the wrong direction for anything spend-related.
            Self::Accounting(
                PaidProbeReservation::CapExhausted
                | PaidProbeReservation::Overloaded
                | PaidProbeReservation::WriteFailed
                | PaidProbeReservation::MalformedState,
            ) => true,
            Self::Accounting(
                PaidProbeReservation::Unavailable | PaidProbeReservation::Committed { .. },
            )
            | Self::NoCandidate
            | Self::StaleIncarnation
            | Self::NoSeat
            | Self::Unattributable
            | Self::CapZero
            | Self::NoProfile
            | Self::NoLedger
            // A superseded attempt names router state that no longer serves, and
            // the list it would go back onto has already been cleared by the
            // publication or shutdown that superseded it.
            | Self::Superseded
            // A timeout may yet become a commit, so a requeue could pair a second
            // unit with the first -- and no refund exists to undo either.
            | Self::ReservationTimeout => false,
        }
    }

    /// Closed-set log token. Carries no identity and no count -- a caller logs
    /// those from its own fields.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn as_str(&self) -> &'static str {
        match self {
            Self::NoCandidate => "no_candidate",
            Self::StaleIncarnation => "stale_incarnation",
            Self::NoSeat => "no_seat",
            Self::Unattributable => "unattributable",
            Self::CapZero => "cap_zero",
            Self::NoProfile => "no_profile",
            Self::NoConcurrencySlot => "no_concurrency_slot",
            Self::NoLedger => "no_ledger",
            Self::Superseded => "superseded",
            Self::ReservationTimeout => "reservation_timeout",
            Self::Accounting(reservation) => reservation.as_str(),
        }
    }
}

/// What one authorization attempt established: permission, or a closed refusal.
///
/// A two-variant enum rather than `Result`, because neither arm is an error --
/// a refusal is the feature working as configured far more often than it is a
/// fault, and `Result` would invite a `?` that propagated it as one.
#[derive(Debug)]
pub enum PaidProbeOutcome {
    /// One paid call is authorized. See [`PaidProbeAuthorization`].
    ///
    /// BOXED because the authorization is two orders of magnitude larger than a
    /// refusal (it owns a seat, a payload, and a slot), and this value is
    /// returned by every pass including the overwhelmingly common refusing one.
    /// An unboxed variant would make each of those pay the authorized size.
    ///
    /// Allowance rationale as for [`Router::authorize_paid_probe`]: the arm that
    /// dials on the authorization is a separate change, so nothing outside the
    /// tests reads the payload yet. `cfg_attr` rather than a blanket allow, so
    /// the allowance disappears the moment that caller lands.
    #[cfg_attr(not(test), allow(dead_code))]
    Authorized(Box<PaidProbeAuthorization>),
    /// No paid call may be made, for this reason.
    Refused(PaidProbeRefusal),
}

impl PaidProbeOutcome {
    /// The refusal, or `None` on an authorization. Test-only: production
    /// consumers match both arms.
    #[cfg(test)]
    pub(super) fn refusal(&self) -> Option<PaidProbeRefusal> {
        match self {
            Self::Authorized(_) => None,
            Self::Refused(refusal) => Some(refusal.clone()),
        }
    }
}

impl Router {
    /// Claim at most one paid-probe candidate, removing it from the list.
    ///
    /// ATOMIC, and the whole critical section is synchronous: the lock is taken,
    /// one candidate is popped, and the lock is released with nothing awaited in
    /// between. Two concurrent claimers therefore get two DIFFERENT candidates
    /// or one gets none -- never the same one twice, which for a
    /// never-refunded day counter would be two committed units for one
    /// exhausted lane.
    ///
    /// FIFO: pops the FRONT while [`Self::requeue_paid_probe_candidate`] pushes
    /// the BACK, so a refused candidate goes behind every other waiting one.
    /// That is a fairness requirement rather than a style choice -- a LIFO pair
    /// (pop back, push back) re-claims the candidate it just refused on the very
    /// next pass, so one lane whose accounting keeps refusing (a cap exhausted
    /// until the UTC rollover, an overloaded layer) starves every healthy lane
    /// behind it for as long as the condition lasts.
    fn claim_paid_probe_candidate(&self) -> Option<PaidProbeCandidate> {
        self.paid_probe_candidates.lock().pop_front()
    }

    /// Put a claimed candidate back at the END of the queue, subject to the same
    /// bound, identity, and liveness rules a fresh recording obeys.
    ///
    /// FIFO (see [`Self::claim_paid_probe_candidate`]): pushing the back is what
    /// keeps a repeatedly-refusing lane from starving the queue behind it.
    ///
    /// Bounded and deduplicated, because a requeue is a WRITE to the same
    /// bounded list: a requeue that skipped the depth check could grow the list
    /// past its ceiling one retry at a time, and one that skipped the identity
    /// check could leave two records for one lane, which two claimers would
    /// then both reserve against.
    ///
    /// LIVENESS IS READ UNDER THE QUEUE LOCK, and the two orderings together are
    /// what make this safe. A publication or shutdown advances the shared ticket
    /// BEFORE it acquires this same lock to clear, so the interleavings are:
    ///
    /// - this call takes the lock first: it completes, and the clear that follows
    ///   removes what it pushed;
    /// - the clear takes it first: the ticket has already moved, so the read below
    ///   observes the supersession and this call drops the candidate.
    ///
    /// Reading liveness BEFORE the lock reopens the window between them: the read
    /// passes, the clear runs, and the push then lands on an emptied list where
    /// nothing removes it -- resurrecting a retained payload into a generation that
    /// must not see it.
    ///
    /// A refused requeue is counted on the SAME capacity counter the original
    /// refusal reports through -- it is the same fact (an exhausted lane went
    /// unrecorded) reached by a different route.
    fn requeue_paid_probe_candidate(&self, candidate: PaidProbeCandidate) {
        // THE QUEUE LOCK IS TAKEN FIRST, before either liveness read, and held
        // through the dedupe, the capacity check, and the push. That ordering is
        // the whole of this function's race-freedom, and it is not
        // interchangeable with the obvious one.
        //
        // Checking liveness BEFORE taking the lock leaves a window: the check
        // passes, a publication or shutdown then advances the ticket AND clears
        // the list, and this call afterwards pushes a candidate -- with its
        // retained payload -- onto a list that was just emptied. Nothing later
        // removes it, so it survives into a generation that must not see it.
        //
        // With the lock first the two orders linearize. Publication and shutdown
        // both advance the ticket BEFORE acquiring this same lock to clear, so:
        //
        // - This call wins the lock first: it completes, and the clear that
        //   follows removes what it pushed. Safe.
        // - The clear wins the lock first: the ticket has already moved, so when
        //   this call acquires the lock its liveness read observes the supersession
        //   and it drops the candidate. Safe.
        //
        // There is no interleaving in which a push outlives a clear.
        let mut candidates = self.paid_probe_candidates.lock();
        // TEST PARK POINT, immediately after the function's FIRST step.
        //
        // It sits here rather than anywhere else because this is the only
        // position that discriminates the two lock orders. With the lock taken
        // first (production), parking here means a concurrent clear is BLOCKED --
        // so the requeue completes and is then cleared. With the liveness check
        // first, parking here holds NO lock, so a concurrent clear runs to
        // completion and the requeue afterwards pushes onto an emptied list on the
        // strength of a check it passed before the ticket moved.
        //
        // Inert outside tests, and inert in tests that install no hook.
        #[cfg(test)]
        Self::requeue_park_hook();
        // BOTH liveness reads, and they answer different questions. The
        // incarnation comparison catches a candidate from a generation THIS router
        // no longer publishes under; the shared-ticket predicate catches this
        // ROUTER being superseded or a shutdown having advanced the ticket to its
        // terminal generation -- neither of which changes this router's own stamp.
        // Without the second, an old router could requeue a candidate whose
        // incarnation still matches its own stamp, resurrecting a retained payload
        // after the publication or shutdown that cleared the list.
        if candidate.incarnation != self.probe_incarnation() || !self.is_current_publication() {
            return;
        }
        if candidates
            .iter()
            .any(|c| c.key == candidate.key && c.incarnation == candidate.incarnation)
        {
            return;
        }
        if candidates.len() >= crate::probe_scheduler::PROBE_QUEUE_DEPTH {
            drop(candidates);
            self.probe_scheduler.note_paid_candidate_capacity_refusal();
            return;
        }
        candidates.push_back(candidate);
    }

    /// The park hook the lock-order tests install, called once inside
    /// [`Self::requeue_paid_probe_candidate`].
    ///
    /// A thread-local rather than a parameter or a field: the hook must reach a
    /// function whose production signature takes only a candidate, and it must
    /// affect ONLY the thread the test drives -- a shared hook would park the
    /// concurrent clear as well and prove nothing.
    #[cfg(test)]
    fn requeue_park_hook() {
        REQUEUE_PARK.with(|hook| {
            if let Some(park) = hook.borrow().as_ref() {
                park();
            }
        });
    }

    /// [`Self::requeue_paid_probe_candidate`], for the tests that must drive the
    /// requeue against an ALREADY-FULL list.
    ///
    /// That state is not reachable through `authorize_paid_probe`: the claim
    /// frees a slot, so the requeue always finds room unless a concurrent
    /// recording took it first -- a window no test can hold open. Calls the
    /// production function, so a mutation to its bound or identity check is
    /// observable here.
    #[cfg(test)]
    pub(super) fn requeue_paid_probe_candidate_for_tests(&self, candidate: PaidProbeCandidate) {
        self.requeue_paid_probe_candidate(candidate);
    }

    /// Claim one paid-probe candidate and authorize a single paid call for it,
    /// or refuse.
    ///
    /// THE paid-authorization seam. Order is the contract:
    ///
    /// 1. CLAIM one candidate atomically. Nothing else runs against it.
    /// 2. RECHECK every precondition against current state -- live incarnation,
    ///    exact seat, attributability, non-zero live cap, committed profile.
    ///    Each refusal costs no ledger call, because nothing the ledger could
    ///    say would make a stale, seatless, unattributable, un-capped, or
    ///    un-priced lane dialable.
    /// 3. ACQUIRE the shared concurrency slot, before the await rather than
    ///    after, so the awaiting window is counted too.
    /// 4. CONFIRM this router still holds the current publication, immediately
    ///    before the call, so a publication that already landed costs nothing.
    /// 5. RESERVE exactly one unit, once, under
    ///    [`PAID_PROBE_RESERVATION_TIMEOUT`].
    /// 6. CONFIRM CURRENT AGAIN on the acknowledgement, before constructing
    ///    anything. A publication inside the await means the unit may be
    ///    committed and no call may follow it.
    ///
    /// Nothing here blocks a publication or a shutdown -- see this module's docs
    /// for why the authorization abandons itself instead. Once the value IS
    /// constructed it is final: a later reload or lowered cap does not invalidate
    /// it, because the unit is already spent.
    ///
    /// A refusal at any step returns a closed outcome and requeues the
    /// candidate only when the condition can change without a publication (see
    /// [`PaidProbeRefusal::requeues`]).
    ///
    /// NO PRODUCTION CALLER YET, for the same reason
    /// `Router::reserve_paid_probe_unit` has none: the arm that dials on the
    /// authorization is a separate change, and pinning the refusals before any
    /// spender exists is what keeps that change from having to invent them.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) async fn authorize_paid_probe(&self) -> PaidProbeOutcome {
        let Some(candidate) = self.claim_paid_probe_candidate() else {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::NoCandidate);
        };
        match self.authorize_claimed_candidate(&candidate).await {
            PaidProbeOutcome::Refused(refusal) if refusal.requeues() => {
                self.requeue_paid_probe_candidate(candidate);
                PaidProbeOutcome::Refused(refusal)
            }
            outcome => outcome,
        }
    }

    /// The recheck-and-reserve half, over an ALREADY-CLAIMED candidate.
    ///
    /// Split from the claim so the requeue decision sits at exactly one place:
    /// this function never touches the candidate list, so it cannot requeue by
    /// accident and its caller cannot forget to.
    async fn authorize_claimed_candidate(
        &self,
        candidate: &PaidProbeCandidate,
    ) -> PaidProbeOutcome {
        // LIVE INCARNATION first, because a retired candidate describes router
        // state that no longer serves -- every later check would be asking
        // about the wrong Router.
        if candidate.incarnation != self.probe_incarnation() {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::StaleIncarnation);
        }
        // THE EXACT SEAT, re-resolved now. `probe_seat_for` answers `None` for
        // an identity naming no seat AND for one naming two accounts alike, and
        // both must refuse: a paid call against an ambiguous key would bill an
        // account the identity does not uniquely name.
        let Some(seat) = self.probe_seat_for(&candidate.key) else {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::NoSeat);
        };
        // ATTRIBUTABILITY, through the shared predicate both free stages call,
        // against the entry as it reads NOW. A reload can have replaced the
        // entry the candidate was recorded against with a forwarded, Mantle, or
        // loopback one since -- and a paid rejection this stage cannot
        // attribute is money spent on evidence it may not act on.
        if !self.probe_entry_is_attributable(&seat.provider_name) {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::Unattributable);
        }
        // THE LIVE CAP, not the one the candidate was recorded under. An
        // operator lowering a cap to zero must block the next reservation, and
        // reading zero here is how: the ledger would refuse it too, but asking
        // would be a call made after the answer was already knowable.
        let daily_cap = self.paid_probe_daily_cap(&candidate.key);
        if daily_cap == 0 {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::CapZero);
        }
        // THE COMMITTED PROFILE: whether the catalog confirms a sizeable body
        // at all. Every missing or degenerate catalog fact lands here as a
        // single `None`, each with its own named case in the profile sidecar.
        let Some(profile) = self.paid_probe_profile(&candidate.key) else {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::NoProfile);
        };
        // NO ACCOUNTING INSTALLED, checked BEFORE the trait call rather than read
        // off its answer. Two reasons, and the second is why it is a distinct
        // refusal rather than an `Unavailable` passed through: a Router nobody
        // installed a ledger on will never have one for the rest of the process
        // (the installation is a consuming boot-time builder), so there is
        // nothing to ask and nothing to retry -- the candidate is DISCARDED
        // rather than requeued onto a queue where it could only be re-refused
        // forever.
        if self.paid_probe_ledger().is_none() {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::NoLedger);
        }
        // THE SLOT, before the await. A slot taken after the commit would leave
        // the reservation window uncounted, so the ceiling would bound only the
        // part of a paid call that is cheapest to bound.
        let Some(slot) = self.probe_scheduler.try_acquire_paid_slot() else {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::NoConcurrencySlot);
        };
        // CURRENT PUBLICATION, immediately before the call. A publication or
        // shutdown that has ALREADY landed costs nothing here: the attempt stops
        // with zero ledger calls rather than committing a unit for router state
        // that no longer serves.
        if !self.is_current_publication() {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::Superseded);
        }
        // THE ONE reservation, with the provider identity and cap every local
        // check was taken against -- so the cap this stage gated on and the cap
        // the unit was committed under cannot be two different numbers.
        //
        // BOUNDED, so a wedged accounting layer cannot strand this claimed
        // candidate and its held slot. The timeout DROPS the call's future rather
        // than merely giving up on it -- but dropping it does not un-submit
        // whatever the actor already received, which is exactly why expiry is
        // terminal unknown rather than retryable.
        let reservation = tokio::time::timeout(
            PAID_PROBE_RESERVATION_TIMEOUT,
            self.reserve_paid_probe_unit(&seat.provider_name, daily_cap),
        )
        .await;
        let Ok(reservation) = reservation else {
            // The slot is dropped on this path too. The actor may commit the unit
            // after this returns, so there is nothing to retry toward and nothing
            // may be dialed -- underuse, deliberately, since no refund exists.
            return PaidProbeOutcome::Refused(PaidProbeRefusal::ReservationTimeout);
        };
        let PaidProbeReservation::Committed { used, cap } = reservation else {
            // The slot is dropped here, which releases it. NOT a refund: no
            // unit was committed on this path, and a committed one is never
            // given back.
            return PaidProbeOutcome::Refused(PaidProbeRefusal::Accounting(reservation));
        };
        // CURRENT PUBLICATION AGAIN, on the acknowledgement and BEFORE any
        // authorization exists. A publication or shutdown that landed inside the
        // await means the committed unit belongs to router state that no longer
        // serves, so no call may be made on its strength -- and the unit STAYS
        // CONSUMED, because the ledger has no release method and spending one unit
        // of a day's cap is the cheap side of this trade.
        //
        // Placed before the construction rather than after, so no authorization
        // ever exists for a superseded generation: a value built and then
        // discarded would be one a caller could have already read.
        if !self.is_current_publication() {
            return PaidProbeOutcome::Refused(PaidProbeRefusal::Superseded);
        }
        PaidProbeOutcome::Authorized(Box::new(PaidProbeAuthorization {
            key: candidate.key.clone(),
            incarnation: candidate.incarnation,
            payload: candidate.payload.clone(),
            seat,
            profile,
            committed: CommittedUnits { used, cap },
            slot,
        }))
    }
}

// Per-thread park hook for the requeue lock-order tests.
//
// Test-only, and thread-local by design: the racing clear runs on another
// thread and must NOT be parked, or the test would be arranging a deadlock
// rather than a race. A line comment rather than a doc comment because rustdoc
// generates nothing for a macro invocation and rejects one attached to it.
#[cfg(test)]
thread_local! {
    static REQUEUE_PARK: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Install `park` as this thread's requeue hook for the duration of `body`.
#[cfg(test)]
pub(super) fn with_requeue_park<R>(park: impl Fn() + 'static, body: impl FnOnce() -> R) -> R {
    REQUEUE_PARK.with(|hook| *hook.borrow_mut() = Some(Box::new(park)));
    let out = body();
    REQUEUE_PARK.with(|hook| *hook.borrow_mut() = None);
    out
}

#[cfg(test)]
#[path = "paid_probe_authorize_tests.rs"]
mod paid_probe_authorize_tests;
