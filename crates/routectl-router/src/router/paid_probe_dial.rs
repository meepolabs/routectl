//! Dialing exactly ONE paid completion on a committed authorization: the
//! pre-dial gate, the minimal canonical body, the single direct provider call,
//! and the closed outcome the next stage reads.
//!
//! # One committed unit, one call, and the order is the contract
//!
//! [`Router::run_paid_probe`] is the only route from a candidate to an outbound
//! paid call, and it runs the two halves in one place: authorize, then dial. The
//! order is not an implementation detail -- a call dispatched before its unit is
//! committed is spend the accounting layer never authorized, and the ledger has
//! no refund method to undo it. Keeping both halves behind one function is what
//! makes the ordering structural rather than a convention a second call site
//! could get wrong.
//!
//! The authorization itself carries the rest of the guarantee. It is consumed BY
//! VALUE and is not `Clone`, so one committed unit cannot become two dials; and
//! no function here accepts a raw seat, payload, or profile, so there is no way
//! to assemble a paid call out of parts without the permission that paid for it.
//!
//! # The gate is asked AFTER the commit, and its refusal spends the unit
//!
//! A paid probe passes the same per-attempt gate every dispatch does, in its
//! PROBE mode: it must not bypass an operator rate limit, must not dial into an
//! open breaker, and must not take the breaker's single half-open recovery
//! attempt. That attempt is how the breaker asks whether the lane is healthy for
//! CLIENTS, and this call cannot answer it -- it neither credits nor debits the
//! breaker, exactly as the free validator does not, so a probe holding the slot
//! would produce no recovery signal while displacing the real request that would.
//!
//! Asking it after the commit means a refusal CONSUMES the unit. That is the
//! deliberate side of the trade: the commit is what authorizes, so it has to come
//! first, and the alternative -- gating before reserving -- would spend a rate
//! token or read a breaker phase that could change before the unit landed. A
//! deferred call is therefore never requeued and never retried; the unit is spent,
//! and a retry could pair a second unit with the first.
//!
//! # Nothing is settled, nothing is repaired, nothing falls back
//!
//! The call goes straight to the authorization's own seat -- `seat.provider`, the
//! `Arc` the reservation was committed for -- with no alias resolution, no
//! fallback target, no retry, and no provider loop. Every one of those would spend
//! a second call against one committed unit. The outcome is returned as a closed
//! value for the driver stage to act on; this module mints no verdict, writes no
//! usage row, and renders no status.
//!
//! # A failure is CLASSIFIED before the error is dropped
//!
//! The one thing a settlement stage cannot recover without spending a second unit
//! is WHY the call failed, so the classification is taken here, while the error is
//! still in hand, and travels on as the closed [`PaidProbeFailure`]. The raw
//! `Error` goes no further: no upstream status, message, or body outlives this
//! frame, which is what keeps a failure diagnostic on a money-spending path from
//! becoming the place an upstream's own text gets logged.
//!
//! The classification comes from `probe_failure_class`, which routes through the
//! SAME `routectl_core::failure_class::classify` call the free validator uses,
//! with the same provider kind. One upstream rejection therefore cannot mean two
//! things on the two probe paths, and no second response parser exists to drift
//! from the first. What the paid vocabulary adds over the free one is the
//! distinction the paid class exists to draw: a capability/feature rejection is
//! evidence about the FIELD under test, while auth, throttling, transient faults,
//! and a malformed request are evidence about the credential, the load, the
//! upstream, or routectl's own body. Anything unaudited fails closed as
//! unclassified rather than inheriting the capability reading.

use super::Router;
use super::paid_probe_authorize::{PaidProbeAuthorization, PaidProbeOutcome, PaidProbeRefusal};
use super::probe_failure_class::PaidProbeFailure;

/// What the one paid call established. Closed set, and every variant carries a
/// crate-internal log token ([`Self::as_str`]).
///
/// Deliberately NOT a `Result`: a gate deferral and an expiry are neither
/// successes nor errors, and a `Result` would invite a `?` that propagated them
/// as faults. Each variant is a fact the driver stage acts on differently, and
/// none of them authorizes a second call -- the unit is spent in all four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidProbeDialOutcome {
    /// The upstream answered. This stage draws no verdict from the answer --
    /// minting is not its job -- but the question was asked and answered.
    Completed,
    /// The upstream refused or failed, CARRYING WHICH KIND of refusal. An
    /// ANSWER for this stage rather than a reason to try again: the committed
    /// unit is spent, and a retry would pair a second unit with the first.
    ///
    /// The classification rides along rather than being dropped because the
    /// settlement stage acts on it, and the only other way to recover it would
    /// be to replay the paid call -- a second never-refundable unit for a fact
    /// the first call already established. It is the CLOSED
    /// [`PaidProbeFailure`], derived from the shared classifier, so no upstream
    /// text, status, or body is retained.
    ProviderFailed(PaidProbeFailure),
    /// The per-attempt gate declined before any call. The unit is consumed -- see
    /// this module's docs for why the gate is asked after the commit.
    GateDeferred,
    /// The call did not answer inside
    /// [`crate::probe_scheduler::PROBE_OPERATION_TIMEOUT`].
    ///
    /// TERMINAL UNKNOWN, like a reservation expiry: the upstream may have
    /// received the request, so nothing may follow it.
    TimedOut,
}

