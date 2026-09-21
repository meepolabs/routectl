//! The acknowledged paid-probe reservation: the writer command, its bounded
//! admission, and the outcome vocabulary a spender may act on.
//!
//! One unit of paid-probe budget is a SPEND, not telemetry, so this write
//! shares nothing with the crate's best-effort posture beyond the channel it
//! rides. Three departures, all scoped to this one operation and all for the
//! same reason -- a caller may make a paid upstream call ONLY on a unit that
//! is already durable:
//!
//! - **It is acknowledged.** The caller learns what the accounting state
//!   established, and every answer is a NAMED [`PaidProbeCommit`]. Only
//!   [`PaidProbeCommit::Committed`] means a unit is spent and a paid call may
//!   follow; every other answer means nothing was written.
//! - **It bypasses the `usage.enabled` gate.** That gate is an operator's
//!   telemetry preference. Honoring it here would let disabling capture
//!   silently disable the spend ceiling -- either blocking every paid call or,
//!   far worse, licensing one against no accounting at all. Ordinary request
//!   and capability writes stay gated.
//! - **Admission is bounded and non-blocking.** A saturated or closed channel
//!   is refused at admission ([`PaidProbeAdmission::Saturated`] /
//!   [`PaidProbeAdmission::Unavailable`]) rather than queued or waited on, so
//!   a wedged writer can never stall a request path -- and a refusal
//!   authorizes nothing.
//!
//! It rides the EXISTING writer channel and the EXISTING single SQLite
//! connection. That is what makes the cap hold: the writer thread is the one
//! serialized point of truth for this database, and the reservation itself
//! runs inside one immediate transaction
//! ([`crate::paid_probe::reserve_one_unit`]), so two processes sharing the
//! file still cannot push a provider-day past its cap.
//!
//! THE ANSWER IS PRODUCED AFTER THE TRANSACTION, never before it: the writer
//! samples the clock, runs the reservation, and only then acknowledges. So a
//! `Committed` answer always describes a unit that is already on disk.
//!
//! THERE IS NO RELEASE, REFUND, DECREMENT, OR TIMED-OUT ANSWER, and the
//! absence is the design. A committed unit is spent: releasing one whose call
//! may already have reached the upstream is how a crash loop spends a day's
//! cap several times over. By the same rule a caller that drops its receipt
//! loses only the ANSWER -- the unit stays committed, the writer stays
//! healthy, and nothing is rolled back.
//!
//! WHAT DOES NOT CROSS THIS BOUNDARY: the accounting day and the storage key.
//! Which day a unit landed in, how that day is resolved, and how a rollover is
//! handled are this crate's concerns; a spender needs only whether a unit was
//! committed and where that leaves the budget.

#![expect(
    clippy::redundant_pub_crate,
    reason = "this module is private and its published types are re-exported by \
              the crate root, so a crate-wide visibility on the writer-side \
              entry points reads as redundant -- but the writer has to name \
              them, and widening them to `pub` would publish the connection \
              and the storage vocabulary a spender must never see"
)]

use crate::handle::UsageHandle;
use crate::paid_probe::{StoredReservation, reserve_one_unit};

