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

use crate::learn_event::CapabilityLearnEvent;
use crate::record::UsageRecord;
use crate::writer::WriterMessage;

/// Shared, lock-free health counters. The producer side bumps
/// `enqueued` / `dropped_full`; the consumer thread bumps the persist /
/// error / prune counters. All reads are relaxed snapshots for
/// observability -- never used for control flow.
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
}

impl UsageHandle {
    pub(crate) const fn new(
        sender: Sender<WriterMessage>,
        enabled: Arc<AtomicBool>,
        counters: Arc<UsageCounters>,
    ) -> Self {
        Self {
            sender,
            enabled,
            counters,
        }
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
            .try_send(WriterMessage::Request(Box::new(record)))
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
    pub fn try_send_learn_event(&self, event: CapabilityLearnEvent) {
        if !self.is_enabled() {
            self.counters.incr_dropped_disabled();
            return;
        }
        match self.sender.try_send(WriterMessage::LearnEvent(event)) {
            Ok(()) => self.counters.incr_learn_events_enqueued(),
            Err(_) => self.note_learn_event_overflow_drop(),
        }
    }

    /// Whether usage capture is currently enabled (runtime-flippable).
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flip the runtime enabled gate. The daemon calls this on hot-reload;
    /// no restart of the writer task is needed.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Read-only view of the shared health counters.
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
}
