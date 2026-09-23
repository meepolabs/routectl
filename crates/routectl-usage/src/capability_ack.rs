//! The acknowledged single capability-event write.
//!
//! Every ordinary capability-event write in this crate is best effort: a dropped
//! row costs the ledger a fact and the next warm rebuild a little evidence, and
//! the producer never waits or learns whether it landed. One consumer cannot work
//! that way.
//!
//! A learned envelope-field verdict becomes eligible to rewrite client requests
//! BEFORE any rejection only once its event write is durable. The reason is not
//! bookkeeping: every way that verdict can be taken back out of service -- the
//! durable clear a disproving canary performs, an operator purge, a later
//! confirmation -- is itself a capability-event write. A verdict made eligible on
//! a row that never landed is one the next boot's replay cannot see, so it could
//! rewrite traffic today and be unexplainable tomorrow. The router therefore
//! advances pre-flight eligibility on the ACKNOWLEDGMENT and nowhere else.
//!
//! # What this is NOT
//!
//! It is not the boundary batch ([`crate::capability_batch`]). That one
//! ESTABLISHES a replay boundary, raises per-key purge floors, commits several
//! rows in one transaction, and deliberately bypasses the `usage.enabled` gate
//! because a telemetry preference must not destroy routing state. This carries
//! ONE ordinary event that establishes nothing, so it passes the same
//! boundary-generation check and the same per-key purge floor a best-effort event
//! passes -- through the writer's one shared body, not a copy of it.
//!
//! It does honour the `usage.enabled` gate at admission, and that is deliberate
//! too: an operator who turned capture off gets no new rows, so no verdict may
//! become pre-flight eligible on the strength of a row that was never written.
//! The refusal is reported rather than silent, so the router declines to advance
//! eligibility instead of assuming it.

use crate::capability_event::CapabilityEvent;
use crate::handle::UsageHandle;
use crate::writer::{CapabilityEventWrite, EventStamp};

/// One capability event plus the one-shot channel its outcome returns on. Sent as
/// a single writer message so it keeps its place in the writer's append order.
///
/// CRATE-INTERNAL and deliberately not re-exported: it is constructed only inside
/// [`UsageHandle::admit_acknowledged_capability_event`], which is what applies the
/// enabled gate and the refusal accounting. A downstream caller able to build one
/// could put it on the channel directly and bypass both -- and the accounting is
/// what capability-persistence health reads.
#[expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so pub(crate) reads as redundant -- but \
              `writer::WriterCommand` names this type in its own variant, so the \
              visibility is load-bearing; `pub` would publish the payload a caller \
              could use to bypass the admission's enabled gate and accounting"
)]
pub(crate) struct AcknowledgedCapabilityEvent {
    /// The row to append.
    pub(crate) event: CapabilityEvent,
    /// The event's own sequencing stamp, from the guarded registry mutation that
    /// produced it. Checked against the committed boundary and the key's purge
    /// floor exactly as a best-effort event's is.
    pub(crate) stamp: EventStamp,
    /// Where the writer reports what the attempt established. A dropped receiver
    /// (the caller abandoned the wait, e.g. at shutdown) makes the send fail
    /// silently, which is correct: the writer must never block on a caller that
    /// left.
    pub(crate) ack: tokio::sync::oneshot::Sender<CapabilityEventWrite>,
}

/// A pending acknowledged event: admitted to the writer, outcome not yet known.
///
/// Splitting admission from the wait is what lets a request-path caller hand the
/// event off without awaiting SQLite inline, then await the outcome where it can
/// afford to. Dropping the receipt abandons the wait; the row may still commit,
/// but nothing will be published on the strength of it.
#[must_use = "an admitted event's outcome must be awaited or deliberately abandoned"]
pub struct CapabilityEventReceipt {
    ack: tokio::sync::oneshot::Receiver<CapabilityEventWrite>,
}

impl CapabilityEventReceipt {
    /// Await the definitive outcome.
    ///
    /// There is no timeout, for the same reason the boundary batch's wait has
    /// none: the event is already queued, and answering while its insert can
    /// still land would let the caller advance eligibility on a row whose fate is
    /// unsettled. A writer that vanishes without acking drops the sender, which
    /// resolves this as [`CapabilityEventWrite::WriteFailed`] -- the safe answer,
    /// since nothing is known to have landed.
    pub async fn await_outcome(self) -> CapabilityEventWrite {
        (self.ack.await).unwrap_or(CapabilityEventWrite::WriteFailed)
    }
}

impl UsageHandle {
    /// Admit one capability event whose outcome the caller will await.
    ///
    /// Non-blocking: the send is a `try_send`, so a saturated channel is reported
    /// at admission time rather than awaited. The caller learns it wrote nothing
    /// without waiting for anything.
    ///
    /// `Err(CapabilityEventWrite::WriteFailed)` when the event was never queued at
    /// all -- a closed channel, a full channel, or capture disabled. All three are
    /// one answer here because the caller's action is identical for each: it is not
    /// known to be durable, so no verdict may become pre-flight eligible on it.
    /// Each is separately visible on its own counter.
    pub fn admit_acknowledged_capability_event(
        &self,
        event: CapabilityEvent,
        generation: u64,
        incarnation: u64,
    ) -> Result<CapabilityEventReceipt, CapabilityEventWrite> {
        // THE ENABLED GATE, honoured here unlike the boundary batch: an operator
        // who turned capture off gets no new rows, so nothing may be treated as
        // durable. Reported rather than silently dropped, so the caller declines
        // to advance eligibility instead of assuming it.
        if !self.is_enabled() {
            self.counters().incr_dropped_disabled();
            return Err(CapabilityEventWrite::WriteFailed);
        }
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<CapabilityEventWrite>();
        let command = AcknowledgedCapabilityEvent {
            event,
            stamp: EventStamp {
                generation,
                incarnation,
            },
            ack: ack_tx,
        };
        match self
            .sender()
            .try_send(crate::writer::WriterMessage::acknowledged_capability_event(
                command,
            )) {
            Ok(()) => {
                self.counters().incr_capability_events_enqueued();
                Ok(CapabilityEventReceipt { ack: ack_rx })
            }
            // FULL and CLOSED counted APART, through the shared classifier: this
            // path previously reported a closed channel on the full counter, which
            // misread a shutting-down daemon as a saturated one. Both still resolve
            // to one OUTCOME here, because a caller's action is identical either way
            // -- the row is not durable -- while the counters keep the distinction an
            // operator needs.
            Err(refusal) => {
                self.note_capability_refusal(&refusal);
                Err(CapabilityEventWrite::WriteFailed)
            }
        }
    }
}

#[cfg(test)]
#[path = "capability_ack_tests.rs"]
mod tests;
