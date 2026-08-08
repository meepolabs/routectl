use super::*;
use crate::db::{open, open_readonly};
use std::path::PathBuf;
use tempfile::TempDir;

// The shared harness and the per-reader test groups live in sibling files to
// keep each file under the size ceiling. They compile into THIS module via
// `include!`, so the helpers stay in scope and no test's module path changes.
include!("query_test_support.rs");

#[test]
fn aggregate_groups_counts_and_sums_per_group() {
    // Arrange: two (provider, upstream) pairs, two outcomes, NULL tokens.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Group A: provider=pa upstream=ua -- 2 ok + 1 error.
    insert_row(
        &db,
        "a1",
        100,
        "m1",
        "pa",
        "ua",
        "al",
        "ok",
        Some(10),
        Some(20),
        5,
        None,
    );
    insert_row(
        &db,
        "a2",
        110,
        "m1",
        "pa",
        "ua",
        "al",
        "ok",
        Some(5),
        Some(7),
        15,
        None,
    );
    insert_row(
        &db,
        "a3",
        120,
        "m1",
        "pa",
        "ua",
        "al",
        "upstream_error",
        None,
        None,
        25,
        None,
    );
    // Group B: provider=pb upstream=ub -- 1 ok.
    insert_row(
        &db,
        "b1",
        130,
        "m2",
        "pb",
        "ub",
        "al",
        "ok",
        Some(3),
        None,
        9,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: two groups.
    assert_eq!(rows.len(), 2);
    let a = find_row(&rows, "pa", "ua");
    assert_eq!(a.requests, 3);
    assert_eq!(a.ok, 2);
    assert_eq!(a.errors, 1);
    // input: 10 + 5 + 0(NULL) = 15; output: 20 + 7 + 0 = 27.
    assert_eq!(a.input_tokens, 15);
    assert_eq!(a.output_tokens, 27);

    let b = find_row(&rows, "pb", "ub");
    assert_eq!(b.requests, 1);
    assert_eq!(b.ok, 1);
    assert_eq!(b.errors, 0);
    assert_eq!(b.input_tokens, 3);
    // output was NULL -> 0.
    assert_eq!(b.output_tokens, 0);
}

#[test]
fn aggregate_excludes_rows_outside_window() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db,
        "in",
        500,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        1,
        None,
    );
    insert_row(
        &db,
        "lo",
        99,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        1,
        None,
    );
    insert_row(
        &db,
        "hi",
        1000,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        1,
        None,
    );

    // Act: window [100, 1000) excludes ts 99 and ts 1000.
    let rows = aggregate(&db, 100, 1000).expect("aggregate");

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 1);
}

#[test]
fn aggregate_sums_server_tool_calls_from_json() {
    // Arrange: two rows whose server_tool_use JSON maps carry int counts.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db,
        "s1",
        100,
        "m",
        "p",
        "u",
        "a",
        "ok",
        None,
        None,
        1,
        Some(r#"{"web_search": 2, "code_exec": 1}"#),
    );
    insert_row(
        &db,
        "s2",
        110,
        "m",
        "p",
        "u",
        "a",
        "ok",
        None,
        None,
        1,
        Some(r#"{"web_search": 3}"#),
    );
    // A row with no server tools contributes 0.
    insert_row(
        &db, "s3", 120, "m", "p", "u", "a", "ok", None, None, 1, None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: 2 + 1 + 3 = 6 invocations across the group.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].server_tool_calls, 6);
}

#[test]
fn aggregate_cache_read_reports_peak_avg_and_billed_with_distinct_semantics() {
    // Arrange: several rows in the SAME group with a CLIMBING cache_read.
    // cache_read is a per-turn SNAPSHOT of the cached prefix re-read that
    // turn. For DISPLAY (context SIZE) the group reports the peak (MAX) and
    // mean (AVG) -- summing those would repeat-count the same growing
    // prefix. For COST, cache reads are billed PER TURN, so the cumulative
    // cost basis IS the sum (`cache_read_billed`). All three must coexist
    // with the right semantics.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(
        &db,
        "k1",
        100,
        1,
        "ok",
        Some(10),
        50,
        Some(1),
        None,
        Some(88_000),
    );
    insert_full_row(
        &db,
        "k2",
        110,
        1,
        "ok",
        Some(10),
        50,
        Some(1),
        None,
        Some(89_000),
    );
    insert_full_row(
        &db,
        "k3",
        120,
        1,
        "ok",
        Some(10),
        50,
        Some(1),
        None,
        Some(91_000),
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: one group; peak is the MAX, avg is the integer mean, and the
    // billed figure is the SUM (the cost basis), distinct from peak/avg.
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.cache_read_peak, 91_000);
    assert_eq!(r.cache_read_avg, 89_333); // (88000+89000+91000)/3 truncated
    assert_eq!(r.cache_read_billed, 268_000); // SUM, the per-turn cost basis
    // The display figures must NOT equal the billed sum.
    assert_ne!(r.cache_read_peak, r.cache_read_billed);
    assert_ne!(r.cache_read_avg, r.cache_read_billed);
    // cache_read_present still counts the reporting rows (all three).
    assert_eq!(r.cache_read_present, 3);
}

