//! The usage writer's health as the status surface reads it: whether the writer
//! can persist rows at all, and the capability-persistence failure counters.

use std::sync::Arc;

use routectl_usage::UsageCounters;

/// Read-only view of the usage writer's shared health counters, for the status
/// surface.
///
/// A FACADE rather than the writer's producer handle, because `StatusState`'s whole
/// contract is that it carries no such handle: that handle can enqueue rows, and a
/// status handler holding one would be structurally capable of writing -- which is
/// why a lexical guard in this module's parent refuses its type name in every
/// status source, this one included. This type holds the shared counters `Arc` --
/// which the writer also holds -- and exposes exactly the reads the status panels
/// need. The `Arc` itself is private, so a panel cannot reach past these methods.
pub struct UsageHealthView {
    counters: Arc<UsageCounters>,
}

impl UsageHealthView {
    /// Wrap the writer's shared counters.
    pub const fn new(counters: Arc<UsageCounters>) -> Self {
        Self { counters }
    }

    /// Whether the writer is currently unable to persist rows.
    ///
    /// Derived from the write-error count rather than from the writer's own
    /// degraded flag, because that flag lives on the writer's thread-local state
    /// and nothing outside that thread can read it. The two agree on the question
    /// that matters here: a writer that has failed a write is a writer whose
    /// durability cannot be promised.
    ///
    /// A LATCHING reading, deliberately: the counter never falls, so a writer that
    /// recovered still reports degraded for the rest of the process. It
    /// over-reports a resolved problem rather than under-reporting a live one, and
    /// the operator's recovery signal is the write-error count no longer climbing.
    pub(super) fn writer_degraded(&self) -> bool {
        self.counters.write_errors() > 0
    }

    /// The three capability-persistence failure counters, read once.
    pub(super) fn capability_writes(&self) -> CapabilityWriteCounters {
        CapabilityWriteCounters {
            capability_events_dropped_full: self.counters.capability_events_dropped_full(),
            write_errors: self.counters.write_errors(),
            capability_writer_unavailable: self.counters.capability_writer_unavailable(),
        }
    }
}

/// The live capability-write failure counters, process-lifetime and
/// monotonic (they reset only on restart). Each names a different operator
/// situation, so they are reported apart rather than summed: a full channel
/// is load (the writer is alive but behind), a write error is a store fault
/// (the row reached the writer and did not land), and an unavailable writer
/// is teardown (the channel is closed). Any nonzero value means some learned
/// verdict this process formed may not survive a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) struct CapabilityWriteCounters {
    /// Capability writes the full writer channel refused.
    pub capability_events_dropped_full: u64,
    /// Writes the writer thread failed to persist (shared with usage rows).
    pub write_errors: u64,
    /// Capability writes refused because the writer channel was closed.
    pub capability_writer_unavailable: u64,
}

#[cfg(test)]
#[path = "usage_health_tests.rs"]
mod tests;