/// What one acknowledged reservation established.
///
/// Only [`Self::Committed`] permits a paid call. The refusals stay DISTINCT
/// rather than collapsing into one "no": an exhausted cap is the budget
/// working as configured, while unreadable state or a failed write is
/// accounting the operator needs to know is broken, and one shared refusal
/// would make a broken ledger look like a lane that simply spent its day.
///
/// There is deliberately no unknown or timed-out variant. Such an answer would
/// mean the unit might still commit afterwards, leaving the caller unable to
/// choose correctly -- and because nothing is ever given back, a wrong guess
/// here either loses a day's budget or spends it twice. Admission is bounded
/// instead, so an outcome is only ever produced for a reservation whose fate is
/// settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidProbeCommit {
    /// One unit is durably committed. The ONLY answer a paid call may follow.
    ///
    /// `#[non_exhaustive]` so no crate but this one can CONSTRUCT the
    /// authorizing value. The fields stay public and readable and matching still
    /// works (`Committed { used, cap, .. }`); what becomes impossible is a
    /// downstream crate building this variant itself and feeding it to its own
    /// spend check, routing around the reservation entirely. The value that
    /// authorizes money must be obtainable only from the writer that committed
    /// the unit.
    #[non_exhaustive]
    Committed {
        /// Units committed for this provider's current accounting day,
        /// INCLUDING this one.
        used: u32,
        /// The cap this reservation was checked against.
        cap: u32,
    },
    /// The configured cap is zero, or the day's units are already spent.
    /// Nothing was written.
    CapExhausted,
    /// The stored accounting state is not a count this crate would have
    /// written. Refused rather than read as zero -- reading corrupt accounting
    /// as "nothing spent" licenses a full cap's worth of calls every time it is
    /// read -- and the stored bytes are left identical.
    MalformedState,
    /// The durable write was attempted and did not land, or the writer holds no
    /// connection at all. No unit was committed.
    WriteFailed,
    /// The accounting subsystem is shutting down, so no paid call may proceed.
    ///
    /// Says nothing about whether a unit was spent, deliberately. It covers both
    /// the reservation refused before its transaction and the one whose
    /// transaction COMMITTED as shutdown began -- in the second case the unit is
    /// on disk and stays there, because nothing is ever given back. A caller
    /// needs exactly one fact here (it may not make the call), and a variant that
    /// distinguished the two would invite a refund path for the difference.
    Unavailable,
}

/// The result of offering a reservation to the writer.
///
/// Admission and the wait are separate steps because they fail for unrelated
/// reasons: a saturated or closed channel is knowable immediately and without
/// blocking, while a commit outcome takes a transaction. Splitting them keeps
/// the refusals out of [`PaidProbeCommit`], so a spender cannot mistake "never
/// submitted" for something the accounting state decided.
#[must_use = "a refused admission authorizes nothing, and an accepted one must be awaited"]
pub enum PaidProbeAdmission {
    /// The reservation is queued. Await the receipt for its outcome.
    Admitted(PaidProbeReceipt),
    /// The bounded channel had no free slot, so nothing was submitted. A LOAD
    /// condition rather than a fault: a later attempt may reach a different
    /// answer with nothing repaired.
    Saturated,
    /// The writer channel is closed -- the subsystem is shutting down or was
    /// never started. Nothing was submitted and nothing can be.
    Unavailable,
}

/// A reservation the writer has accepted, whose outcome is not yet known.
///
/// Dropping the receipt abandons the WAIT, not the reservation: the
/// transaction runs, a committed unit stays committed, and the writer is
/// unaffected. That is the correct shutdown action -- no paid call happens on
/// an answer nobody read.
#[must_use = "an admitted reservation's outcome must be awaited or deliberately abandoned"]
pub struct PaidProbeReceipt {
    ack: tokio::sync::oneshot::Receiver<PaidProbeCommit>,
}

impl PaidProbeReceipt {
    /// Await the definitive outcome.
    ///
    /// There is no time limit: the reservation is already queued, and answering
    /// while its transaction can still commit would let a caller spend against
    /// a unit it believes was refused, or refuse a call whose unit is already
    /// gone. A writer that vanishes without answering drops the sender, which
    /// resolves here as [`PaidProbeCommit::Unavailable`] -- the fail-closed
    /// direction: no paid call proceeds, and at worst a single unit is unusable.
    pub async fn await_outcome(self) -> PaidProbeCommit {
        (self.ack.await).unwrap_or(PaidProbeCommit::Unavailable)
    }
}

/// A reservation request plus the one-shot channel its answer returns on. Sent
/// as a single writer message, so it keeps its place in the writer's serialized
/// order.
///
/// CRATE-PRIVATE, and it must stay that way: it holds the ack sender, so anything
/// able to name and build one could answer a caller's reservation itself --
/// forging the single value that authorizes a paid call. The transport envelope
/// that carries it is opaque for the same reason.
pub(crate) struct PaidProbeCommand {
    /// The CONFIGURED provider name this unit is spent against.
    pub(crate) provider: String,
    /// The configured daily cap the reservation is checked against, passed in
    /// rather than read here so the cap the caller gated on and the cap the
    /// reservation enforces cannot be two different numbers.
    pub(crate) cap: u32,
    /// Where the writer reports the outcome. A dropped receiver makes the send
    /// fail silently, which is correct: the writer never blocks on a caller
    /// that left.
    ack: tokio::sync::oneshot::Sender<PaidProbeCommit>,
}

