//! Tests for the v21 -> v22 read-index step.

use super::*;
use crate::db::open;
use crate::record::UsageRecord;
use crate::writer::UsageWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The three index names v22 creates.
const V22_INDEX_NAMES: [&str; 3] = [
    "idx_requests_agg",
    "idx_requests_errors",
    "idx_requests_quota_seat",
];

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

/// `(name, sql)` of every v22 index present on the file, in name order.
fn v22_indexes(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT name, sql FROM sqlite_master \
             WHERE type = 'index' AND name IN (?1, ?2, ?3) ORDER BY name",
        )
        .expect("prepare");
    stmt.query_map(V22_INDEX_NAMES, |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

fn stamps(conn: &Connection) -> (i64, String) {
    let version = read_user_version(conn).expect("user_version");
    let meta: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [META_SCHEMA_VERSION],
            |r| r.get(0),
        )
        .expect("meta schema_version");
    (version, meta)
}

fn request_ids(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT request_id FROM requests ORDER BY request_id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// A genuine v21 file with two rows: a current file with the v22 indexes
/// dropped and both stamps set to 21. v22 changed nothing else, so this is
/// exactly what a v21 binary leaves behind.
fn v21_file(path: &Path) {
    let db = open(path).expect("open fresh");
    let conn = db.conn();
    for id in ["v21-row-a", "v21-row-b"] {
        conn.execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, msg_count, \
             attempt_count, fallback_count) \
             VALUES (1, 2, ?1, 'anthropic', 'm', 'a', 0, 'ok', 5, 0, 1, 1, 0)",
            [id],
        )
        .expect("v21 row");
    }
    for name in V22_INDEX_NAMES {
        conn.execute_batch(&format!("DROP INDEX {name}"))
            .expect("drop v22 index");
    }
    conn.execute_batch(
        "UPDATE meta SET value = '21' WHERE key = 'schema_version';
         PRAGMA user_version = 21;",
    )
    .expect("stamp v21");
    assert!(v22_indexes(conn).is_empty(), "the fixture has no v22 index");
}

fn wait_persisted(writer: &UsageWriter, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.counters().persisted() < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writer.counters().persisted(), want, "rows persisted");
}

#[test]
fn a_fresh_file_is_stamped_22_with_all_three_read_indexes() {
    // Arrange + Act
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open fresh");

    // Assert
    assert_eq!(SCHEMA_VERSION, 22);
    assert_eq!(stamps(db.conn()), (22, "22".to_string()));
    let names: Vec<String> = v22_indexes(db.conn()).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, V22_INDEX_NAMES);
}

#[test]
fn the_writer_migrates_a_v21_ledger_to_the_fresh_v22_state_with_rows_intact() {
    // Arrange
    let (_dir, path) = temp_db_path();
    v21_file(&path);
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh = v22_indexes(open(&fresh_path).expect("fresh").conn());

    // Act: the production writer opens (and migrates) the file on its own
    // thread, then writes one row.
    let (handle, writer) = UsageWriter::start(path.clone(), 4, 0);
    handle.try_send(UsageRecord {
        request_id: "v22-row".to_string(),
        ..UsageRecord::default()
    });
    wait_persisted(&writer, 1);
    drop(handle);
    writer.shutdown();
    let db = open(&path).expect("reopen at v22");

    // Assert: both stamps read 22, the three indexes match a fresh file's,
    // and the v21 rows survive beside the new one.
    assert_eq!(stamps(db.conn()), (22, "22".to_string()));
    assert_eq!(v22_indexes(db.conn()), fresh);
    assert_eq!(fresh.len(), 3, "a fresh file has all three indexes");
    assert_eq!(
        request_ids(db.conn()),
        ["v21-row-a", "v21-row-b", "v22-row"]
    );

    // Assert: re-running the ladder on a v22 file is a no-op.
    assert_eq!(migrate_to_current(db.conn(), 0).expect("re-run"), 22);
    assert_eq!(v22_indexes(db.conn()), fresh);
}

#[test]
fn an_injected_failure_on_the_second_v22_index_rolls_back_every_index_and_both_stamps() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Arrange: a v21 file whose connection allows the first index build and
    // denies every later one.
    let (_dir, path) = temp_db_path();
    v21_file(&path);
    let mut conn = Connection::open(&path).expect("raw open");
    let creates = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&creates);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
        AuthAction::CreateIndex {
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

    // Assert: the step failed at the second index, and nothing it did
    // survived: not the first index, not either stamp, not a row.
    assert!(
        result.is_err(),
        "the injected failure must surface: {result:?}"
    );
    assert_eq!(creates.load(Ordering::SeqCst), 2, "failed at the second");
    assert_eq!(stamps(&conn), (21, "21".to_string()));
    assert!(v22_indexes(&conn).is_empty());
    assert_eq!(request_ids(&conn), ["v21-row-a", "v21-row-b"]);

    // Assert: once the fault is gone the same file migrates normally.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    assert_eq!(migrate_to_current(&conn, 0).expect("retry"), 22);
    assert_eq!(v22_indexes(&conn).len(), 3);
}
