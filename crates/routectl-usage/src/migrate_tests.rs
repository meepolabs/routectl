//! Tests for the v20 -> v21 cache-write step and the one-shot legacy
//! capability-observation purge.

use super::*;
use crate::db::open;
use crate::downgrade::downgrade_to;
use crate::record::UsageRecord;
use crate::schema::{
    CREATE_CAPABILITY_EVENTS_TABLE, CREATE_CAPABILITY_EVENTS_TS_INDEX,
    CREATE_CAPABILITY_LEARN_EVENTS_TABLE, CREATE_LEDGER_GAPS_TABLE, CREATE_LEDGER_GAPS_TS_INDEX,
    CREATE_META_TABLE, CREATE_REQUESTS_TABLE, CREATE_TS_START_INDEX,
};
use crate::writer::UsageWriter;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
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
         VALUES (1, 'lane', 'tools', 'supported', 'observed', 'live', 'inferred', 1, 1, ?1)",
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
    let downgraded = downgrade_to(&path, 16);

    // Assert
    assert_eq!(version, SCHEMA_VERSION);
    assert!(downgraded.is_ok(), "downgrade refused: {downgraded:?}");
}

/// Open a second connection that takes the write lock and runs `statements`
/// uncommitted, then commit from a thread after a delay. The thread returns
/// the instant taken just before COMMIT, while the lock is still held.
fn rival_writer_committing_later(path: &Path, statements: &str) -> JoinHandle<Instant> {
    let rival = Connection::open(path).expect("rival open");
    rival
        .execute_batch(&format!("BEGIN IMMEDIATE; {statements}"))
        .expect("rival takes the write lock");
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let committed_at = Instant::now();
        rival.execute_batch("COMMIT").expect("rival commit");
        committed_at
    })
}

const PREMISE: &str = "premise: the purge must start while the rival still holds the lock";
const WAITED: &str = "the purge must not return before the rival commits";

#[test]
fn purge_waits_for_a_concurrent_writer_instead_of_failing() {
    // Arrange: a second connection holds the write lock with an uncommitted
    // insert and commits it shortly after the purge starts.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let expected_deleted = seed_mixed_ledger(db.conn());
    let committer =
        rival_writer_committing_later(&path, "INSERT INTO meta (key, value) VALUES ('t', 'x');");

    // Act
    let started = Instant::now();
    let outcome = purge_legacy_capability_observations(db.conn());
    let returned_at = Instant::now();
    let committed_at = committer.join().expect("committer thread");

    // Assert
    assert!(started < committed_at, "{PREMISE}");
    assert!(returned_at >= committed_at, "{WAITED}");
    assert_eq!(
        outcome.expect("purge must wait out the rival writer"),
        Some(usize::try_from(expected_deleted).expect("count"))
    );
}

#[test]
fn purge_skips_when_a_concurrent_writer_records_the_marker_first() {
    // Arrange: the marker is absent at the purge's plain read, but a second
    // connection holding the write lock inserts it and commits while the
    // purge waits for that lock.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_mixed_ledger(db.conn());
    let before = class_counts(db.conn());
    let committer = rival_writer_committing_later(
        &path,
        &format!(
            "INSERT INTO meta (key, value) VALUES ('{META_LEGACY_CAPABILITY_PURGE}', 'rival');"
        ),
    );

    // Act
    let started = Instant::now();
    let outcome = purge_legacy_capability_observations(db.conn());
    let returned_at = Instant::now();
    let committed_at = committer.join().expect("committer thread");

    // Assert
    assert!(started < committed_at, "{PREMISE}");
    assert!(returned_at >= committed_at, "{WAITED}");
    assert_eq!(outcome.expect("purge must see the rival's marker"), None);
    assert_eq!(class_counts(db.conn()), before);
    assert_eq!(marker_value(db.conn()), Some("rival".to_string()));
}

#[test]
fn a_failed_marker_insert_rolls_back_the_deletion() {
    // Arrange: a trigger makes the marker insert abort after the delete ran.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_mixed_ledger(db.conn());
    let before = class_counts(db.conn());
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_marker BEFORE INSERT ON meta \
             BEGIN SELECT RAISE(ABORT, 'x'); END;",
        )
        .expect("create trigger");

    // Act
    let outcome = purge_legacy_capability_observations(db.conn());

    // Assert
    assert!(
        outcome.is_err(),
        "the aborted insert must surface: {outcome:?}"
    );
    db.conn()
        .execute_batch("DROP TRIGGER fail_marker;")
        .expect("drop trigger");
    assert_eq!(class_counts(db.conn()), before);
    assert_eq!(marker_value(db.conn()), None);
}