impl PaidProbeCommand {
    /// Report the outcome, ignoring a caller that stopped listening.
    pub(crate) fn answer(self, outcome: PaidProbeCommit) {
        let _ = self.ack.send(outcome);
    }
}

impl UsageHandle {
    /// Offer one paid-probe reservation for `provider` against `cap` to the
    /// writer, without ever blocking.
    ///
    /// Deliberately bypasses the `usage.enabled` gate: this is a spend control,
    /// not telemetry (see the module docs).
    ///
    /// Returns immediately in every case. A queued reservation yields a receipt
    /// to await; a saturated or closed channel, or a subsystem already shutting
    /// down, is a named refusal that authorizes nothing.
    pub fn admit_paid_probe_reservation(&self, provider: &str, cap: u32) -> PaidProbeAdmission {
        // Refuse the moment shutdown has begun, BEFORE anything is queued. The
        // channel alone is not enough of a signal: it stays open while any handle
        // holds a sender clone, so a reservation admitted here could otherwise be
        // answered by a writer that is already draining.
        if self.is_shutting_down() {
            return PaidProbeAdmission::Unavailable;
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<PaidProbeCommit>();
        let command = PaidProbeCommand {
            provider: provider.to_string(),
            cap,
            ack: ack_tx,
        };
        match self
            .sender()
            .try_send(crate::writer::WriterMessage::paid_probe_reservation(
                command,
            )) {
            Ok(()) => PaidProbeAdmission::Admitted(PaidProbeReceipt { ack: ack_rx }),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => PaidProbeAdmission::Saturated,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                PaidProbeAdmission::Unavailable
            }
        }
    }
}

/// Run one reservation against the writer's connection.
///
/// Called from the writer thread's message loop, which owns the connection and
/// applies the health transition the returned outcome implies. `now_epoch_ms`
/// is sampled by that loop immediately before this call, so the day a unit
/// lands in is the day the writer decided on and cannot drift between the cap
/// gate and the commit.
///
/// A writer holding no connection (a failed open) cannot establish anything, so
/// it reports a failed write rather than guessing a count.
pub(crate) fn reserve(
    conn: Option<&mut rusqlite::Connection>,
    command: &PaidProbeCommand,
    now_epoch_ms: i64,
) -> PaidProbeCommit {
    let Some(conn) = conn else {
        return PaidProbeCommit::WriteFailed;
    };
    let stored = reserve_one_unit(conn, &command.provider, command.cap, now_epoch_ms);
    if let StoredReservation::Committed { utc_day, used, cap } = stored {
        tracing::debug!(
            target: "routectl_usage::paid_probe",
            provider = %command.provider,
            utc_day,
            used,
            cap,
            "paid-probe unit committed"
        );
    }
    commit_for(stored)
}

/// Map a storage outcome onto the answer a spender reads.
///
/// Exhaustive by construction, so a storage outcome added later cannot fall
/// into a permissive default: the only mapping that authorizes a paid call is
/// the one the storage layer says committed.
pub(crate) const fn commit_for(stored: StoredReservation) -> PaidProbeCommit {
    match stored {
        // The day and the storage key stay on this side of the boundary.
        StoredReservation::Committed { used, cap, .. } => PaidProbeCommit::Committed { used, cap },
        StoredReservation::CapExhausted => PaidProbeCommit::CapExhausted,
        StoredReservation::MalformedState => PaidProbeCommit::MalformedState,
        StoredReservation::WriteFailed => PaidProbeCommit::WriteFailed,
    }
}

#[cfg(test)]
#[path = "paid_probe_command_tests.rs"]
mod tests;

/// Build a reservation command plus the receiver its answer arrives on.
///
/// Crate-private test support: the command holds the ack sender, so this cannot
/// be published without publishing the ability to answer somebody else's
/// reservation. Reached through `crate::writer::paid_probe_command_for_tests`.
#[cfg(test)]
pub(crate) fn command_for_tests(
    provider: &str,
    cap: u32,
) -> (
    PaidProbeCommand,
    tokio::sync::oneshot::Receiver<PaidProbeCommit>,
) {
    let (ack, rx) = tokio::sync::oneshot::channel::<PaidProbeCommit>();
    (
        PaidProbeCommand {
            provider: provider.to_string(),
            cap,
            ack,
        },
        rx,
    )
}
