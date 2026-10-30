//! Tests for the windowed observation totals and the ledger gap check.

use super::*;
use crate::db::{open, open_readonly_fastfail};
use crate::record::Operation;
use std::path::PathBuf;
use tempfile::TempDir;

const FROM: i64 = 100;
const TO: i64 = 200;

fn open_db() -> (TempDir, PathBuf, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, path, db)
}

/// The five observation counts of one row, in column order; `None` is a
/// legacy NULL.
type Counts = [Option<i64>; 5];

fn insert_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    operation: Option<Operation>,
    counts: Counts,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, operation, wire_losses, \
             translation_drops, local_refusals, field_repairs, missing_terminals) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
             ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                ts_start,
                request_id,
                operation.map(|op| op.as_str()),
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4],
            ],
        )
        .expect("insert row");
}

fn insert_gap(db: &UsageDb, ts_ms: i64, dropped: i64) {
    db.conn()
        .execute(
            "INSERT INTO ledger_gaps (ts_ms, cause, dropped) VALUES (?1, ?2, ?3)",
            rusqlite::params![ts_ms, crate::ledger_gap::QUEUE_FULL_CAUSE, dropped],
        )
        .expect("insert gap");
}

fn same(n: i64) -> Counts {
    [Some(n); 5]
}

fn counts_of(totals: &ObservationTotals) -> [i64; 5] {
    [
        totals.wire_losses,
        totals.translation_drops,
        totals.local_refusals,
        totals.field_repairs,
        totals.missing_terminals,
    ]
}

#[test]
fn sums_every_row_kind_including_count_tokens_and_treats_null_as_zero() {
    // Arrange: inference, count_tokens, a legacy row (operation and counts
    // NULL), and an inference row with partly NULL counts.
    let (_dir, _path, db) = open_db();
    let inference = Some(Operation::Inference);
    let count = Some(Operation::CountTokens);
    insert_row(
        &db,
        "inf",
        110,
        inference,
        [Some(1), Some(2), Some(3), Some(4), Some(5)],
    );
    insert_row(
        &db,
        "cnt",
        120,
        count,
        [Some(10), Some(20), Some(30), Some(40), Some(50)],
    );
    insert_row(&db, "legacy", 130, None, [None; 5]);
    insert_row(
        &db,
        "partial",
        140,
        inference,
        [Some(100), None, Some(300), None, None],
    );

    // Act
    let totals = observation_totals(&db, FROM, TO).expect("observation_totals");

    // Assert: 1+10+100, 2+20, 3+30+300, 4+40, 5+50.
    assert_eq!(counts_of(&totals), [111, 22, 333, 44, 55]);
    assert!(!totals.has_gap());
}

#[test]
fn window_is_half_open_on_ts_start() {
    // Arrange: one row on each side of each bound, with values whose sum
    // identifies exactly which rows were counted.
    let (_dir, _path, db) = open_db();
    let inference = Some(Operation::Inference);
    insert_row(&db, "before-from", FROM - 1, inference, same(1));
    insert_row(&db, "at-from", FROM, inference, same(10));
    insert_row(&db, "before-to", TO - 1, inference, same(100));
    insert_row(&db, "at-to", TO, inference, same(1000));

    // Act
    let totals = observation_totals(&db, FROM, TO).expect("observation_totals");

    // Assert: only `at-from` and `before-to` are in [FROM, TO).
    assert_eq!(counts_of(&totals), [110; 5]);
}

#[test]
fn empty_window_returns_zero_totals_without_gap() {
    // Arrange
    let (_dir, _path, db) = open_db();
    insert_row(&db, "outside", TO + 5, Some(Operation::Inference), same(7));

    // Act
    let totals = observation_totals(&db, FROM, TO).expect("observation_totals");

    // Assert
    assert_eq!(totals, ObservationTotals::default());
    assert!(!totals.has_gap());
}

#[test]
fn has_gap_follows_gap_rows_on_the_half_open_window() {
    // One row per case: (label, gap ts_ms or none, expected has_gap).
    let cases: [(&str, Option<i64>, bool); 5] = [
        ("no gap rows", None, false),
        ("gap just before from", Some(FROM - 1), false),
        ("gap at from", Some(FROM), true),
        ("gap just before to", Some(TO - 1), true),
        ("gap at to", Some(TO), false),
    ];
    for (label, gap_ts, expected) in cases {
        // Arrange
        let (_dir, _path, db) = open_db();
        insert_row(&db, "r", FROM + 1, Some(Operation::Inference), same(1));
        if let Some(ts) = gap_ts {
            insert_gap(&db, ts, 3);
        }

        // Act
        let totals = observation_totals(&db, FROM, TO).expect("observation_totals");

        // Assert
        assert_eq!(totals.has_gap(), expected, "case: {label}");
        assert_eq!(totals.gap_rows, i64::from(expected), "case: {label}");
        assert_eq!(
            totals.gap_dropped,
            if expected { 3 } else { 0 },
            "case: {label}"
        );
    }
}

#[test]
fn gap_totals_count_rows_and_sum_dropped_inside_the_window_only() {
    // Arrange
    let (_dir, _path, db) = open_db();
    insert_gap(&db, FROM - 1, 1000);
    insert_gap(&db, FROM + 10, 4);
    insert_gap(&db, FROM + 20, 5);
    insert_gap(&db, TO, 2000);

    // Act
    let totals = observation_totals(&db, FROM, TO).expect("observation_totals");

    // Assert
    assert_eq!(totals.gap_rows, 2);
    assert_eq!(totals.gap_dropped, 9);
    assert!(totals.has_gap());
}

#[test]
fn reads_over_the_fastfail_readonly_open() {
    // Arrange: seed through the read-write open (which sets WAL), then
    // reopen the way the status surfaces do.
    let (_dir, path, db) = open_db();
    insert_row(&db, "cnt", FROM, Some(Operation::CountTokens), same(2));
    insert_gap(&db, FROM, 6);
    drop(db);
    let ro = open_readonly_fastfail(&path).expect("open readonly fastfail");

    // Act
    let totals = observation_totals(&ro, FROM, TO).expect("observation_totals");

    // Assert
    assert_eq!(counts_of(&totals), [2; 5]);
    assert_eq!((totals.gap_rows, totals.gap_dropped), (1, 6));
}

#[test]
fn observation_totals_uses_ts_start_index() {
    // The request sums must ride idx_requests_ts_start for the window range,
    // not degrade to a full table scan.
    let (_dir, _path, db) = open_db();
    for i in 0..64 {
        insert_row(
            &db,
            &format!("r{i}"),
            FROM + i,
            Some(Operation::Inference),
            same(1),
        );
    }

    let plan: Vec<String> = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {OBSERVATION_TOTALS_SQL}"))
        .expect("prepare explain")
        .query_map([FROM, TO], |row| row.get::<_, String>(3))
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    assert!(
        plan.iter().any(|d| d.contains("idx_requests_ts_start")),
        "observation totals must use idx_requests_ts_start; plan was {plan:?}"
    );
}