impl PaidProbeDialOutcome {
    /// Closed-set log token. Carries no identity, count, or body.
    ///
    /// A provider failure renders as its own token PLUS the classification's,
    /// joined by the failure's closed vocabulary rather than by interpolating
    /// upstream text -- so an operator filtering on `provider_failed` still
    /// matches every failure while a settlement reads the specific class off
    /// [`Self::failure`].
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ProviderFailed(_) => "provider_failed",
            Self::GateDeferred => "gate_deferred",
            Self::TimedOut => "timed_out",
        }
    }

    /// The failure classification, or `None` on an outcome that is not a
    /// provider failure. Logged as a closed token beside [`Self::as_str`].
    pub(super) const fn failure(self) -> Option<PaidProbeFailure> {
        match self {
            Self::ProviderFailed(failure) => Some(failure),
            Self::Completed | Self::GateDeferred | Self::TimedOut => None,
        }
    }
}

/// What one whole paid pass did: dialed with this outcome, or refused before any
/// authorization existed.
///
/// Two variants rather than an `Option` pair, because the two halves fail for
/// different reasons and a caller acts on both: a refusal is the authorization
/// stage's closed vocabulary (and may have cost nothing at all), while a dial
/// outcome always names a spent unit.
#[derive(Debug)]
pub enum PaidProbePass {
    /// A unit was committed and exactly one call was made, or the gate declined
    /// it. Either way the unit is spent.
    ///
    /// Allowance rationale as for [`Router::run_paid_probe`]: the driver pass
    /// that reads the outcome is a separate change, so nothing outside the tests
    /// inspects the payload yet. `cfg_attr` rather than a blanket allow, so the
    /// allowance disappears the moment that caller lands.
    #[cfg_attr(not(test), allow(dead_code))]
    Dialed(PaidProbeDialOutcome),
    /// No authorization was granted, for this reason. Nothing was dialed.
    #[cfg_attr(not(test), allow(dead_code))]
    Refused(PaidProbeRefusal),
}