#[test]
fn a_recorded_purge_returns_without_taking_the_write_lock() {
    // Arrange: the purge already ran, and a second connection holds the write
    // lock. A zero busy timeout turns any write-lock attempt into an error.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_mixed_ledger(db.conn());
    purge_legacy_capability_observations(db.conn()).expect("first purge");
    let rival = Connection::open(&path).expect("rival open");
    rival
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("rival takes the write lock");
    db.conn()
        .busy_timeout(std::time::Duration::from_millis(0))
        .expect("busy timeout");

    // Act
    let outcome = purge_legacy_capability_observations(db.conn());

    // Assert
    assert_eq!(outcome.expect("a recorded purge must not contend"), None);
    rival.execute_batch("ROLLBACK;").expect("rival rollback");
}

/// The `requests` DDL as v20 created it: the current DDL cut after its last
/// v20 column, so the v21 pair is absent.
fn requests_ddl_v20() -> String {
    const LAST_V20_COLUMN: &str = "    excluded_probe_run INTEGER,";
    let (head, tail) = CREATE_REQUESTS_TABLE
        .split_once(LAST_V20_COLUMN)
        .expect("the DDL still ends its v20 set with excluded_probe_run");
    for (column, _) in V21_REQUESTS_COLUMNS {
        assert!(tail.contains(column), "{column} must be in the v21 tail");
    }
    format!("{head}    excluded_probe_run INTEGER\n)")
}

/// A WAL file with every v20 table and index, stamped 20 in both places.
fn v20_file(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("raw open");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    for ddl in [
        requests_ddl_v20().as_str(),
        CREATE_TS_START_INDEX,
        CREATE_CAPABILITY_LEARN_EVENTS_TABLE,
        CREATE_CAPABILITY_EVENTS_TABLE,
        CREATE_CAPABILITY_EVENTS_TS_INDEX,
        CREATE_LEDGER_GAPS_TABLE,
        CREATE_LEDGER_GAPS_TS_INDEX,
        CREATE_META_TABLE,
    ] {
        conn.execute_batch(ddl).expect("v20 ddl");
    }
    conn.execute_batch(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '20');
         PRAGMA user_version = 20;",
    )
    .expect("stamp v20");
    conn
}

/// Insert a row as a v20 writer would: naming no v21 column, with a legacy
/// 5-minute write.
fn insert_as_v20_writer(conn: &Connection, request_id: &str, cache_write_5m: i64) {
    conn.execute(
        "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
         requested_model, alias, stream, outcome, latency_ms, tool_count, msg_count, \
         attempt_count, fallback_count, cache_write_5m, excluded_model) \
         VALUES (1, 2, ?1, 'anthropic', 'm', 'a', 0, 'ok', 5, 0, 1, 1, 0, ?2, 0)",
        rusqlite::params![request_id, cache_write_5m],
    )
    .expect("v20 insert");
}

fn requests_columns(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('requests')")
        .expect("prepare table_info");
    stmt.query_map([], |r| r.get(0))
        .expect("query table_info")
        .collect::<Result<_, _>>()
        .expect("names")
}

fn meta_version(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [META_SCHEMA_VERSION],
        |r| r.get(0),
    )
    .expect("meta schema_version")
}

/// `(cache_write_5m, cache_write_tokens, cache_write_lifetimes)` of one row.
type CacheWriteRow = (Option<i64>, Option<i64>, Option<String>);

