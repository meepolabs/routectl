//! The Clone producer handle plus shared health counters.
//!
//! `UsageHandle` is the only surface request handlers touch. It is
//! `Clone` (cheap -- a few `Arc`s and a bounded `Sender`), and its sole
//! hot-path method, [`UsageHandle::try_send`], never blocks, never
//! awaits, and never panics. When usage is disabled the record is dropped
//! at the gate; when the bounded channel is full the record is dropped
//! and an atomic counter is bumped. Either way the caller returns
//! immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::mpsc::Sender;

use crate::capability_event::CapabilityEvent;
use crate::learn_event::CapabilityLearnEvent;
use crate::record::UsageRecord;
use crate::writer::WriterMessage;

/// Shared, lock-free health counters. The producer side bumps
/// `enqueued` / `dropped_full`; the consumer thread bumps the persist /
/// error / prune counters. All reads are relaxed snapshots for
/// observability -- never used for control flow.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct UsageCounters {
    enqueued: AtomicU64,
    dropped_full: AtomicU64,
    dropped_disabled: AtomicU64,
    persisted: AtomicU64,
    write_errors: AtomicU64,
    prune_errors: AtomicU64,
    learn_events_enqueued: AtomicU64,
    learn_events_dropped_full: AtomicU64,
    learn_events_persisted: AtomicU64,
    capability_events_enqueued: AtomicU64,
    /// Capability events the writer dropped because an operator purge of the same
    /// key had already superseded them.
    ///
    /// Expected to be zero on a daemon nobody purges, and small and bounded on
    /// one that does: it counts genuinely delayed pre-purge metadata, not
    /// failures. A LARGE value means events are queueing for long enough to
    /// straddle purges, which is a throughput signal rather than a correctness
    /// one -- the drop itself is the protection working.
    capability_events_superseded: AtomicU64,
    capability_events_dropped_full: AtomicU64,
    capability_events_persisted: AtomicU64,
    /// Paid-probe units that COMMITTED but whose caller was never authorized,
    /// because shutdown began between the transaction and the answer.
    ///
    /// Budget consumed for no call. Expected to be zero on a daemon that is not
    /// being torn down, and at most a handful per shutdown; anything larger means
    /// reservations are routinely straddling teardown. Deliberately its own
    /// counter rather than a degraded/healthy transition: it is neither a storage
    /// fault (the write landed) nor a healthy write (nothing was authorized), and
    /// folding it into either would hide a real divergence between what was spent
    /// and what was used.
    paid_probe_consumed_unauthorized: AtomicU64,
}