impl Router {
    /// Claim, authorize, and dial exactly one paid probe.
    ///
    /// THE paid-pass seam, and the ONE place the two halves are sequenced:
    /// `authorize_paid_probe` first, the dial second, with nothing between them
    /// that could reorder. Moving the dial ahead of the authorization would put an
    /// outbound paid call in front of the reservation that pays for it.
    ///
    /// NO PRODUCTION CALLER YET, for the same reason `authorize_paid_probe` had
    /// none before this landed: the driver pass that ticks it is a separate
    /// change, and pinning the call's shape before any driver exists is what
    /// keeps that change from having to invent it. `cfg_attr` rather than a
    /// blanket allow, so the allowance disappears the moment that caller lands.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) async fn run_paid_probe(&self) -> PaidProbePass {
        match self.authorize_paid_probe().await {
            PaidProbeOutcome::Refused(refusal) => PaidProbePass::Refused(refusal),
            PaidProbeOutcome::Authorized(authorization) => {
                PaidProbePass::Dialed(self.dial_paid_probe(*authorization).await)
            }
        }
    }

    /// Make the one call this authorization paid for.
    ///
    /// Takes the authorization BY VALUE and names no seat, payload, or profile in
    /// its signature: the permission and the parts are inseparable, so there is no
    /// way to reach this code with parts assembled from somewhere else. Holding
    /// the value also holds the paid concurrency slot through the call -- the
    /// concurrency the ceiling bounds IS the call -- and dropping it on every
    /// exit, including a cancelled future, is what releases it.
    ///
    /// Never credits or debits the breaker, on any path. See this module's docs.
    async fn dial_paid_probe(&self, authorization: PaidProbeAuthorization) -> PaidProbeDialOutcome {
        // THE GATE, in probe mode, on the authorization's own state key and
        // provider -- the exact seat the unit was committed for, so the lane
        // gated on and the lane billed cannot be two different ones. Decided
        // inside `ProviderState`'s critical section, before the half-open claim
        // and before the RPM debit.
        let (refusal, probe_guard) = self.admit_probe_dispatch(
            &authorization.seat().state_key,
            &authorization.seat().provider_name,
        );
        if refusal.is_some() {
            // The guard is inert on a refusal by construction, but dropped
            // explicitly so the release path is the same on every exit. The
            // authorization is dropped with this frame, which releases the paid
            // slot and never refunds the unit -- there is nothing to refund
            // through.
            drop(probe_guard);
            return self.settled(&authorization, PaidProbeDialOutcome::GateDeferred);
        }
        // Admitted, which for a probe means the breaker was CLOSED: the probe mode
        // refuses every half-open-ready lane, so this call provably holds no
        // claim. The inert guard is still carried to the end rather than dropped
        // early, so the release path is its `Drop` on every exit -- including a
        // cancelled future, since the call below runs inside a timeout.
        let request = super::probe_lifecycle::build_probe_request(
            authorization.seat(),
            authorization.payload(),
            Some(authorization.profile().max_tokens()),
        );
        // THE ONE CALL, directly on the seat's own provider handle. No alias
        // resolution, no fallback target, no retry, no provider loop: each would
        // be a second call against one committed unit.
        //
        // BOUNDED by the same operation timeout the free worker wraps its
        // validator in, so a paid call cannot outlive a free one by carrying its
        // own bound. Expiry DROPS the call future rather than merely giving up on
        // it, which is why it is terminal unknown rather than retryable.
        let answered = tokio::time::timeout(
            crate::probe_scheduler::PROBE_OPERATION_TIMEOUT,
            authorization.seat().provider.complete(request),
        )
        .await;
        let outcome = match answered {
            Err(_elapsed) => PaidProbeDialOutcome::TimedOut,
            Ok(Ok(_response)) => PaidProbeDialOutcome::Completed,
            // Classified HERE, where the error is still in hand, through the
            // SHARED classifier the free path calls -- so one upstream rejection
            // cannot mean two things, and no second response parser exists to
            // drift. The `Error` itself is dropped with this arm: only the closed
            // class travels onward, so no upstream text can reach a settlement or
            // a log line.
            Ok(Err(err)) => PaidProbeDialOutcome::ProviderFailed(
                super::probe_failure_class::classify_paid_probe_failure(&err),
            ),
        };
        // NEVER credit or debit the breaker, exactly as the free validator does
        // not: it governs CLIENT traffic on a paid lane, and a background probe is
        // not client traffic. A probe success would close a breaker the operator's
        // own requests have not proven healthy, and a probe failure would open or
        // re-trip a lane serving clients fine. The scheduler's own bounds are what
        // limit a failing probe.
        drop(probe_guard);
        self.settled(&authorization, outcome)
    }

    /// Record the pass and hand the outcome back.
    ///
    /// DEBUG level: routine background bookkeeping, not an operator-actionable
    /// signal. What is logged is the ACCOUNTING shape and the closed outcome token
    /// and nothing else -- the same withholding the authorization's own `Debug`
    /// performs, and for the same reason. The identity, the seat, the captured
    /// payload, both beta sets, the request body, and the response are all absent:
    /// this is a money-spending path, so it is exactly what a diagnostic reaches
    /// for, and one careless field would put a client's captured request context
    /// into a line that asked only whether a call was made.
    fn settled(
        &self,
        authorization: &PaidProbeAuthorization,
        outcome: PaidProbeDialOutcome,
    ) -> PaidProbeDialOutcome {
        tracing::debug!(
            event = PAID_PROBE_DIAL_EVENT,
            outcome = outcome.as_str(),
            // The closed failure token, or the empty string on a non-failure. A
            // closed-set literal either way -- never the upstream's own message,
            // which is exactly what a failure diagnostic would otherwise reach
            // for on a money-spending path.
            failure = outcome.failure().map_or("", PaidProbeFailure::as_str),
            incarnation = authorization.incarnation(),
            committed_used = authorization.committed().used(),
            committed_cap = authorization.committed().cap(),
            "paid probe pass settled",
        );
        outcome
    }
}

/// Event name of the paid-dial settlement diagnostic.
const PAID_PROBE_DIAL_EVENT: &str = "paid_probe_dial_settled";

#[cfg(test)]
#[path = "paid_probe_dial_tests.rs"]
mod paid_probe_dial_tests;
