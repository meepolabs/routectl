//! The acknowledged atomic capability-event batch.
//!
//! Every other write in this crate is best effort: a dropped usage row costs
//! accounting fidelity and nothing else, so the producer never waits and
//! never learns whether the row landed. One class of write cannot work that
//! way.
//!
//! The capability ledger's replay boundary is a tombstone row, and the reader
//! trusts only rows appended AFTER the newest one. So a caller that moves the
//! boundary must, in the same breath, re-append every entry that has to
//! survive past it. Those two writes are one indivisible fact: a committed
//! tombstone whose survivors were dropped does not lose telemetry, it
//! silently evicts live routing state -- the precise failure this batch
//! exists to prevent. And a caller that publishes a new router before the
//! batch is durable cannot tell the difference.
//!
//! Hence two departures from the crate's best-effort posture, both scoped to
//! this one operation:
//!
//! - **It is acknowledged.** The caller learns, within a bounded wait,
//!   whether the rows committed -- and every non-commit is a NAMED outcome
//!   ([`BatchCommit`]), never an ambiguous silence, so a caller can hold its
//!   old state rather than proceed on an unpersisted boundary.
//! - **It bypasses the `usage.enabled` gate.** That gate is an operator's
//!   telemetry preference; honoring it here would let disabling capture
//!   destroy routing state, which no operator setting that word means to do.
//!   Ordinary request and capability writes stay gated.
//!
//! It rides the EXISTING writer channel and the EXISTING single SQLite
//! connection: the writer thread already serializes every write, so routing
//! the batch through it keeps one writer and one append order, and the whole
//! batch commits inside one transaction
//! ([`crate::capability_event::insert_capability_events_atomic`]).

use crate::capability_event::CapabilityEvent;
use crate::handle::UsageHandle;

/// The outcome of an acknowledged batch commit. Every variant other than
/// [`Self::Committed`] means NOTHING was durably appended by this call, and
/// each names its own cause so a caller can log and act on the distinction
/// (an unavailable writer is an environment fact; a write failure is a DB
/// fault).
///
/// There is deliberately NO "unknown" or "timed out" variant. Such an outcome
/// would mean the rows might still commit later, leaving the caller unable to
/// choose correctly: keeping its old state risks disagreeing with a boundary
/// that did move, and adopting the new state risks depending on one that never
/// landed. Admission is bounded instead, so an outcome is only ever produced
/// for a batch whose fate is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchCommit {
    /// Every row committed in one transaction.
    Committed {
        /// Number of rows appended (zero for an empty batch, a successful
        /// no-op).
        rows: usize,
    },
    /// The writer channel is closed -- the subsystem is shutting down or was
    /// never started.
    Unavailable,
    /// The bounded channel had no free slot, so the batch was never admitted.
    ChannelFull,
    /// The writer reached the batch and the transaction failed (or it holds
    /// no connection). No row committed.
    WriteFailed,
}

/// A batch plus the one-shot channel its acknowledgement returns on. Sent as
/// a single writer message so the batch keeps its place in the writer's
/// append order.
pub struct CapabilityBatch {
    /// The rows to append, in the order they must be appended.
    pub events: Vec<CapabilityEvent>,
    /// Where the writer reports the transaction's outcome. A dropped receiver
    /// (the caller abandoned the wait, e.g. at shutdown) makes the send fail
    /// silently, which is correct: the writer must never block on a caller
    /// that left.
    pub ack: tokio::sync::oneshot::Sender<BatchCommit>,
    /// The registry generation this batch establishes. Once it commits, the
    /// writer refuses any capability event stamped with an older generation:
    /// such an event predates the boundary and, appended after the tombstone,
    /// would restore state the boundary evicted. Transient in-memory
    /// sequencing only -- never persisted.
    pub generation: u64,
    /// The INCARNATION this batch establishes for any key it clears.
    ///
    /// Only a `cleared` row uses it: on commit the writer records it as that
    /// key's purge floor, so a pre-purge event delayed past the clear is dropped
    /// while a genuine post-purge relearn (a strictly greater incarnation) is
    /// accepted. Zero for a batch that clears nothing.
    pub incarnation: u64,
}

/// A pending acknowledged batch: admitted to the writer, outcome not yet
/// known.
///
/// Splitting admission from the wait is what lets the caller hold a registry
/// guard across the snapshot-and-submit step (so no observation can interleave
/// between them) and then release it before awaiting SQLite. Holding a lock
/// across the transaction is never acceptable.
///
/// Dropping the receipt abandons the wait. That is a legitimate shutdown
/// action: the transaction may still commit, but nothing will be published on
/// the strength of it.
#[must_use = "an admitted batch's outcome must be awaited or deliberately abandoned"]
pub struct BatchReceipt {
    ack: tokio::sync::oneshot::Receiver<BatchCommit>,
}

impl BatchReceipt {
    /// Await the definitive outcome.
    ///
    /// There is no timeout: the batch is already queued, and answering while
    /// its transaction can still commit would let the caller act on a boundary
    /// state that does not match the ledger. A writer that vanishes without
    /// acking drops the sender, which resolves this as
    /// [`BatchCommit::Unavailable`].
    pub async fn await_outcome(self) -> BatchCommit {
        (self.ack.await).unwrap_or(BatchCommit::Unavailable)
    }
}

