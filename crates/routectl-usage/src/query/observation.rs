//! Windowed request-observation totals from the ledger, with the gap check
//! that says whether the window lost rows.

use crate::db::UsageDb;

use super::QueryError;

/// Ledger-side observation totals over one half-open window.
///
/// The five counts are summed over EVERY `requests` row in the window,
/// token-count rows included: the process-wide live counters count that
/// traffic too, so a caller combining the two must not see the ledger side
/// filtered. A row written before the columns existed (NULL) contributes 0.
///
/// The gap fields come from `ledger_gaps`. A nonzero `gap_rows` means request
/// rows were lost to a full queue, so every count here is a lower bound.
/// Plain data; the caller decides how to display it and which live counters
/// to combine with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ObservationTotals {
    /// Summed `wire_losses`.
    pub wire_losses: i64,
    /// Summed `translation_drops`.
    pub translation_drops: i64,
    /// Summed `local_refusals`.
    pub local_refusals: i64,
    /// Summed `field_repairs`.
    pub field_repairs: i64,
    /// Summed `missing_terminals`.
    pub missing_terminals: i64,
    /// `ledger_gaps` rows recorded inside the window.
    pub gap_rows: i64,
    /// Summed `ledger_gaps.dropped` over those rows: request rows lost.
    pub gap_dropped: i64,
}

impl ObservationTotals {
    /// Whether the ledger recorded a gap inside the window, i.e. the window's
    /// ledger rows are incomplete.
    pub const fn has_gap(&self) -> bool {
        self.gap_rows > 0
    }
}

/// One statement so the request sums and the gap check read one snapshot.
/// The `ledger_gaps` subquery scans the table: it holds one row per drop
/// episode, so it stays far too small for an index on `ts_ms` to pay for its
/// write and migration cost.
pub(super) const OBSERVATION_TOTALS_SQL: &str = "\
SELECT
    COALESCE(SUM(wire_losses), 0)       AS wire_losses,
    COALESCE(SUM(translation_drops), 0) AS translation_drops,
    COALESCE(SUM(local_refusals), 0)    AS local_refusals,
    COALESCE(SUM(field_repairs), 0)     AS field_repairs,
    COALESCE(SUM(missing_terminals), 0) AS missing_terminals,
    (SELECT COUNT(*) FROM ledger_gaps
      WHERE ts_ms >= ?1 AND ts_ms < ?2)                AS gap_rows,
    (SELECT COALESCE(SUM(dropped), 0) FROM ledger_gaps
      WHERE ts_ms >= ?1 AND ts_ms < ?2)                AS gap_dropped
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2";

/// Observation totals for requests whose `ts_start` is in `[from_ms, to_ms)`,
/// and gap rows whose `ts_ms` is in the same half-open window.
///
/// A gap row's `ts_ms` is when the writer RECORDED the drops, not when the
/// lost requests started, so a drop just before `from_ms` that the writer
/// recorded inside the window marks this window incomplete rather than the
/// earlier one.
pub fn observation_totals(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<ObservationTotals, QueryError> {
    let totals = db
        .conn()
        .query_row(OBSERVATION_TOTALS_SQL, [from_ms, to_ms], |row| {
            Ok(ObservationTotals {
                wire_losses: row.get(0)?,
                translation_drops: row.get(1)?,
                local_refusals: row.get(2)?,
                field_repairs: row.get(3)?,
                missing_terminals: row.get(4)?,
                gap_rows: row.get(5)?,
                gap_dropped: row.get(6)?,
            })
        })?;
    Ok(totals)
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