impl UsageCounters {
    /// Records accepted into the channel by `try_send`.
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Relaxed)
    }

    /// Records dropped because the bounded channel was full.
    pub fn dropped_full(&self) -> u64 {
        self.dropped_full.load(Ordering::Relaxed)
    }

    /// Records dropped at the enabled gate (intentional, not overflow).
    pub fn dropped_disabled(&self) -> u64 {
        self.dropped_disabled.load(Ordering::Relaxed)
    }

    /// Rows successfully persisted by the consumer thread.
    pub fn persisted(&self) -> u64 {
        self.persisted.load(Ordering::Relaxed)
    }

    /// Write failures (INSERT errors, degraded/no-DB drops) seen by the
    /// consumer thread.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    /// Startup-prune failures (best-effort; never blocks serving).
    pub fn prune_errors(&self) -> u64 {
        self.prune_errors.load(Ordering::Relaxed)
    }

    /// Learn events accepted into the channel by `try_send_learn_event`.
    pub fn learn_events_enqueued(&self) -> u64 {
        self.learn_events_enqueued.load(Ordering::Relaxed)
    }

    /// Learn events dropped because the bounded channel was full or closed.
    pub fn learn_events_dropped_full(&self) -> u64 {
        self.learn_events_dropped_full.load(Ordering::Relaxed)
    }

    /// Learn-event rows successfully persisted by the consumer thread.
    pub fn learn_events_persisted(&self) -> u64 {
        self.learn_events_persisted.load(Ordering::Relaxed)
    }

    /// Capability events accepted into the channel by
    /// `try_send_capability_event`.
    pub fn capability_events_enqueued(&self) -> u64 {
        self.capability_events_enqueued.load(Ordering::Relaxed)
    }

    /// Capability events dropped because the bounded channel was full or
    /// closed.
    pub fn capability_events_dropped_full(&self) -> u64 {
        self.capability_events_dropped_full.load(Ordering::Relaxed)
    }

    /// Capability-event rows successfully persisted by the consumer thread.
    /// Capability events dropped as superseded by an operator purge of the same
    /// key. See the field's own note on how to read a nonzero value.
    pub fn capability_events_superseded(&self) -> u64 {
        self.capability_events_superseded.load(Ordering::Relaxed)
    }

    pub fn capability_events_persisted(&self) -> u64 {
        self.capability_events_persisted.load(Ordering::Relaxed)
    }

    /// Paid-probe units committed whose caller was not authorized, because
    /// shutdown began first. See the field's own note on how to read it.
    pub fn paid_probe_consumed_unauthorized(&self) -> u64 {
        self.paid_probe_consumed_unauthorized
            .load(Ordering::Relaxed)
    }

    pub(crate) fn incr_enqueued(&self) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_dropped_full(&self) -> u64 {
        self.dropped_full.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn incr_dropped_disabled(&self) {
        self.dropped_disabled.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_persisted(&self) {
        self.persisted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_write_errors(&self) -> u64 {
        self.write_errors.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn incr_prune_errors(&self) {
        self.prune_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_learn_events_enqueued(&self) {
        self.learn_events_enqueued.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_learn_events_dropped_full(&self) -> u64 {
        self.learn_events_dropped_full
            .fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn incr_learn_events_persisted(&self) {
        self.learn_events_persisted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_capability_events_enqueued(&self) {
        self.capability_events_enqueued
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_capability_events_superseded(&self) {
        self.capability_events_superseded
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_capability_events_dropped_full(&self) -> u64 {
        self.capability_events_dropped_full
            .fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn incr_capability_events_persisted(&self) {
        self.capability_events_persisted
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn incr_paid_probe_consumed_unauthorized(&self) {
        self.paid_probe_consumed_unauthorized
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// WARN about overflow drops at most this often (every Nth drop). The
/// first drop always warns; thereafter every `DROP_WARN_INTERVAL`-th
/// drop warns. Keeps a wedged or saturated channel from flooding logs.
const DROP_WARN_INTERVAL: u64 = 1024;

/// The cheap, `Clone` producer handle wired into request handlers.
///
/// Holds the bounded `Sender`, the runtime-flippable `enabled` flag, and
/// the shared counters. Cloning is cheap; clone freely into per-request
/// state.
#[derive(Clone)]
pub struct UsageHandle {
    sender: Sender<WriterMessage>,
    enabled: Arc<AtomicBool>,
    counters: Arc<UsageCounters>,
    /// Marked terminal once the writer subsystem begins shutting down. Read ONLY
    /// by the acknowledged paid-probe admission, which must not hand out an
    /// authorization the subsystem will not outlive; best-effort writes do not
    /// consult it, because a dropped telemetry row at shutdown is the accepted
    /// contract and a closed channel already covers it.
    lifecycle: crate::paid_probe_lifecycle::LifecycleGate,
}

impl UsageHandle {
    pub(crate) const fn new(
        sender: Sender<WriterMessage>,
        enabled: Arc<AtomicBool>,
        counters: Arc<UsageCounters>,
        lifecycle: crate::paid_probe_lifecycle::LifecycleGate,
    ) -> Self {
        Self {
            sender,
            enabled,
            counters,
            lifecycle,
        }
    }

    /// Whether the writer subsystem has begun shutting down.
    ///
    /// Crate-internal: the only legitimate consumer is the paid-probe
    /// admission, and publishing it would invite a caller to ask the question
    /// and then act on a stale answer -- the check that matters is the one the
    /// writer itself performs around the transaction.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.lifecycle.is_terminal()
    }

    /// Hand a record to the writer without ever blocking, awaiting, or
    /// panicking.
    ///
    /// Returns immediately in every case. When usage is disabled the
    /// record is dropped at the gate (counted as a disabled-drop, not an
    /// overflow). When the bounded channel is full or already closed the
    /// record is dropped and the overflow counter is bumped (with a
    /// rate-limited WARN). Safe to call from any context, including a
    /// `Drop` impl.
    pub fn try_send(&self, record: UsageRecord) {
        if !self.is_enabled() {
            self.counters.incr_dropped_disabled();
            return;
        }
        match self
            .sender
            .try_send(WriterMessage::request(Box::new(record)))
        {
            Ok(()) => self.counters.incr_enqueued(),
            Err(_) => self.note_overflow_drop(),
        }
    }

    /// Hand a capability learn event to the writer without ever blocking,
    /// awaiting, or panicking. Mirrors [`UsageHandle::try_send`]: the same
    /// enabled gate applies (a learn event is a usage write), and a full or
    /// closed channel drops the event with its own counter and rate-limited
    /// WARN. Routing never depends on this landing -- it is best-effort.
    ///
    /// DEPRECATED: the request path no longer calls this -- learned negatives
    /// now ride out as `broken` rows through `try_send_capability_event_in_generation`
    /// into the unified `capability_events` ledger. Retained (with the
    /// `LearnEvent` writer branch and the `capability_learn_events` DDL) so the
    /// legacy write path stays compilable and existing rows are untouched;
    /// removal is a later change.
    pub fn try_send_learn_event(&self, event: CapabilityLearnEvent) {
        if !self.is_enabled() {
            self.counters.incr_dropped_disabled();
            return;
        }
        match self.sender.try_send(WriterMessage::learn_event(event)) {
            Ok(()) => self.counters.incr_learn_events_enqueued(),
            Err(_) => self.note_learn_event_overflow_drop(),
        }
    }

    /// Hand a capability event to the writer without ever blocking,
    /// awaiting, or panicking. Mirrors [`UsageHandle::try_send`]: the same
    /// enabled gate applies (a capability event is a usage write), and a
    /// full or closed channel drops the event with its own counter and
    /// rate-limited WARN. Routing never depends on this landing -- it is
    /// best-effort; the warm-rebuild replayer tolerates the gap.
    /// Hand a capability event to the writer, stamped with the registry
    /// generation that produced it.
    ///
    /// THE only way to enqueue a capability event. There is deliberately no
    /// generation-free wrapper: an unstamped event would default to a generation
    /// older than every boundary, so the writer would drop it -- silently losing
    /// a real observation. Requiring the argument makes the caller state which
    /// generation the event belongs to.
    ///
    /// Best effort like every usage write (the enabled gate applies; a full or
    /// closed channel drops with a counter).
    /// The generation is transient sequencing: the writer drops the event if a
    /// boundary batch has since committed at a NEWER generation, because such
    /// an event predates that boundary and would otherwise be replayed after
    /// its tombstone -- restoring state the boundary evicted. Nothing about the
    /// generation is persisted.
    pub fn try_send_capability_event_in_generation(&self, event: CapabilityEvent, generation: u64) {
        self.try_send_capability_event_at(event, generation, 0);
    }

    /// [`Self::try_send_capability_event_in_generation`] carrying the event's own
    /// INCARNATION, so the writer can drop it if a purge of the same key has
    /// since superseded it. Every producer that has an incarnation passes it;
    /// zero means "no per-key ordering claim", which the floor treats as
    /// superseded by any purge.
    pub fn try_send_capability_event_at(
        &self,
        event: CapabilityEvent,
        generation: u64,
        incarnation: u64,
    ) {
        if !self.is_enabled() {
            self.counters.incr_dropped_disabled();
            return;
        }
        match self.sender.try_send(WriterMessage::capability_event(
            event,
            crate::writer::EventStamp {
                generation,
                incarnation,
            },
        )) {
            Ok(()) => self.counters.incr_capability_events_enqueued(),
            Err(_) => self.note_capability_event_overflow_drop(),
        }
    }

    /// Whether usage capture is currently enabled (runtime-flippable).
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The producer end of the writer channel. Crate-internal so the
    /// acknowledged capability batch can send its own message variant
    /// without duplicating the handle's construction.
    pub(crate) const fn sender(&self) -> &Sender<WriterMessage> {
        &self.sender
    }

    /// Flip the runtime enabled gate. The daemon calls this on hot-reload;
    /// no restart of the writer task is needed.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Read-only view of the shared health counters.
    #[doc(hidden)]
    pub const fn counters(&self) -> &Arc<UsageCounters> {
        &self.counters
    }

    fn note_overflow_drop(&self) {
        let prior = self.counters.incr_dropped_full();
        if prior == 0 || (prior + 1).is_multiple_of(DROP_WARN_INTERVAL) {
            tracing::warn!(
                target: "routectl_usage::handle",
                dropped_total = prior + 1,
                "usage channel full -- dropping record (capture lags writer)"
            );
        }
    }

    fn note_learn_event_overflow_drop(&self) {
        let prior = self.counters.incr_learn_events_dropped_full();
        if prior == 0 || (prior + 1).is_multiple_of(DROP_WARN_INTERVAL) {
            tracing::warn!(
                target: "routectl_usage::handle",
                dropped_total = prior + 1,
                "usage channel full -- dropping learn event (capture lags writer)"
            );
        }
    }

    fn note_capability_event_overflow_drop(&self) {
        let prior = self.counters.incr_capability_events_dropped_full();
        if prior == 0 || (prior + 1).is_multiple_of(DROP_WARN_INTERVAL) {
            tracing::warn!(
                target: "routectl_usage::handle",
                dropped_total = prior + 1,
                "usage channel full -- dropping capability event (capture lags writer)"
            );
        }
    }
}
