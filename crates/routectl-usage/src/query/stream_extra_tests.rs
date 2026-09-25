//! Tests for the raw streaming-row `extra` reader: which rows are in the
//! window, and that every `extra` value comes back rather than being
//! dropped.

use super::*;
use crate::db::{open, open_readonly};
use tempfile::TempDir;

fn open_db() -> (TempDir, std::path::PathBuf, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, path, db)
}

fn insert(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    dialect: &str,
    stream: i64,
    extra: rusqlite::types::Value,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, upstream, stream, \
             outcome, latency_ms, tool_count, msg_count, attempt_count, fallback_count, extra) \
             VALUES (?1, ?1, ?2, ?3, 'req', 'al', 'opus', 'anth', 'anthropic-api', \
             'claude-opus-4-7', ?4, 'ok', 5, 0, 1, 1, 0, ?5)",
            rusqlite::params![ts_start, request_id, dialect, stream, extra],
        )
        .expect("insert row");
}

fn text(s: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(s.to_string())
}

fn visit_all(db: &UsageDb, dialect: &str, from: i64, to: i64) -> (u64, Vec<StreamExtraRow>) {
    let mut rows = Vec::new();
    let n = for_each_stream_extra(db, dialect, from, to, |row| rows.push(row)).expect("read");
    (n, rows)
}

#[test]
fn visits_only_the_dialects_streaming_rows_inside_the_half_open_window() {
    // Arrange: edges on both sides, a non-stream row and another dialect.
    let (_dir, _path, db) = open_db();
    insert(&db, "before", 99, "anthropic", 1, text("{}"));
    insert(&db, "first", 100, "anthropic", 1, text("{}"));
    insert(&db, "unary", 150, "anthropic", 0, text("{}"));
    insert(&db, "openai", 150, "openai", 1, text("{}"));
    insert(&db, "last", 199, "anthropic", 1, text("{}"));
    insert(&db, "at-end", 200, "anthropic", 1, text("{}"));

    // Act
    let (n, rows) = visit_all(&db, "anthropic", 100, 200);

    // Assert
    let starts: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(starts, vec![100, 199]);
    assert_eq!(n, 2);
}

#[test]
fn returns_the_served_lane_outcome_and_extra_text_byte_for_byte() {
    // Arrange
    let (_dir, _path, db) = open_db();
    let extra = "{\"opening_present\":true,\"note\":\"caf\u{e9} \u{1f600}\"}";
    insert(&db, "r1", 10, "anthropic", 1, text(extra));

    // Act
    let (_, rows) = visit_all(&db, "anthropic", 0, 100);

    // Assert
    assert_eq!(
        rows,
        vec![StreamExtraRow {
            ts_start_ms: 10,
            provider: Some("anth".to_string()),
            provider_kind: Some("anthropic-api".to_string()),
            model: Some("opus".to_string()),
            upstream: Some("claude-opus-4-7".to_string()),
            outcome: "ok".to_string(),
            extra: Some(extra.to_string()),
        }]
    );
}

#[test]
fn a_null_extra_is_none_and_a_non_text_extra_is_still_returned() {
    // Arrange
    let (_dir, _path, db) = open_db();
    insert(
        &db,
        "null",
        10,
        "anthropic",
        1,
        rusqlite::types::Value::Null,
    );
    insert(
        &db,
        "int",
        11,
        "anthropic",
        1,
        rusqlite::types::Value::Integer(5),
    );
    insert(
        &db,
        "blob",
        12,
        "anthropic",
        1,
        rusqlite::types::Value::Blob(b"{\"k\":1}".to_vec()),
    );

    // Act
    let (n, rows) = visit_all(&db, "anthropic", 0, 100);

    // Assert
    let extras: Vec<Option<&str>> = rows.iter().map(|r| r.extra.as_deref()).collect();
    assert_eq!(extras, vec![None, Some("5"), Some("{\"k\":1}")]);
    assert_eq!(n, 3);
}

#[test]
fn an_empty_window_visits_nothing() {
    // Arrange
    let (_dir, _path, db) = open_db();
    insert(&db, "r1", 10, "anthropic", 1, text("{}"));

    // Act
    let (n, rows) = visit_all(&db, "anthropic", 11, 11);

    // Assert
    assert_eq!(n, 0);
    assert!(rows.is_empty());
}

#[test]
fn reads_through_a_read_only_connection() {
    // Arrange: rows written by one connection, read by a read-only one.
    let (_dir, path, db) = open_db();
    insert(&db, "r1", 10, "anthropic", 1, text("{}"));
    drop(db);
    let ro = open_readonly(&path).expect("open read-only");

    // Act
    let (n, _) = visit_all(&ro, "anthropic", 0, 100);

    // Assert
    assert_eq!(n, 1);
}
