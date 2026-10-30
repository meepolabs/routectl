//! The usage writer's health as the status surface reads it: whether the writer
//! can persist rows at all, the capability-persistence failure counters, the
//! request-row loss counters, and whether a ledger window is complete.

use std::sync::Arc;

use routectl_usage::{QueryError, UsageCounters, UsageDb, observation_totals};

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

    /// The three request-row loss counters, read once.
    pub(super) fn ledger_health(&self) -> LedgerHealth {
        LedgerHealth {
            dropped_full: self.counters.dropped_full(),
            dropped_closed: self.counters.dropped_closed(),
            write_errors: self.counters.write_errors(),
        }
    }
}

/// The live request-row loss counters, process-lifetime and monotonic (they
/// reset only on restart). Reported apart because they reach the ledger
/// differently: the writer records every full-queue drop as a `ledger_gaps` row,
/// so `dropped_full` already marks the window it landed in; a closed-channel drop
/// or a failed write leaves no row behind, so only these live counters say a
/// window may be missing rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub(super) struct LedgerHealth {
    /// Request rows the full writer channel refused.
    pub dropped_full: u64,
    /// Request rows refused because the writer channel was closed.
    pub dropped_closed: u64,
    /// Writes the writer thread failed to persist (shared with capability rows).
    pub write_errors: u64,
}

impl LedgerHealth {
    /// Whether this process lost request rows that no `ledger_gaps` row records.
    /// Such a loss is not tied to a time, so it marks every window incomplete.
    const fn has_unrecorded_loss(&self) -> bool {
        self.dropped_closed > 0 || self.write_errors > 0
    }
}

/// One window's ledger observation totals and whether its rows are complete.
///
/// `complete` is false when a `ledger_gaps` row falls inside the window or when
/// the live counters show a loss no gap row records. A closed-channel drop from
/// an EARLIER process is not visible here at all: nothing was left to write it
/// down, so such a window can read complete while missing rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) struct WindowObservation {
    /// Inclusive lower bound of the window, epoch-ms.
    pub from_ms: i64,
    /// Exclusive upper bound of the window, epoch-ms.
    pub to_ms: i64,
    pub wire_losses: i64,
    pub translation_drops: i64,
    pub local_refusals: i64,
    pub field_repairs: i64,
    pub missing_terminals: i64,
    /// `ledger_gaps` rows recorded inside the window.
    pub gap_rows: i64,
    /// Request rows those gap rows say were lost.
    pub gap_dropped: i64,
    pub complete: bool,
}

/// Read one window's observation totals from the ledger and judge completeness
/// against the live counters in `health`.
pub(super) fn window_observation(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
    health: LedgerHealth,
) -> Result<WindowObservation, QueryError> {
    let totals = observation_totals(db, from_ms, to_ms)?;
    Ok(WindowObservation {
        from_ms,
        to_ms,
        wire_losses: totals.wire_losses,
        translation_drops: totals.translation_drops,
        local_refusals: totals.local_refusals,
        field_repairs: totals.field_repairs,
        missing_terminals: totals.missing_terminals,
        gap_rows: totals.gap_rows,
        gap_dropped: totals.gap_dropped,
        complete: !totals.has_gap() && !health.has_unrecorded_loss(),
    })
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