#[test]
fn aggregate_null_model_attributes_to_requested_model() {
    // Arrange: a pre-dispatch abort has model=NULL but always carries a
    // requested_model. The aggregate must attribute it to requested_model
    // (the route asked for), not drop it into a NULL group key.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens) \
             VALUES (100, 100, 'abort', 'openai', 'asked-model', 'al', NULL, NULL, \
             NULL, 0, 'client_disconnect', 5, 0, 0, 0, 0, 7, 0)",
            [],
        )
        .expect("insert null-model row");

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: the group key's model is the requested_model, never NULL.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key.model.as_deref(), Some("asked-model"));
    assert!(
        rows[0].key.model.is_some(),
        "must not be a NULL model bucket"
    );
}

#[test]
fn aggregate_errors_excludes_client_disconnect_rows() {
    // Arrange: one ok row and one client_disconnect row in the same group.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db,
        "ok1",
        100,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        5,
        None,
    );
    insert_row(
        &db,
        "cd1",
        110,
        "m",
        "p",
        "u",
        "a",
        "client_disconnect",
        None,
        None,
        5,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: the disconnect row counts toward requests but not errors.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 2);
    assert_eq!(rows[0].ok, 1);
    assert_eq!(rows[0].errors, 0);
}

#[test]
fn aggregate_errors_still_counts_gate_blocked_and_upstream_error() {
    // Arrange: a gate_blocked and an upstream_error row, plus one ok row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db,
        "ok1",
        100,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        5,
        None,
    );
    insert_row(
        &db,
        "gb1",
        110,
        "m",
        "p",
        "u",
        "a",
        "gate_blocked",
        None,
        None,
        5,
        None,
    );
    insert_row(
        &db,
        "ue1",
        120,
        "m",
        "p",
        "u",
        "a",
        "upstream_error",
        None,
        None,
        5,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: both non-ok, non-disconnect outcomes count as errors.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 3);
    assert_eq!(rows[0].errors, 2);
}

#[test]
fn aggregate_client_disconnect_pre_dispatch_counts_model_null_rows_only() {
    // Arrange: two client_disconnect rows -- one pre-dispatch (raw model
    // NULL, disconnected before a provider was ever stamped) and one
    // post-first-content-chunk (model stamped, then the client hung up
    // mid-stream) -- plus one ok row that must not be counted.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
             VALUES (100, 100, 'pre', 'anthropic', 'asked', 'a', NULL, NULL, NULL, \
             1, 'client_disconnect', 5, 0, 0, 0, 0)",
            [],
        )
        .expect("insert pre-dispatch disconnect");
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
             VALUES (110, 110, 'post', 'anthropic', 'asked', 'a', 'm', 'p', 'u', \
             1, 'client_disconnect', 5, 0, 0, 0, 0)",
            [],
        )
        .expect("insert post-dispatch disconnect");
    insert_row(
        &db,
        "ok1",
        120,
        "m",
        "p",
        "u",
        "a",
        "ok",
        Some(1),
        Some(1),
        5,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: both disconnects count toward the total; only the
    // NULL-raw-model one counts toward the pre-dispatch subset.
    let total_cd: i64 = rows.iter().map(|r| r.client_disconnect_total).sum();
    let total_pre: i64 = rows.iter().map(|r| r.client_disconnect_pre_dispatch).sum();
    assert_eq!(total_cd, 2);
    assert_eq!(total_pre, 1);
}

include!("summary_reader_tests.rs");
include!("window_metrics_tests.rs");
include!("calibration_and_errors_tests.rs");
