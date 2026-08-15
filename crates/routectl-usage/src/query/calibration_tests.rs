//! Tests for the calibration-evidence read query: the admission filters that
//! must match the live path, the newest-N cap, and the oldest-first ordering.

use super::*;
use crate::db::open;
use tempfile::TempDir;

fn open_db() -> (TempDir, crate::db::UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, db)
}

/// Insert one request row with full control over every column the query
/// filters or returns.
#[allow(clippy::too_many_arguments)]
fn insert_row(
    db: &crate::db::UsageDb,
    request_id: &str,
    ts_start: i64,
    provider_kind: Option<&str>,
    model: Option<&str>,
    session_id: Option<&str>,
    estimated: Option<i64>,
    prompt: Option<i64>,
    outcome: &str,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider_kind, session_id, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             calib_estimated_tokens, calib_prompt_tokens) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', ?3, ?4, ?5, 0, ?6, 5, 0, 0, \
             1, 0, ?7, ?8)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider_kind,
                session_id,
                outcome,
                estimated,
                prompt,
            ],
        )
        .expect("insert row");
}

/// An admissible row: a success carrying a full pair and a full lane key.
fn insert_admissible(db: &crate::db::UsageDb, request_id: &str, ts_start: i64, prompt: i64) {
    insert_row(
        db,
        request_id,
        ts_start,
        Some("anthropic-api"),
        Some("opus"),
        Some("sess"),
        Some(10_000),
        Some(prompt),
        "ok",
    );
}

#[test]
fn an_admissible_row_returns_every_column_the_reducer_needs() {
    // Arrange
    let (_dir, db) = open_db();
    insert_admissible(&db, "r1", 500, 12_500);

    // Act
    let rows = read_calibration_samples_since(db.conn(), 0, 100).expect("read");

    // Assert
    assert_eq!(
        rows,
        vec![CalibrationSampleRow {
            ts_start_ms: 500,
            provider_kind: "anthropic-api".to_string(),
            model: "opus".to_string(),
            session_id: Some("sess".to_string()),
            estimated_tokens: 10_000,
            prompt_tokens: 12_500,
        }]
    );
}

/// The admission contract, exercised one refusal at a time. Live traffic
/// records only on the success finalize, and that finalize admits or refuses
/// the evidence pair as a UNIT -- so a non-success, or a NULL in either half,
/// is a row the live store never saw. Replaying one would let a restart admit
/// rows live traffic never would, silently diverging the two paths.
#[test]
fn rows_the_live_path_never_recorded_are_not_admitted() {
    // Arrange: one admissible row plus one row per refusal cause.
    let (_dir, db) = open_db();
    insert_admissible(&db, "admissible", 500, 12_500);
    insert_row(
        &db,
        "mid-stream-failure",
        510,
        Some("anthropic-api"),
        Some("opus"),
        Some("sess"),
        Some(10_000),
        Some(12_500),
        "upstream_error",
    );
    insert_row(
        &db,
        "null-estimate-half",
        520,
        Some("anthropic-api"),
        Some("opus"),
        Some("sess"),
        None,
        Some(12_500),
        "ok",
    );
    insert_row(
        &db,
        "null-prompt-half",
        530,
        Some("anthropic-api"),
        Some("opus"),
        Some("sess"),
        Some(10_000),
        None,
        "ok",
    );
    insert_row(
        &db,
        "no-provider-kind",
        540,
        None,
        Some("opus"),
        Some("sess"),
        Some(10_000),
        Some(12_500),
        "ok",
    );
    insert_row(
        &db,
        "no-served-model",
        550,
        Some("anthropic-api"),
        None,
        Some("sess"),
        Some(10_000),
        Some(12_500),
        "ok",
    );

    // Act
    let rows = read_calibration_samples_since(db.conn(), 0, 100).expect("read");

    // Assert: exactly the one admissible row.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts_start_ms, 500);
}

/// A keyless request IS recorded by the live path (under a shared cohort), so
/// filtering NULL session ids out here would admit FEWER rows than live
/// traffic did -- divergence in the other direction.
#[test]
fn a_keyless_row_is_admitted_because_the_live_path_records_one() {
    let (_dir, db) = open_db();
    insert_row(
        &db,
        "keyless",
        500,
        Some("anthropic-api"),
        Some("opus"),
        None,
        Some(10_000),
        Some(12_500),
        "ok",
    );

    let rows = read_calibration_samples_since(db.conn(), 0, 100).expect("read");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, None);
}

#[test]
fn rows_before_the_window_start_are_excluded() {
    let (_dir, db) = open_db();
    insert_admissible(&db, "before", 400, 12_500);
    insert_admissible(&db, "at-boundary", 500, 12_500);
    insert_admissible(&db, "after", 600, 12_500);

    let rows = read_calibration_samples_since(db.conn(), 500, 100).expect("read");

    let stamps: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(stamps, vec![500, 600], "the window start is inclusive");
}

#[test]
fn the_cap_keeps_the_newest_rows_and_returns_them_oldest_first() {
    // Arrange: five rows, a cap of three.
    let (_dir, db) = open_db();
    for i in 0..5 {
        insert_admissible(&db, &format!("r{i}"), 100 + i, 12_500);
    }

    // Act
    let rows = read_calibration_samples_since(db.conn(), 0, 3).expect("read");

    // Assert: the OLDEST two were dropped, and the survivors emit
    // oldest-first so a replay lands in arrival order.
    let stamps: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(stamps, vec![102, 103, 104]);
}

/// `rowid` breaks ties on an identical `ts_start`: it tracks insertion order,
/// so the most recently inserted rows win the cap and survivors emit in
/// stable insertion order. Selection at the boundary is deterministic.
#[test]
fn same_timestamp_rows_break_ties_by_insertion_order() {
    let (_dir, db) = open_db();
    for i in 0..4 {
        insert_admissible(&db, &format!("same-ts-{i}"), 700, 12_500 + i64::from(i));
    }

    let rows = read_calibration_samples_since(db.conn(), 0, 2).expect("read");

    let totals: Vec<i64> = rows.iter().map(|r| r.prompt_tokens).collect();
    assert_eq!(
        totals,
        vec![12_502, 12_503],
        "the two most recently inserted rows win the cap, in insertion order"
    );
}

#[test]
fn an_empty_ledger_yields_no_rows_rather_than_an_error() {
    let (_dir, db) = open_db();

    let rows = read_calibration_samples_since(db.conn(), 0, 100).expect("read");

    assert!(rows.is_empty());
}
