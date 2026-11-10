//! Windowed request-observation totals from the ledger, with the gap check
//! that says whether the window lost rows.

use crate::db::UsageDb;

use super::QueryError;

/// Ledger-side observation totals over one half-open window.
///
/// The five observation counts and the five walk counts are summed over
/// EVERY `requests` row in the window, token-count rows included: the
/// process-wide live counters count that traffic too, so a caller combining
/// the two must not see the ledger side filtered. A row written before a
/// column existed (NULL) contributes 0 to it.
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
    /// Summed `request_caused_failures`: failed attempts the request itself
    /// caused (endpoint or model scope).
    pub request_caused_failures: i64,
    /// Summed `excluded_endpoint`: targets passed over because an endpoint
    /// exclusion covered them.
    pub excluded_endpoint: i64,
    /// Summed `excluded_model`: targets passed over because a model
    /// exclusion covered them.
    pub excluded_model: i64,
    /// Summed `excluded_target`: targets passed over because a target
    /// exclusion covered them.
    pub excluded_target: i64,
    /// Summed `excluded_probe_run`: targets passed over because a probe-run
    /// exclusion covered them.
    pub excluded_probe_run: i64,
    /// `ledger_gaps` rows recorded inside the window: one per writer check
    /// that found new drops, not one per drop episode.
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
/// Each side is a single-row aggregate, so the cross join yields exactly one
/// row. `ledger_gaps` gains a row on every writer check that found new drops,
/// which under sustained saturation is close to one row per message, so the
/// gap side reads `idx_ledger_gaps_ts_ms` (covering `ts_ms` and `dropped`)
/// rather than scanning the table.
pub(super) const OBSERVATION_TOTALS_SQL: &str = "\
SELECT
    r.wire_losses,
    r.translation_drops,
    r.local_refusals,
    r.field_repairs,
    r.missing_terminals,
    r.request_caused_failures,
    r.excluded_endpoint,
    r.excluded_model,
    r.excluded_target,
    r.excluded_probe_run,
    g.gap_rows,
    g.gap_dropped
FROM
    (SELECT
        COALESCE(SUM(wire_losses), 0)       AS wire_losses,
        COALESCE(SUM(translation_drops), 0) AS translation_drops,
        COALESCE(SUM(local_refusals), 0)    AS local_refusals,
        COALESCE(SUM(field_repairs), 0)     AS field_repairs,
        COALESCE(SUM(missing_terminals), 0) AS missing_terminals,
        COALESCE(SUM(request_caused_failures), 0) AS request_caused_failures,
        COALESCE(SUM(excluded_endpoint), 0)       AS excluded_endpoint,
        COALESCE(SUM(excluded_model), 0)          AS excluded_model,
        COALESCE(SUM(excluded_target), 0)         AS excluded_target,
        COALESCE(SUM(excluded_probe_run), 0)      AS excluded_probe_run
     FROM requests
     WHERE ts_start >= ?1 AND ts_start < ?2) AS r,
    (SELECT
        COUNT(*)                    AS gap_rows,
        COALESCE(SUM(dropped), 0)   AS gap_dropped
     FROM ledger_gaps
     WHERE ts_ms >= ?1 AND ts_ms < ?2) AS g";

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
                request_caused_failures: row.get(5)?,
                excluded_endpoint: row.get(6)?,
                excluded_model: row.get(7)?,
                excluded_target: row.get(8)?,
                excluded_probe_run: row.get(9)?,
                gap_rows: row.get(10)?,
                gap_dropped: row.get(11)?,
            })
        })?;
    Ok(totals)
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
