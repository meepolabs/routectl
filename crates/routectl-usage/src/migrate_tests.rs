//! Tests for the one-shot legacy capability-observation purge.

use super::*;
use crate::db::open;
use crate::downgrade::downgrade_to_v16;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The vocabulary version stamped on non-legacy fixture rows.
const CURRENT_VOCAB: i64 = 2;

/// Row classes the purge must tell apart. Counted per class, so a test sees
/// both what was deleted and what survived.
#[derive(Debug, PartialEq, Eq)]
struct ClassCounts {
    tombstones: i64,
    versioned: i64,
    legacy_before_boundary: i64,
    legacy_after_boundary: i64,
}

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn insert_observation(conn: &Connection, vocab_version: Option<i64>) -> i64 {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, catalog_version, overlay_revision, vocab_version) \
         VALUES (1, 'lane', 'tools', 'supported', 'observed', 'live', 'paid', 1, 1, ?1)",
        rusqlite::params![vocab_version],
    )
    .expect("insert observation");
    conn.last_insert_rowid()
}

fn insert_tombstone(conn: &Connection) -> i64 {
    conn.execute(
        "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
         tier, catalog_version, overlay_revision, vocab_version) \
         VALUES (1, '', '', ?1, '', '', '', 1, 1, NULL)",
        rusqlite::params![TOMBSTONE_VERDICT],
    )
    .expect("insert tombstone");
    conn.last_insert_rowid()
}

/// Seed a ledger with legacy observations on both sides of the latest
/// tombstone, two legacy (NULL-vocabulary) tombstones, and versioned rows on
/// both sides of the boundary. Returns the number of legacy observations that
/// precede the latest tombstone.
fn seed_mixed_ledger(conn: &Connection) -> i64 {
    insert_observation(conn, None);
    insert_observation(conn, Some(CURRENT_VOCAB));
    insert_tombstone(conn);
    insert_observation(conn, None);
    insert_observation(conn, Some(CURRENT_VOCAB));
    insert_observation(conn, None);
    insert_tombstone(conn);
    insert_observation(conn, None);
    insert_observation(conn, Some(CURRENT_VOCAB));
    3
}

fn class_counts(conn: &Connection) -> ClassCounts {
    let boundary: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM capability_events WHERE verdict = ?1",
            rusqlite::params![TOMBSTONE_VERDICT],
            |r| r.get(0),
        )
        .expect("boundary");
    let count = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> i64 {
        conn.query_row(sql, params, |r| r.get(0)).expect("count")
    };
    ClassCounts {
        tombstones: count(
            "SELECT COUNT(*) FROM capability_events WHERE verdict = ?1",
            &[&TOMBSTONE_VERDICT],
        ),
        versioned: count(
            "SELECT COUNT(*) FROM capability_events WHERE vocab_version IS NOT NULL",
            &[],
        ),
        legacy_before_boundary: count(
            "SELECT COUNT(*) FROM capability_events \
             WHERE vocab_version IS NULL AND verdict <> ?1 AND rowid < ?2",
            &[&TOMBSTONE_VERDICT, &boundary],
        ),
        legacy_after_boundary: count(
            "SELECT COUNT(*) FROM capability_events \
             WHERE vocab_version IS NULL AND verdict <> ?1 AND rowid > ?2",
            &[&TOMBSTONE_VERDICT, &boundary],
        ),
    }
}

fn marker_value(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![META_LEGACY_CAPABILITY_PURGE],
        |r| r.get(0),
    )
    .optional()
    .expect("marker read")
}

fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).expect("reopen");
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

#[test]
fn purge_deletes_only_legacy_observations_before_the_latest_tombstone() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let expected_deleted = seed_mixed_ledger(db.conn());
    let before = class_counts(db.conn());

    // Act
    let outcome = purge_legacy_capability_observations(db.conn()).expect("purge");

    // Assert
    let after = class_counts(db.conn());
    assert_eq!(
        outcome,
        Some(usize::try_from(expected_deleted).expect("count"))
    );
    assert_eq!(before.legacy_before_boundary, expected_deleted);
    assert_eq!(
        after,
        ClassCounts {
            tombstones: before.tombstones,
            versioned: before.versioned,
            legacy_before_boundary: 0,
            legacy_after_boundary: before.legacy_after_boundary,
        }
    );
    assert_eq!(
        (
            before.tombstones,
            before.versioned,
            before.legacy_after_boundary
        ),
        (2, 3, 1),
        "the fixture must carry every surviving class"
    );
}

#[test]
fn purge_marker_records_the_deleted_count() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let expected_deleted = seed_mixed_ledger(db.conn());

    // Act
    purge_legacy_capability_observations(db.conn()).expect("purge");

    // Assert
    assert_eq!(marker_value(db.conn()), Some(expected_deleted.to_string()));
}

#[test]
fn purge_runs_once_even_when_new_legacy_rows_fall_behind_a_new_boundary() {
    // Arrange: a first run, then a legacy observation that a fresh tombstone
    // places before the boundary.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_mixed_ledger(db.conn());
    purge_legacy_capability_observations(db.conn()).expect("first purge");
    let late_row = insert_observation(db.conn(), None);
    insert_tombstone(db.conn());

    // Act
    let second = purge_legacy_capability_observations(db.conn()).expect("second purge");

    // Assert
    assert_eq!(second, None);
    let survives: bool = db
        .conn()
        .query_row(
            "SELECT COUNT(*) = 1 FROM capability_events WHERE rowid = ?1",
            [late_row],
            |r| r.get(0),
        )
        .expect("late row lookup");
    assert!(survives, "a second call must not delete anything");
}

#[test]
fn purge_without_a_tombstone_deletes_nothing_and_records_zero() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_observation(db.conn(), None);
    insert_observation(db.conn(), None);
    insert_observation(db.conn(), Some(CURRENT_VOCAB));

    // Act
    let outcome = purge_legacy_capability_observations(db.conn()).expect("purge");

    // Assert
    assert_eq!(outcome, Some(0));
    assert_eq!(marker_value(db.conn()), Some("0".to_string()));
    let remaining: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM capability_events", [], |r| r.get(0))
        .expect("count");
    assert_eq!(remaining, 3);
}

#[test]
fn purge_keeps_the_schema_version_and_the_file_still_downgrades() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_mixed_ledger(db.conn());
    purge_legacy_capability_observations(db.conn()).expect("purge");
    drop(db);

    // Act
    let version = user_version(&path);
    let downgraded = downgrade_to_v16(&path);

    // Assert
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(version, 17);
    assert!(downgraded.is_ok(), "downgrade refused: {downgraded:?}");
}
