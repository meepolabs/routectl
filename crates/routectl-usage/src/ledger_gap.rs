//! Persisting queue-full request drops as `ledger_gaps` rows.
//!
//! The producer never touches the DB: `try_send` only bumps the in-memory
//! `dropped_full` counter. The writer thread compares that counter with the
//! total it has already persisted and appends one row carrying the
//! difference, so a window whose rows were lost to a full queue stays
//! visibly incomplete after a restart.

use rusqlite::Connection;

/// `ledger_gaps.cause` for request rows dropped because the writer channel
/// was full.
pub const QUEUE_FULL_CAUSE: &str = "queue_full";

/// The writer's in-memory view of how many queue-full drops it has already
/// recorded in `ledger_gaps` during this process.
///
/// Starts at zero rather than at the counter's value when the writer began:
/// any drop counted before the writer's first check is still a lost row.
#[derive(Debug, Default)]
pub struct GapTracker {
    persisted_full: u64,
}

impl GapTracker {
    /// Append one `queue_full` gap row for the drops counted since the last
    /// persisted row, if there are any. Returns the number of drops the new
    /// row carries, or `None` when there was nothing to record.
    ///
    /// The persisted total advances only after the INSERT succeeds, so a
    /// failed write is retried (with the full delta) on the next check.
    pub fn persist_new_drops(
        &mut self,
        conn: &Connection,
        dropped_full: u64,
        now_ms: i64,
    ) -> Result<Option<u64>, rusqlite::Error> {
        let delta = dropped_full.saturating_sub(self.persisted_full);
        if delta == 0 {
            return Ok(None);
        }
        conn.execute(
            "INSERT INTO ledger_gaps (ts_ms, cause, dropped) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                now_ms,
                QUEUE_FULL_CAUSE,
                i64::try_from(delta).unwrap_or(i64::MAX)
            ],
        )?;
        self.persisted_full = dropped_full;
        Ok(Some(delta))
    }
}

#[cfg(test)]
#[path = "ledger_gap_tests.rs"]
mod tests;