fn cache_write_row(conn: &Connection, request_id: &str) -> CacheWriteRow {
    conn.query_row(
        "SELECT cache_write_5m, cache_write_tokens, cache_write_lifetimes \
         FROM requests WHERE request_id = ?1",
        [request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .expect("cache write row")
}

fn wait_persisted(writer: &UsageWriter, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.counters().persisted() < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writer.counters().persisted(), want, "rows persisted");
}

#[test]
fn the_fresh_schema_ends_with_the_two_cache_write_columns() {
    // Arrange + Act
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open fresh");

    // Assert
    assert_eq!(SCHEMA_VERSION, 21);
    assert_eq!(user_version(&path), 21);
    assert_eq!(meta_version(db.conn()), "21");
    let columns = requests_columns(db.conn());
    let tail: Vec<&str> = V21_REQUESTS_COLUMNS.iter().map(|(name, _)| *name).collect();
    assert_eq!(&columns[columns.len() - tail.len()..], tail.as_slice());
}

#[test]
fn a_seeded_v20_ledger_migrates_to_v21_with_null_cache_write_columns_on_old_rows() {
    // Arrange: a genuine v20 file holding two rows a v20 writer wrote.
    let (_dir, path) = temp_db_path();
    {
        let conn = v20_file(&path);
        insert_as_v20_writer(&conn, "v20-row-a", 11);
        insert_as_v20_writer(&conn, "v20-row-b", 0);
    }
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh_columns = requests_columns(open(&fresh_path).expect("fresh").conn());

    // Act: the production writer opens (and migrates) the file on its own
    // thread, then writes a row carrying a split.
    let (handle, writer) = UsageWriter::start(path.clone(), 4, 0, true);
    handle.try_send(UsageRecord {
        request_id: "v21-row".to_string(),
        cache_write_tokens: Some(30),
        cache_write_lifetimes: Some(BTreeMap::from([(300, 30)])),
        ..UsageRecord::default()
    });
    wait_persisted(&writer, 1);
    drop(handle);
    writer.shutdown();
    let db = open(&path).expect("reopen at v21");

    // Assert: both stamps read 21, the fresh column list, the old rows keep
    // their legacy write and read NULL in both new columns.
    assert_eq!(user_version(&path), 21);
    assert_eq!(meta_version(db.conn()), "21");
    assert_eq!(requests_columns(db.conn()), fresh_columns);
    assert_eq!(
        cache_write_row(db.conn(), "v20-row-a"),
        (Some(11), None, None)
    );
    assert_eq!(
        cache_write_row(db.conn(), "v20-row-b"),
        (Some(0), None, None)
    );
    assert_eq!(
        cache_write_row(db.conn(), "v21-row"),
        (None, Some(30), Some(r#"{"300":30}"#.to_string()))
    );
    drop(db);

    // Assert: reopening a v21 file changes neither shape nor rows.
    let again = open(&path).expect("reopen at v21");
    assert_eq!(migrate_to_current(again.conn(), 0).expect("re-run"), 21);
    assert_eq!(user_version(&path), 21);
    assert_eq!(requests_columns(again.conn()), fresh_columns);
    assert_eq!(
        cache_write_row(again.conn(), "v20-row-a"),
        (Some(11), None, None)
    );
    let rows: i64 = again
        .conn()
        .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 3);
}

#[test]
fn an_injected_failure_after_the_first_v21_alter_rolls_back_the_columns_and_both_stamps() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Arrange: a v20 file whose connection allows the first `ALTER TABLE
    // requests` and denies every later one.
    let (_dir, path) = temp_db_path();
    let conn = v20_file(&path);
    let columns_before = requests_columns(&conn);
    let alters = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&alters);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
        AuthAction::AlterTable {
            table_name: "requests",
            ..
        } => {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Allow,
    }))
    .expect("install authorizer");

    // Act
    let result = migrate_to_current(&conn, 0);

    // Assert: the step failed at the second ALTER, and nothing it did
    // survived: not the first added column, not either version stamp.
    assert!(
        result.is_err(),
        "the injected failure must surface: {result:?}"
    );
    assert_eq!(
        alters.load(Ordering::SeqCst),
        2,
        "failed at the second ALTER"
    );
    assert_eq!(user_version(&path), 20);
    assert_eq!(meta_version(&conn), "20");
    assert_eq!(requests_columns(&conn), columns_before);

    // Assert: once the fault is gone the same file migrates normally.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    assert_eq!(migrate_to_current(&conn, 0).expect("retry"), 21);
    let columns = requests_columns(&conn);
    assert_eq!(
        &columns[columns.len() - 2..],
        ["cache_write_tokens", "cache_write_lifetimes"]
    );
}
