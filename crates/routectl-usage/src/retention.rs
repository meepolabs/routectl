//! Startup retention prune.
//!
//! On daemon startup the writer runs a single bounded `DELETE` that drops
//! rows older than the configured retention window. The prune is
//! best-effort: a failure logs a WARN and bumps a counter but never
//! blocks serving or writing. A retention window of zero means "keep
//! everything" -- no delete runs at all.

use rusqlite::Connection;

/// Milliseconds in one day. Retention is configured in whole days.
const MS_PER_DAY: i64 = 86_400_000;

/// Outcome of a single startup prune attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneOutcome {
    /// Retention was zero -- nothing was deleted by design.
    Skipped,
    /// The delete ran and removed `deleted` rows (may be zero).
    Pruned { deleted: usize },
}

/// Delete rows whose `ts_start` predates the retention cutoff.
///
/// `retention_days == 0` is an explicit "keep everything" and returns
/// `Skipped` without touching the DB. Otherwise the cutoff is
/// `now_ms - retention_days * MS_PER_DAY` and every row strictly older
/// than the cutoff is removed. Returns the row count on success.
pub fn prune(
    conn: &Connection,
    retention_days: u32,
    now_ms: i64,
) -> Result<PruneOutcome, rusqlite::Error> {
    if retention_days == 0 {
        return Ok(PruneOutcome::Skipped);
    }
    let cutoff = now_ms.saturating_sub(i64::from(retention_days).saturating_mul(MS_PER_DAY));
    let deleted = conn.execute("DELETE FROM requests WHERE ts_start < ?1", [cutoff])?;
    Ok(PruneOutcome::Pruned { deleted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, crate::db::UsageDb) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        let db = open(&path).expect("open");
        (dir, db)
    }

    fn insert_row(conn: &Connection, request_id: &str, ts_start: i64) {
        conn.execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0)",
            rusqlite::params![ts_start, request_id],
        )
        .expect("insert row");
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn prune_deletes_old_keeps_new() {
        // Arrange
        let (_dir, db) = open_db();
        let now = 10 * MS_PER_DAY;
        insert_row(db.conn(), "old", now - 9 * MS_PER_DAY);
        insert_row(db.conn(), "new", now - MS_PER_DAY);

        // Act: keep 7 days -> cutoff is now - 7 days; "old" (9d) goes, "new" (1d) stays.
        let outcome = prune(db.conn(), 7, now).expect("prune");

        // Assert
        assert_eq!(outcome, PruneOutcome::Pruned { deleted: 1 });
        assert_eq!(count(db.conn()), 1);
        let survivor: String = db
            .conn()
            .query_row("SELECT request_id FROM requests", [], |r| r.get(0))
            .expect("survivor");
        assert_eq!(survivor, "new");
    }

    #[test]
    fn retention_zero_keeps_everything() {
        // Arrange
        let (_dir, db) = open_db();
        let now = 10 * MS_PER_DAY;
        insert_row(db.conn(), "ancient", 0);
        insert_row(db.conn(), "recent", now);

        // Act
        let outcome = prune(db.conn(), 0, now).expect("prune");

        // Assert
        assert_eq!(outcome, PruneOutcome::Skipped);
        assert_eq!(count(db.conn()), 2);
    }

    #[test]
    fn prune_on_empty_db_deletes_nothing() {
        // Arrange
        let (_dir, db) = open_db();

        // Act
        let outcome = prune(db.conn(), 30, 1_000_000_000_000).expect("prune");

        // Assert
        assert_eq!(outcome, PruneOutcome::Pruned { deleted: 0 });
    }
}
