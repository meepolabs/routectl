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

/// Delete `capability_events` rows older than the retention cutoff, never
/// crossing the correctness boundary.
///
/// The tombstone is the boundary the warm-rebuild replayer trusts, so this
/// hygiene prune must never drop it or any row after it. With a tombstone
/// present, only rows whose `ts` predates the cutoff AND whose rowid is
/// strictly below the latest tombstone's rowid are removed -- the tombstone
/// itself and every later event survive regardless of age. With no
/// tombstone the ledger has no boundary to protect, so it age-prunes as
/// normal (every row older than the cutoff). `retention_days == 0` is an
/// explicit "keep everything" and returns `Skipped`.
pub fn prune_capability_events(
    conn: &Connection,
    retention_days: u32,
    now_ms: i64,
) -> Result<PruneOutcome, rusqlite::Error> {
    if retention_days == 0 {
        return Ok(PruneOutcome::Skipped);
    }
    let cutoff = now_ms.saturating_sub(i64::from(retention_days).saturating_mul(MS_PER_DAY));
    let deleted = match crate::query::latest_tombstone(conn)? {
        Some(tombstone) => conn.execute(
            "DELETE FROM capability_events WHERE ts < ?1 AND rowid < ?2",
            rusqlite::params![cutoff, tombstone.rowid],
        )?,
        None => conn.execute("DELETE FROM capability_events WHERE ts < ?1", [cutoff])?,
    };
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

    use crate::capability_event::{CapabilityEvent, insert_capability_event};

    fn insert_event(conn: &Connection, ts: i64, capability: &str) {
        insert_capability_event(
            conn,
            &CapabilityEvent {
                ts,
                lane_key: "gpt-nick".to_string(),
                capability: capability.to_string(),
                verdict: "broken".to_string(),
                phase: "f1".to_string(),
                source: "live".to_string(),
                tier: "inferred".to_string(),
                evidence_class: None,
                upstream_token: None,
                catalog_version: 1,
                overlay_revision: 1,
            },
        )
        .expect("insert event");
    }

    fn event_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn capability_prune_never_crosses_the_tombstone() {
        // Arrange: an old pre-tombstone event, an old tombstone, then a
        // pre-tombstone-aged-but-post-tombstone event and a fresh event.
        let (_dir, db) = open_db();
        let now = 100 * MS_PER_DAY;
        insert_event(db.conn(), 10 * MS_PER_DAY, "before"); // rowid 1, old, pre-tombstone
        insert_capability_event(
            db.conn(),
            &CapabilityEvent::tombstone(20 * MS_PER_DAY, 1, 1),
        )
        .expect("tombstone"); // rowid 2, old ts
        insert_event(db.conn(), 30 * MS_PER_DAY, "after-old"); // rowid 3, old ts but post-tombstone
        insert_event(db.conn(), 90 * MS_PER_DAY, "after-new"); // rowid 4, fresh

        // Act: 30-day retention -> cutoff at 70 days; everything below is old.
        let outcome = prune_capability_events(db.conn(), 30, now).expect("prune");

        // Assert: only the pre-tombstone old row is dropped. The tombstone
        // and every row after it survive regardless of age.
        assert_eq!(outcome, PruneOutcome::Pruned { deleted: 1 });
        assert_eq!(event_count(db.conn()), 3);
        let survivors: Vec<String> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT verdict FROM capability_events ORDER BY rowid ASC")
                .expect("prepare");
            stmt.query_map([], |r| r.get::<_, String>(0))
                .expect("query")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect")
        };
        assert_eq!(survivors, vec!["tombstone", "broken", "broken"]);
    }

    #[test]
    fn capability_prune_age_prunes_when_no_tombstone() {
        // Arrange: no tombstone -> ordinary age prune.
        let (_dir, db) = open_db();
        let now = 100 * MS_PER_DAY;
        insert_event(db.conn(), 10 * MS_PER_DAY, "old");
        insert_event(db.conn(), 90 * MS_PER_DAY, "new");

        // Act
        let outcome = prune_capability_events(db.conn(), 30, now).expect("prune");

        // Assert: the old row goes, the fresh row stays.
        assert_eq!(outcome, PruneOutcome::Pruned { deleted: 1 });
        assert_eq!(event_count(db.conn()), 1);
        let survivor: String = db
            .conn()
            .query_row("SELECT capability FROM capability_events", [], |r| r.get(0))
            .expect("survivor");
        assert_eq!(survivor, "new");
    }

    #[test]
    fn capability_prune_zero_retention_keeps_everything() {
        // Arrange
        let (_dir, db) = open_db();
        insert_event(db.conn(), 0, "ancient");
        insert_event(db.conn(), 10 * MS_PER_DAY, "recent");

        // Act
        let outcome = prune_capability_events(db.conn(), 0, 100 * MS_PER_DAY).expect("prune");

        // Assert
        assert_eq!(outcome, PruneOutcome::Skipped);
        assert_eq!(event_count(db.conn()), 2);
    }
}
