//! Tests for the schema step that creates `account_readings`.

/// The version before the step.
const PREVIOUS: i64 = SCHEMA_VERSION - 1;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value;

use super::*;
use crate::db::open;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn stamps(conn: &Connection) -> (i64, String) {
    let user_version = read_user_version(conn).expect("user_version");
    let meta = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [META_SCHEMA_VERSION],
            |r| r.get(0),
        )
        .expect("meta schema_version");
    (user_version, meta)
}

/// A file at the previous version holding `rows` request rows: a fresh file
/// with `account_readings` removed and both stamps lowered, which is exactly
/// what the previous version created.
fn previous_file_with_requests(path: &Path, rows: usize) -> Connection {
    drop(open(path).expect("open fresh"));
    let conn = Connection::open(path).expect("raw open");
    for i in 0..rows {
        conn.execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, msg_count, \
             attempt_count, fallback_count, input_tokens, cache_write_tokens) \
             VALUES (?1, ?1, ?2, 'anthropic', 'm', 'a', 0, 'ok', 5, 0, 1, 1, 0, ?1, ?1)",
            rusqlite::params![i64::try_from(i).expect("small"), format!("req-{i}")],
        )
        .expect("request row");
    }
    conn.execute_batch(&format!(
        "DROP TABLE account_readings;
         UPDATE meta SET value = '{PREVIOUS}' WHERE key = 'schema_version';
         PRAGMA user_version = {PREVIOUS};"
    ))
    .expect("stamp previous");
    conn
}

/// Every schema object `account_readings` owns, as `(type, name, sql)`. The
/// table's SQL carries its columns and CHECKs; the index's carries its key.
fn account_readings_objects(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    conn.prepare(
        "SELECT type, name, sql FROM sqlite_master \
         WHERE tbl_name = 'account_readings' ORDER BY type, name",
    )
    .expect("prepare")
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .expect("objects")
    .collect::<Result<_, _>>()
    .expect("rows")
}

/// Every `requests` row, every column, in rowid order.
fn requests_contents(conn: &Connection) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare("SELECT * FROM requests ORDER BY rowid")
        .expect("prepare");
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("contents")
}

#[test]
fn a_fresh_file_and_an_upgraded_previous_file_carry_the_same_account_readings_shape() {
    // Arrange
    let (_fresh_dir, fresh_path) = temp_db_path();
    let fresh = open(&fresh_path).expect("fresh");
    let (_dir, path) = temp_db_path();
    let conn = previous_file_with_requests(&path, 2);
    assert_eq!(account_readings_objects(&conn), vec![], "premise: absent");

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate");

    // Assert: same table (columns and CHECKs) and same index, both stamps
    // current.
    let objects = account_readings_objects(&conn);
    assert_eq!(objects, account_readings_objects(fresh.conn()));
    assert_eq!(
        objects
            .iter()
            .map(|(kind, name, _)| (kind.as_str(), name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("index", "idx_account_readings_ts_ms"),
            ("table", "account_readings")
        ]
    );
    assert_eq!(version, SCHEMA_VERSION);
    let current = (SCHEMA_VERSION, SCHEMA_VERSION.to_string());
    assert_eq!(stamps(&conn), current);
    assert_eq!(stamps(fresh.conn()), current);
    let impossible_window = "INSERT INTO account_readings (ts_ms, account, source, report, \
         window_secs, length_class, used, started) \
         VALUES (1, 'acct', 'poll', 'windows', -1, 'long', 2.0, 'no')";
    assert!(
        conn.execute(impossible_window, []).is_err(),
        "the upgraded table carries the window CHECKs"
    );
}

#[test]
fn the_upgrade_never_updates_or_deletes_a_request_row() {
    // Arrange: a previous-version ledger whose `requests` aborts any UPDATE or DELETE.
    let (_dir, path) = temp_db_path();
    let conn = previous_file_with_requests(&path, 50);
    conn.execute_batch(
        "CREATE TRIGGER no_request_update BEFORE UPDATE ON requests
             BEGIN SELECT RAISE(ABORT, 'requests updated'); END;
         CREATE TRIGGER no_request_delete BEFORE DELETE ON requests
             BEGIN SELECT RAISE(ABORT, 'requests deleted'); END;",
    )
    .expect("guard triggers");
    assert!(
        conn.execute("DELETE FROM requests WHERE rowid = 1", [])
            .is_err(),
        "control: the trigger aborts a delete"
    );
    let before = requests_contents(&conn);
    assert_eq!(before.len(), 50);

    // Act
    let version = migrate_to_current(&conn, 0).expect("migrate under the guard");

    // Assert
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(requests_contents(&conn), before);
}

#[test]
fn an_injected_failure_at_the_index_leaves_the_file_at_the_previous_version() {
    // Arrange: a previous-version file whose connection denies creating any index.
    let (_dir, path) = temp_db_path();
    let mut conn = previous_file_with_requests(&path, 1);
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::CreateIndex { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .expect("authorizer");

    // Act
    let result = migrate_to_current(&conn, 0);

    // Assert: refused, and neither the table nor a stamp survived.
    assert!(result.is_err(), "{result:?}");
    assert_eq!(account_readings_objects(&conn), vec![]);
    assert_eq!(stamps(&conn), (PREVIOUS, PREVIOUS.to_string()));

    // Assert: with the fault gone the same file migrates.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    assert_eq!(migrate_to_current(&conn, 0).expect("retry"), SCHEMA_VERSION);
    assert_eq!(account_readings_objects(&conn).len(), 2);
}
