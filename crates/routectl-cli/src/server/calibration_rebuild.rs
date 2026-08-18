//! Startup warm of the per-lane token-estimate correction from the usage
//! ledger.
//!
//! On a fresh process start the router's calibration store is empty, so every
//! lane falls back to the uncorrected estimate until live traffic re-earns its
//! correction. Because that fallback IS the pre-correction behavior, the loss
//! reads as health rather than as breakage -- nothing errors, every lane
//! simply reports as not-yet-calibrated. This module bridges the leaf usage
//! crate to the router-side reader seam (which the router crate cannot satisfy
//! itself, since it does not depend on the usage crate) and runs a one-shot
//! rebuild at the serve bootstrap.
//!
//! The warm runs ONLY at the initial bootstrap, never on a hot-reload:
//! `Router::carry_over_calibration_from` already preserves the live in-memory
//! store across a reload, so re-running the rebuild there would clobber
//! fresher live samples with older ledger history.
//!
//! Like its sibling warms it runs the migrating open itself before its first
//! query -- the evidence columns exist only after the newest migration, and a
//! read-only open of a not-yet-migrated file is rejected outright on its schema
//! version. See the sibling `warm_open` module for why the writer's own open
//! cannot be relied on for that.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use routectl_router::{
    CalibrationLedgerReader, CalibrationLedgerRow, CalibrationRebuildSummary, Router,
};
use routectl_usage::{OpenError, open_readonly, read_calibration_samples_since};

use super::warm_open::migrate_before_warm;

/// Upper bound on the number of ledger rows the startup rebuild reads. A
/// plain compile-time cap (never derived from runtime input) on the boot read;
/// the per-lane ring bounds in-memory retention regardless of how many rows
/// come back.
const REBUILD_ROW_LIMIT: usize = 5000;

/// Bridges the leaf usage ledger to the router-side calibration reader seam.
///
/// Holds the resolved DB path and opens a fresh read-only connection per read.
/// The rebuild calls it exactly once at startup, so a per-call open is fine
/// and avoids holding a connection open for the daemon's whole life.
struct UsageCalibrationReader {
    db_path: PathBuf,
    loaded_rows: AtomicUsize,
}

impl UsageCalibrationReader {
    const fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            loaded_rows: AtomicUsize::new(0),
        }
    }

    /// Warn that a ledger read failed. `failure_class` separates an open
    /// failure (`"open"`) from a query failure (`"query"`) so a broken ledger
    /// is never mistaken downstream for a legitimately empty one (both
    /// otherwise yield zero rows). Rows still return EMPTY, never partial: a
    /// half-read replay would produce a factor from evidence the reducer never
    /// saw in full.
    fn warn_read_failure(&self, failure_class: &str, error: &dyn std::fmt::Display) {
        tracing::warn!(
            db_path = %self.db_path.display(),
            failure_class,
            error = %error,
            "usage ledger read failed during calibration startup warm; leaving lanes uncorrected"
        );
    }
}

impl CalibrationLedgerReader for UsageCalibrationReader {
    fn read_calibration_samples(
        &self,
        window_start: SystemTime,
        limit: usize,
    ) -> Vec<CalibrationLedgerRow> {
        let window_start_ms = window_start
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let db = match open_readonly(&self.db_path) {
            Ok(db) => db,
            // An absent file or a table-less DB is the legitimately-empty
            // ledger, not a failure: return no rows silently, matching the
            // cold-start path.
            Err(OpenError::NoData { .. }) => return Vec::new(),
            Err(e) => {
                self.warn_read_failure("open", &e);
                return Vec::new();
            }
        };

        match read_calibration_samples_since(db.conn(), window_start_ms, limit) {
            Ok(rows) => {
                self.loaded_rows.store(rows.len(), Ordering::Relaxed);
                rows.into_iter().map(evidence_row_to_ledger_row).collect()
            }
            Err(e) => {
                self.warn_read_failure("query", &e);
                Vec::new()
            }
        }
    }
}

/// Map one usage-local evidence row into the router-side row, clamping the
/// three theoretically-negative columns to a safe floor: a negative epoch-ms
/// timestamp becomes the UNIX epoch, and a negative token count becomes 0 --
/// which the store then refuses as a degenerate pair rather than admitting as
/// evidence.
fn evidence_row_to_ledger_row(row: routectl_usage::CalibrationSampleRow) -> CalibrationLedgerRow {
    CalibrationLedgerRow::new(
        UNIX_EPOCH + Duration::from_millis(row.ts_start_ms.max(0) as u64),
        row.provider_kind,
        row.model,
        row.session_id,
        row.estimated_tokens.max(0) as u64,
        row.prompt_tokens.max(0) as u64,
    )
}

/// One-shot warm of the router's calibration store from the usage ledger at
/// serve bootstrap.
///
/// Best-effort: a database that cannot be brought to the current schema, or
/// cannot be read, skips the rebuild and leaves every lane uncorrected --
/// which is exactly the pre-correction behavior, so it must never fail
/// bootstrap. Freshness is judged against the clock read here, so a lane whose
/// newest evidence is already past the reduction's age bound comes back
/// uncorrected.
///
/// Runs the migrating open synchronously first (see the sibling `warm_open`
/// module): the evidence columns exist only after the newest migration, and the
/// read-only open the query needs rejects an older schema outright.
///
/// Returns the tally it logged, so a caller (today only a test) can assert on
/// the outcome without a second read path into the private store.
pub(crate) fn warm_calibration_from_ledger(
    db_path: &Path,
    router: &Router,
) -> CalibrationRebuildSummary {
    if !migrate_before_warm(db_path, "calibration", "every lane stays uncorrected") {
        return CalibrationRebuildSummary::default();
    }
    let reader = UsageCalibrationReader::new(db_path.to_path_buf());
    let summary =
        router.rebuild_calibration_from_ledger(&reader, SystemTime::now(), REBUILD_ROW_LIMIT);
    emit_rebuild_log(&summary);
    summary
}

/// Report the rebuild outcome: an `info` with the per-verdict tally, plus a
/// one-shot `warn` when the load hit the row cap (the warm is then truncated
/// to the newest `REBUILD_ROW_LIMIT` rows, which a silent info line would
/// present as "we loaded everything").
fn emit_rebuild_log(summary: &CalibrationRebuildSummary) {
    if summary.rows_loaded == REBUILD_ROW_LIMIT {
        tracing::warn!(
            rows_loaded = summary.rows_loaded,
            row_cap = REBUILD_ROW_LIMIT,
            "calibration warm rebuild hit the row cap; warm state may be truncated"
        );
    }
    tracing::info!(
        rows_loaded = summary.rows_loaded,
        accepted = summary.accepted,
        rejected_unknown_nickname = summary.rejected_unknown_nickname,
        rejected_pair = summary.rejected_pair,
        lanes_calibrated = summary.lanes_calibrated,
        row_cap = REBUILD_ROW_LIMIT,
        "warmed per-lane token-estimate correction from usage ledger"
    );
}

#[cfg(test)]
#[path = "calibration_rebuild_tests.rs"]
mod tests;