impl UsageHandle {
    /// Admit `events` to the writer as ONE acknowledged transaction, without
    /// blocking, returning the receipt to await later.
    ///
    /// Deliberately bypasses the `usage.enabled` gate: this is a
    /// correctness-control write, not telemetry (see the module docs).
    ///
    /// Non-blocking by contract, because the caller runs this while holding the
    /// registry guard: the boundary snapshot and this submission must be one
    /// indivisible step, or an observation could interleave and end up neither
    /// in the snapshot nor after the boundary. The guard is released before the
    /// receipt is awaited, so no lock is ever held across SQLite.
    ///
    /// `generation` is the registry generation this batch establishes; once it
    /// commits the writer refuses older-generation capability events.
    ///
    /// Returns `Err(BatchCommit::ChannelFull | BatchCommit::Unavailable)` when
    /// the batch was never queued -- reported at admission time, so the caller
    /// learns it wrote nothing without awaiting anything.
    pub fn admit_capability_batch(
        &self,
        events: Vec<CapabilityEvent>,
        generation: u64,
    ) -> Result<BatchReceipt, BatchCommit> {
        self.admit_capability_batch_at(events, generation, 0)
    }

    /// [`Self::admit_capability_batch`] carrying the INCARNATION any `cleared`
    /// row in the batch establishes as its key's purge floor.
    ///
    /// The purge path uses this one: its clear supersedes exactly the version of
    /// the key the operator approved removing, and the floor is what makes a
    /// delayed pre-purge event drop while a genuine relearn still lands.
    pub fn admit_capability_batch_at(
        &self,
        events: Vec<CapabilityEvent>,
        generation: u64,
        incarnation: u64,
    ) -> Result<BatchReceipt, BatchCommit> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<BatchCommit>();
        let batch = CapabilityBatch {
            events,
            ack: ack_tx,
            generation,
            incarnation,
        };
        match self
            .sender()
            .try_send(crate::writer::WriterMessage::capability_batch(batch))
        {
            Ok(()) => Ok(BatchReceipt { ack: ack_rx }),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(BatchCommit::ChannelFull),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(BatchCommit::Unavailable),
        }
    }

    /// Admit a batch and block the calling thread until its outcome is known.
    ///
    /// For synchronous callers only -- notably the startup warm, which runs off
    /// the runtime entirely. An async caller uses
    /// [`Self::admit_capability_batch`] plus
    /// [`BatchReceipt::await_outcome`] instead, so no runtime worker is blocked.
    ///
    /// # Blocking
    ///
    /// Blocks until the writer reports back. MUST NOT be called on a Tokio
    /// worker.
    pub fn commit_capability_events_blocking(
        &self,
        events: Vec<CapabilityEvent>,
        generation: u64,
    ) -> BatchCommit {
        self.commit_capability_events_blocking_at(events, generation, 0)
    }

    /// [`Self::commit_capability_events_blocking`] carrying the purge-floor
    /// incarnation. Synchronous callers only -- see the blocking note above.
    ///
    /// # Blocking
    ///
    /// Blocks until the writer reports back. MUST NOT be called on a Tokio
    /// worker.
    pub fn commit_capability_events_blocking_at(
        &self,
        events: Vec<CapabilityEvent>,
        generation: u64,
        incarnation: u64,
    ) -> BatchCommit {
        match self.admit_capability_batch_at(events, generation, incarnation) {
            Ok(receipt) => receipt
                .ack
                .blocking_recv()
                .unwrap_or(BatchCommit::Unavailable),
            Err(failure) => failure,
        }
    }
}

/// A handle whose writer channel is already closed -- every send fails as
/// [`BatchCommit::Unavailable`].
///
/// Exists because an unavailable writer cannot be produced through the normal
/// lifecycle: `UsageWriter::shutdown` does not close the channel while a
/// `UsageHandle` still holds a sender clone, so a shutdown-based fixture
/// leaves the channel OPEN and the write succeeds. A caller testing its
/// keep-the-old-state-on-failure path needs the genuinely-closed case.
/// Construct a handle over a caller-supplied writer channel.
///
/// Test seam for the cases that need a channel whose consumer they control: an
/// admitted-but-unanswered batch (to exercise the shutdown race) cannot be
/// produced through the real writer, which always answers.
#[doc(hidden)]
pub fn handle_over_channel(
    sender: tokio::sync::mpsc::Sender<crate::writer::WriterMessage>,
) -> UsageHandle {
    UsageHandle::new(
        sender,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        std::sync::Arc::new(crate::handle::UsageCounters::default()),
        crate::paid_probe_lifecycle::LifecycleGate::running(),
    )
}

#[doc(hidden)]
pub fn handle_with_closed_channel() -> UsageHandle {
    let (tx, rx) = tokio::sync::mpsc::channel::<crate::writer::WriterMessage>(1);
    drop(rx);
    UsageHandle::new(
        tx,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        std::sync::Arc::new(crate::handle::UsageCounters::default()),
        crate::paid_probe_lifecycle::LifecycleGate::running(),
    )
}

#[cfg(test)]
#[path = "capability_batch_tests.rs"]
mod tests;
