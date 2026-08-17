//! Tests for the cache-breakpoint decision read query.

use super::*;
use crate::db::open;
use tempfile::TempDir;

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, db)
}

/// Insert a row with explicit v16 decision columns. Each is `Option` so the
/// NULL-vs-decided accounting is exercisable.
fn insert_decision_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    front: Option<&str>,
    terminal: Option<&str>,
    epoch: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, cache_front_decision, \
             cache_terminal_decision, prefix_epoch_event) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3, ?4, ?5)",
            rusqlite::params![ts_start, request_id, front, terminal, epoch],
        )
        .expect("insert decision row");
}

#[test]
fn counts_decided_and_emitted_per_region_ignoring_undecided_rows() {
    // Arrange: an emitted pair, a declined pair, and an undecided (pre-v16
    // shaped) row that must land in neither count.
    let (_dir, db) = open_db();
    insert_decision_row(
        &db,
        "d1",
        100,
        Some("auto_emitted"),
        Some("auto_emitted"),
        None,
    );
    insert_decision_row(
        &db,
        "d2",
        110,
        Some("auto_emitted"),
        Some("auto_skipped:breakpoint_cap"),
        None,
    );
    insert_decision_row(&db, "d3", 120, None, None, None);

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert: COUNT ignores the NULL row; the two regions are counted apart.
    assert_eq!(s.front_decided, 2);
    assert_eq!(s.front_emitted, 2);
    assert_eq!(s.terminal_decided, 2);
    assert_eq!(s.terminal_emitted, 1);
}

#[test]
fn partitions_the_prefix_epoch_events_and_ignores_unclassified_rows() {
    // Arrange: one of each event value plus an unclassified (NULL) row.
    let (_dir, db) = open_db();
    insert_decision_row(&db, "e0", 100, None, None, Some(PREFIX_EPOCH_STABLE));
    insert_decision_row(&db, "e1", 110, None, None, Some(PREFIX_EPOCH_REWRITTEN));
    insert_decision_row(&db, "e2", 120, None, None, Some(PREFIX_EPOCH_RESEEDED));
    insert_decision_row(&db, "e3", 130, None, None, Some(PREFIX_EPOCH_STABLE));
    insert_decision_row(&db, "unclassified", 140, None, None, None);

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(
        s.epoch_classified, 4,
        "the row with no comparable prior prefix is excluded"
    );
    assert_eq!(s.epoch_stable, 2);
    assert_eq!(s.epoch_rewritten, 1);
    assert_eq!(s.epoch_reseeded, 1);
}

#[test]
fn restricts_to_the_requested_window() {
    // Arrange: one in-window row and one before it.
    let (_dir, db) = open_db();
    insert_decision_row(
        &db,
        "inside",
        100,
        Some("auto_emitted"),
        Some("auto_emitted"),
        Some(PREFIX_EPOCH_REWRITTEN),
    );
    insert_decision_row(
        &db,
        "before",
        5,
        Some("auto_emitted"),
        Some("auto_emitted"),
        Some(PREFIX_EPOCH_REWRITTEN),
    );

    // Act
    let s = cache_decision_summary(&db, 100, 1000).expect("summary");

    // Assert
    assert_eq!(s.front_decided, 1);
    assert_eq!(s.terminal_decided, 1);
    assert_eq!(s.epoch_rewritten, 1);
}

#[test]
fn on_an_empty_ledger_returns_all_zeros() {
    // Arrange
    let (_dir, db) = open_db();

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s, CacheDecisionSummary::default());
}
