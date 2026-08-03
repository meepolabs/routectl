use super::*;
use crate::db::{open, open_readonly};
use std::path::PathBuf;
use tempfile::TempDir;

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

/// Insert a row with explicit group keys, outcome, tokens, latency, and
/// optional server_tool_use JSON. Token args are Option to exercise the
/// NULL-contributes-0 path.
#[allow(clippy::too_many_arguments)]
fn insert_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    model: &str,
    provider: &str,
    upstream: &str,
    alias: &str,
    outcome: &str,
    input: Option<i64>,
    output: Option<i64>,
    latency_ms: i64,
    server_tool_use: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, server_tool_use) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, 0, ?7, \
             ?8, 0, 0, 1, 0, ?9, ?10, ?11)",
            rusqlite::params![
                ts_start,
                request_id,
                alias,
                model,
                provider,
                upstream,
                outcome,
                latency_ms,
                input,
                output,
                server_tool_use,
            ],
        )
        .expect("insert row");
}

/// Insert a row with explicit `stream`, `ttfb_ms`, `outcome`,
/// `reasoning_tokens`, and cache columns so the streaming /
/// presence-count paths can be exercised. `ttfb_ms`, `reasoning`, and the
/// cache args are `Option` so NULL-vs-reported-0 is testable.
#[allow(clippy::too_many_arguments)]
fn insert_full_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    stream: i64,
    outcome: &str,
    ttfb_ms: Option<i64>,
    latency_ms: i64,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
             fallback_count, output_tokens, reasoning_tokens, cache_read) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', 'pa', 'ua', \
             ?3, ?4, ?5, ?6, 0, 0, 1, 0, ?7, ?8, ?9)",
            rusqlite::params![
                ts_start, request_id, stream, outcome, latency_ms, ttfb_ms, output, reasoning,
                cache_read,
            ],
        )
        .expect("insert full row");
}

/// Insert a quota-bearing row with an explicit `seat` / `provider_kind` and
/// individually nullable quota columns, so the per-seat partition and the
/// widened `status OR utilization` eligibility predicate are both exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_seat_quota_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    seat: Option<&str>,
    provider_kind: Option<&str>,
    status: Option<&str>,
    utilization: Option<f64>,
    reset: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, seat, provider_kind, \
             quota_status, quota_utilization, quota_reset) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
             ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                seat,
                provider_kind,
                status,
                utilization,
                reset
            ],
        )
        .expect("insert quota row");
}

fn find_seat<'a>(snaps: &'a [QuotaSnapshot], seat: Option<&str>) -> &'a QuotaSnapshot {
    snaps
        .iter()
        .find(|s| s.seat.as_deref() == seat)
        .expect("seat bucket present")
}

fn find_row<'a>(rows: &'a [AggRow], provider: &str, upstream: &str) -> &'a AggRow {
    rows.iter()
        .find(|r| {
            r.key.provider.as_deref() == Some(provider)
                && r.key.upstream.as_deref() == Some(upstream)
        })
        .expect("group present")
}

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

/// Insert a row with an optional `would_trim_tokens` value so the
/// would-trim summary's NULL-vs-present accounting is testable.
fn insert_would_trim_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    would_trim_tokens: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, would_trim_tokens) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3)",
            rusqlite::params![ts_start, request_id, would_trim_tokens],
        )
        .expect("insert would-trim row");
}

#[test]
fn would_trim_summary_counts_candidates_and_sums_tokens() {
    // Arrange: two rows with candidates, one without (NULL), plus an
    // out-of-window candidate row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_would_trim_row(&db, "w1", 100, Some(40_000));
    insert_would_trim_row(&db, "w2", 110, Some(20_000));
    insert_would_trim_row(&db, "w3", 120, None);
    insert_would_trim_row(&db, "out", 5, Some(99_000));

    // Act
    let s = would_trim_summary(&db, 100, 1000).expect("summary");

    // Assert: COUNT ignores the NULL row and the out-of-window row.
    assert_eq!(s.candidate_requests, 2);
    assert_eq!(s.would_trim_tokens, 60_000);
}

#[test]
fn would_trim_summary_is_zero_when_no_candidates() {
    // Arrange: only a plain row with no would-trim candidate.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_would_trim_row(&db, "plain", 100, None);

    // Act + Assert
    let s = would_trim_summary(&db, 0, 1000).expect("summary");
    assert_eq!(s.candidate_requests, 0);
    assert_eq!(s.would_trim_tokens, 0);
}

#[test]
fn would_trim_summary_on_empty_ledger_returns_all_zeros() {
    // Arrange: a healthy but EMPTY ledger (no rows at all). Over zero
    // matching rows the verdict `SUM(CASE ...)` columns return SQL NULL,
    // which the row mapping would read as a non-nullable i64 and error
    // (InvalidColumnType Null). The COALESCE guard yields a zeroed summary.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act
    let s = would_trim_summary(&db, 0, 1000).expect("summary over empty ledger");

    // Assert: a valid, fully-zeroed summary rather than a query error.
    assert_eq!(s, WouldTrimSummary::default());
}

/// Insert a row carrying the M1 attribution columns, or a baseline
/// (pre-M1) row when `recorder_version` is `None` -- the latter must
/// never contribute to `m1_attribution_summary` totals even when it
/// carries a `would_trim_tokens` baseline candidate.
#[allow(clippy::too_many_arguments)]
fn insert_m1_attribution_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    recorder_version: Option<i64>,
    dedup_tokens: Option<i64>,
    supersession_tokens: Option<i64>,
    path_units: Option<i64>,
    path_extractable: Option<i64>,
    context_fraction: Option<f64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, would_trim_tokens, \
             would_trim_recorder_version, would_trim_dedup_tokens, \
             would_trim_supersession_tokens, would_trim_path_units, \
             would_trim_path_extractable, would_trim_context_fraction) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, 99999, \
             ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                ts_start,
                request_id,
                recorder_version,
                dedup_tokens,
                supersession_tokens,
                path_units,
                path_extractable,
                context_fraction,
            ],
        )
        .expect("insert m1 attribution row");
}

#[test]
fn m1_attribution_summary_excludes_baseline_rows_without_recorder_version() {
    // Arrange: two M1-recorded rows (recorder_version = 1) and one
    // baseline row (recorder_version = NULL) that also carries a
    // baseline would_trim_tokens candidate and would incorrectly inflate
    // the M1 totals if the recorder-version filter were dropped.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_m1_attribution_row(
        &db,
        "m1",
        100,
        Some(1),
        Some(500),
        Some(300),
        Some(4),
        Some(3),
        Some(0.10),
    );
    insert_m1_attribution_row(
        &db,
        "m2",
        110,
        Some(1),
        Some(200),
        Some(0),
        Some(2),
        Some(2),
        Some(0.20),
    );
    insert_m1_attribution_row(&db, "baseline", 120, None, None, None, None, None, None);

    // Act
    let s = m1_attribution_summary(&db, 100, 1000).expect("summary");

    // Assert: only the two recorder-version rows contribute.
    assert_eq!(s.recorder_requests, 2);
    assert_eq!(s.dedup_tokens, 700);
    assert_eq!(s.supersession_tokens, 300);
    assert_eq!(s.path_units, 6);
    assert_eq!(s.path_extractable, 5);
    assert_eq!(s.context_fraction_present, 2);
    assert!((s.context_fraction_sum - 0.30).abs() < 1e-9);
}

#[test]
fn m1_attribution_summary_is_zero_when_no_recorder_rows() {
    // Arrange: only a baseline row with no M1 recording.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_m1_attribution_row(&db, "baseline", 100, None, None, None, None, None, None);

    // Act + Assert
    let s = m1_attribution_summary(&db, 0, 1000).expect("summary");
    assert_eq!(s.recorder_requests, 0);
    assert_eq!(s.dedup_tokens, 0);
    assert_eq!(s.supersession_tokens, 0);
    assert_eq!(s.path_units, 0);
    assert_eq!(s.path_extractable, 0);
    assert_eq!(s.context_fraction_present, 0);
    assert_eq!(s.context_fraction_sum, 0.0);
}

#[test]
fn latest_quota_by_seat_returns_the_newest_row_per_seat() {
    // Arrange: two rows for seatA (old + new) and one for seatB.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "a-old",
        100,
        Some("seatA"),
        Some("anthropic-api"),
        Some("active"),
        Some(0.10),
        Some(5_000),
    );
    insert_seat_quota_row(
        &db,
        "a-new",
        200,
        Some("seatA"),
        Some("anthropic-api"),
        Some("throttled"),
        Some(0.90),
        Some(9_000),
    );
    insert_seat_quota_row(
        &db,
        "b-only",
        150,
        Some("seatB"),
        Some("codex"),
        None,
        Some(0.16),
        Some(1_786_210_114),
    );
    // A non-quota row must be ignored entirely.
    insert_row(
        &db, "plain", 300, "m", "p", "u", "a", "ok", None, None, 1, None,
    );

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert: one snapshot per seat, seatA resolved to its newer row.
    assert_eq!(snaps.len(), 2);
    let a = find_seat(&snaps, Some("seatA"));
    assert_eq!(a.ts_start, 200);
    assert_eq!(a.status.as_deref(), Some("throttled"));
    assert_eq!(a.utilization, Some(0.90));
    assert_eq!(a.reset, Some(9_000));
    assert_eq!(a.provider_kind.as_deref(), Some("anthropic-api"));
    let b = find_seat(&snaps, Some("seatB"));
    assert_eq!(b.ts_start, 150);
    assert_eq!(b.provider_kind.as_deref(), Some("codex"));
}

#[test]
fn latest_quota_by_seat_breaks_a_ts_start_tie_by_rowid() {
    // Arrange: two rows for one seat sharing a ts_start -- the later-inserted
    // row (higher rowid) is the newer snapshot.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for (id, status) in [("tie-first", "active"), ("tie-second", "throttled")] {
        insert_seat_quota_row(
            &db,
            id,
            500,
            Some("seatA"),
            Some("anthropic-api"),
            Some(status),
            Some(0.5),
            None,
        );
    }

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].status.as_deref(), Some("throttled"));
}

#[test]
fn latest_quota_by_seat_includes_a_utilization_only_row() {
    // Arrange: a codex-shaped row -- no status token, utilization present.
    // The widened predicate must keep it visible.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "codex-1",
        100,
        Some("codex"),
        Some("codex"),
        None,
        Some(0.16),
        Some(1_786_210_114),
    );

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert
    assert_eq!(snaps.len(), 1);
    assert!(snaps[0].status.is_none());
    assert_eq!(snaps[0].utilization, Some(0.16));
    assert_eq!(snaps[0].reset, Some(1_786_210_114));
}

#[test]
fn latest_quota_by_seat_includes_a_status_only_row() {
    // Arrange: an Anthropic-shaped row that reported a status token but no
    // utilization -- the predicate is an OR, so it must stay visible.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "status-only",
        100,
        Some("seatA"),
        Some("anthropic-api"),
        Some("allowed"),
        None,
        Some(9_000),
    );

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].status.as_deref(), Some("allowed"));
    assert!(snaps[0].utilization.is_none());
    assert_eq!(snaps[0].reset, Some(9_000));
}

#[test]
fn latest_quota_by_seat_collapses_two_null_seat_rows_to_the_newest() {
    // Arrange: two pre-seat rows with different ts_start. NULL seats share a
    // single bucket, so only the newer snapshot may come back.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "null-old",
        100,
        None,
        None,
        Some("active"),
        Some(0.2),
        None,
    );
    insert_seat_quota_row(
        &db,
        "null-new",
        200,
        None,
        None,
        Some("throttled"),
        Some(0.9),
        None,
    );

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert
    assert_eq!(snaps.len(), 1, "NULL seats collapse to one bucket");
    assert_eq!(snaps[0].ts_start, 200);
    assert_eq!(snaps[0].status.as_deref(), Some("throttled"));
    assert_eq!(snaps[0].utilization, Some(0.9));
}

#[test]
fn latest_quota_by_seat_omits_a_row_with_neither_status_nor_utilization() {
    // Arrange: a row carrying only a reset -- no quota signal to report.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "reset-only",
        100,
        Some("seatA"),
        Some("codex"),
        None,
        None,
        Some(9_000),
    );

    // Act + Assert
    assert!(latest_quota_by_seat(&db).expect("query").is_empty());
}

#[test]
fn latest_quota_by_seat_gives_a_null_seat_row_its_own_bucket() {
    // Arrange: pre-seat history (NULL seat) alongside a seated row. The NULL
    // bucket must survive rather than being filtered or merged.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "legacy",
        100,
        None,
        None,
        Some("active"),
        Some(0.2),
        None,
    );
    insert_seat_quota_row(
        &db,
        "seated",
        200,
        Some("seatA"),
        Some("anthropic-api"),
        Some("throttled"),
        Some(0.9),
        None,
    );

    // Act
    let snaps = latest_quota_by_seat(&db).expect("query");

    // Assert
    assert_eq!(snaps.len(), 2);
    let legacy = find_seat(&snaps, None);
    assert_eq!(legacy.ts_start, 100);
    assert!(legacy.provider_kind.is_none());
    assert_eq!(find_seat(&snaps, Some("seatA")).ts_start, 200);
}

#[test]
fn latest_quota_by_seat_is_empty_when_no_quota_rows() {
    // Arrange
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db, "plain", 100, "m", "p", "u", "a", "ok", None, None, 1, None,
    );

    // Act + Assert
    assert!(latest_quota_by_seat(&db).expect("query").is_empty());
}

#[test]
fn latest_quota_by_seat_on_an_empty_ledger_is_empty() {
    // Arrange: a migrated ledger with zero rows.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act + Assert
    assert!(latest_quota_by_seat(&db).expect("query").is_empty());
}

#[test]
fn earliest_ts_start_returns_the_oldest_rows_timestamp() {
    // Arrange: rows inserted out of timestamp order.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for (id, ts) in [("b", 500), ("a", 100), ("c", 900)] {
        insert_row(&db, id, ts, "m", "p", "u", "a", "ok", None, None, 1, None);
    }

    // Act + Assert
    assert_eq!(earliest_ts_start(&db, 0).expect("query"), Some(100));
}

#[test]
fn earliest_ts_start_ignores_rows_below_the_lower_bound() {
    // Arrange: a row the caller's window excludes, plus two it includes. The
    // bound is the same inclusive one the aggregate applies, so the anchor this
    // feeds can never widen the row set.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for (id, ts) in [("pre", -5_000), ("a", 100), ("c", 900)] {
        insert_row(&db, id, ts, "m", "p", "u", "a", "ok", None, None, 1, None);
    }

    // Act + Assert
    assert_eq!(earliest_ts_start(&db, 0).expect("query"), Some(100));
    assert_eq!(earliest_ts_start(&db, 500).expect("query"), Some(900));
    assert_eq!(
        earliest_ts_start(&db, -10_000).expect("query"),
        Some(-5_000)
    );
}

#[test]
fn earliest_ts_start_on_an_empty_ledger_is_absent_rather_than_an_error() {
    // Arrange: `MIN` over zero rows is a single NULL row, not an absent row, so
    // this must not read as a missing-row failure.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act + Assert
    assert_eq!(earliest_ts_start(&db, 0).expect("query"), None);
}

#[test]
fn aggregate_over_readonly_open_matches_seeded_results() {
    // Arrange: seed via the read-write open, then drop it so the file is
    // read through the real CLI path (open_readonly).
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_row(
        &db,
        "ro-a1",
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
        "ro-a2",
        110,
        "m1",
        "pa",
        "ua",
        "al",
        "upstream_error",
        None,
        None,
        15,
        None,
    );
    drop(db);

    // Act: read via the read-only open path.
    let ro = open_readonly(&path).expect("open readonly");
    let rows = aggregate(&ro, 0, 1000).expect("aggregate");

    // Assert
    assert_eq!(rows.len(), 1);
    let a = find_row(&rows, "pa", "ua");
    assert_eq!(a.requests, 2);
    assert_eq!(a.ok, 1);
    assert_eq!(a.errors, 1);
    assert_eq!(a.input_tokens, 10);
    assert_eq!(a.output_tokens, 20);
    assert_eq!(a.key.alias, "al");
}

#[test]
fn latest_quota_by_seat_over_readonly_open_matches_seeded_results() {
    // Arrange: seed quota rows for one seat, then drop the writer.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_seat_quota_row(
        &db,
        "ro-q-old",
        100,
        Some("seatA"),
        Some("anthropic-api"),
        Some("active"),
        Some(0.10),
        Some(5_000),
    );
    insert_seat_quota_row(
        &db,
        "ro-q-new",
        200,
        Some("seatA"),
        Some("anthropic-api"),
        Some("throttled"),
        Some(0.90),
        Some(9_000),
    );
    drop(db);

    // Act
    let ro = open_readonly(&path).expect("open readonly");
    let snaps = latest_quota_by_seat(&ro).expect("query");

    // Assert: same per-seat resolution the read-write path returns.
    assert_eq!(snaps.len(), 1);
    let snap = find_seat(&snaps, Some("seatA"));
    assert_eq!(snap.ts_start, 200);
    assert_eq!(snap.status.as_deref(), Some("throttled"));
    assert_eq!(snap.utilization, Some(0.90));
    assert_eq!(snap.reset, Some(9_000));
}

#[test]
fn aggregate_gen_window_only_counts_streaming_ok_rows_with_ttfb() {
    // Arrange: one qualifying streaming-ok row, plus rows that the
    // predicate must exclude (non-stream, error, NULL ttfb, latency<=ttfb).
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Qualifying: stream, ok, ttfb=100, latency=500 -> gen window 400.
    insert_full_row(
        &db,
        "g1",
        100,
        1,
        "ok",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // Non-stream row excluded.
    insert_full_row(
        &db,
        "g2",
        110,
        0,
        "ok",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // Error row excluded.
    insert_full_row(
        &db,
        "g3",
        120,
        1,
        "upstream_error",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // NULL ttfb excluded.
    insert_full_row(&db, "g4", 130, 1, "ok", None, 500, Some(40), None, None);
    // latency <= ttfb excluded.
    insert_full_row(
        &db,
        "g5",
        140,
        1,
        "ok",
        Some(500),
        500,
        Some(40),
        None,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: only the first row contributes to the generation window.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].gen_window_ms, 400);
    assert_eq!(rows[0].gen_output_tokens, 40);
}

#[test]
fn aggregate_presence_counts_distinguish_null_from_reported_zero() {
    // Arrange: one row reasoning=0 (reported), one row reasoning=NULL.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(
        &db,
        "p1",
        100,
        1,
        "ok",
        Some(10),
        50,
        Some(1),
        Some(0),
        Some(5),
    );
    insert_full_row(&db, "p2", 110, 1, "ok", Some(10), 50, Some(1), None, None);

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: COUNT(col) ignores the NULL row -> 1, not 2.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reasoning_present, 1);
    assert_eq!(rows[0].cache_read_present, 1);
    assert_eq!(rows[0].reasoning_tokens, 0);
}

#[test]
fn aggregate_stream_count_sums_streaming_flag() {
    // Arrange: two streaming rows, one non-stream row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(&db, "c1", 100, 1, "ok", Some(10), 50, Some(1), None, None);
    insert_full_row(&db, "c2", 110, 1, "ok", Some(10), 50, Some(1), None, None);
    insert_full_row(&db, "c3", 120, 0, "ok", Some(10), 50, Some(1), None, None);

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stream_count, 2);
    assert_eq!(rows[0].ttfb_count, 3);
    assert_eq!(rows[0].sum_ttfb_ms, 30);
}

#[test]
fn ttfbs_returns_in_window_streaming_ok_values() {
    // Arrange: two qualifying streaming-ok rows, plus excluded rows.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(&db, "t1", 100, 1, "ok", Some(11), 50, Some(1), None, None);
    insert_full_row(&db, "t2", 110, 1, "ok", Some(22), 50, Some(1), None, None);
    // Non-stream excluded.
    insert_full_row(&db, "t3", 120, 0, "ok", Some(33), 50, Some(1), None, None);
    // Error excluded.
    insert_full_row(
        &db,
        "t4",
        130,
        1,
        "timeout",
        Some(44),
        50,
        Some(1),
        None,
        None,
    );
    // Out of window excluded.
    insert_full_row(&db, "t5", 5, 1, "ok", Some(55), 50, Some(1), None, None);

    // Act
    let rows = ttfbs(&db, 100, 1000).expect("ttfbs");

    // Assert
    let values: Vec<i64> = rows.iter().map(|(_, ms)| *ms).collect();
    assert_eq!(values.len(), 2);
    assert!(values.contains(&11));
    assert!(values.contains(&22));
    assert!(!values.contains(&33));
    assert!(!values.contains(&44));
    assert!(!values.contains(&55));
}

/// Insert a row exercising the reuse-sample columns: nullable
/// `session_id`, `provider_kind`, `model`, `cache_read`, and an explicit
/// `outcome` so the admission-contract filter is exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_reuse_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: Option<&str>,
    provider_kind: Option<&str>,
    model: Option<&str>,
    cache_read: Option<i64>,
    outcome: &str,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider_kind, session_id, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, ?6, \
             5, 0, 0, 1, 0, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider_kind,
                session_id,
                outcome,
                cache_read,
            ],
        )
        .expect("insert reuse row");
}

#[test]
fn read_reuse_samples_filters_nulls_coalesces_and_orders() {
    // Arrange: a mix of complete and partial rows, two triples, delivered
    // out of ts order, plus an out-of-window row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Complete rows for triple A (one with NULL cache_read -> 0).
    insert_reuse_row(
        &db,
        "a2",
        200,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(42),
        "ok",
    );
    insert_reuse_row(
        &db,
        "a1",
        100,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        None,
        "ok",
    );
    // A second triple (different provider_kind).
    insert_reuse_row(
        &db,
        "b1",
        150,
        Some("s1"),
        Some("bedrock"),
        Some("opus"),
        Some(7),
        "ok",
    );
    // NULL session_id -> filtered out (no usable triple identity).
    insert_reuse_row(
        &db,
        "n-sess",
        120,
        None,
        Some("anthropic-api"),
        Some("opus"),
        Some(9),
        "ok",
    );
    // NULL provider_kind -> filtered out.
    insert_reuse_row(
        &db,
        "n-pk",
        130,
        Some("s2"),
        None,
        Some("opus"),
        Some(9),
        "ok",
    );
    // NULL model -> filtered out.
    insert_reuse_row(
        &db,
        "n-model",
        140,
        Some("s2"),
        Some("anthropic-api"),
        None,
        Some(9),
        "ok",
    );
    // Out of window (ts < window_start).
    insert_reuse_row(
        &db,
        "old",
        50,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(99),
        "ok",
    );

    // Act: window starts at 100.
    let rows = read_reuse_samples_since(db.conn(), 100, 100).expect("read");

    // Assert: three rows survive (the three complete, in-window rows),
    // ordered ascending by ts.
    let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ids, vec![100, 150, 200]);
    // NULL cache_read coalesced to 0 on a1.
    let a1 = rows.iter().find(|r| r.ts_start_ms == 100).expect("a1");
    assert_eq!(a1.cache_read, 0);
    assert_eq!(a1.session_id, "s1");
    assert_eq!(a1.provider_kind, "anthropic-api");
    assert_eq!(a1.model, "opus");
    // The cross-provider row maps its own provider_kind.
    let b1 = rows.iter().find(|r| r.ts_start_ms == 150).expect("b1");
    assert_eq!(b1.provider_kind, "bedrock");
    assert_eq!(b1.cache_read, 7);
}

#[test]
fn read_reuse_samples_selects_newest_within_window() {
    // Arrange: limit + 2 eligible rows with ascending distinct ts_start,
    // all inside the window.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let limit = 3usize;
    let total = limit + 2;
    for i in 0..total as i64 {
        insert_reuse_row(
            &db,
            &format!("r{i}"),
            1000 + i,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(1),
            "ok",
        );
    }

    // Act: window admits all rows; cap below the eligible count.
    let rows = read_reuse_samples_since(db.conn(), 1000, limit).expect("read");

    // Assert: the returned set is the NEWEST `limit` rows (largest
    // timestamps), returned oldest-first. Under the oldest-first ASC
    // LIMIT this returned [1000, 1001, 1002] and fails.
    let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ids, vec![1002, 1003, 1004]);
}

#[test]
fn read_reuse_samples_breaks_cap_boundary_ties_by_insertion_order() {
    // Arrange: more qualifying rows than the limit, where the rows
    // straddling the cap boundary share an identical ts_start. Insertion
    // order (rowid) is the only signal distinguishing them, so it must
    // decide which survive and how survivors are ordered. Each tied row
    // carries a distinct cache_read so the public row shape reveals both
    // which rows survived and in what order.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let limit = 3usize;
    // Four rows at the SAME ts_start, inserted earliest-first with
    // ascending cache_read markers (10, 20, 30, 40).
    for (i, marker) in [10i64, 20, 30, 40].into_iter().enumerate() {
        insert_reuse_row(
            &db,
            &format!("tie{i}"),
            500,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(marker),
            "ok",
        );
    }

    // Act: cap below the count of tied rows.
    let rows = read_reuse_samples_since(db.conn(), 0, limit).expect("read");

    // Assert: the three most-recently-inserted tied rows survive (markers
    // 20, 30, 40 -- dropping the earliest-inserted 10), emitted in stable
    // insertion order (oldest-first, rowid ascending) despite the shared
    // ts_start.
    assert_eq!(rows.len(), limit);
    let ts: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ts, vec![500, 500, 500]);
    let markers: Vec<i64> = rows.iter().map(|r| r.cache_read).collect();
    assert_eq!(markers, vec![20, 30, 40]);
}

#[test]
fn read_reuse_samples_excludes_non_ok_outcome() {
    // Arrange: a mid-stream-failed row (upstream_error) that still
    // carries a full triple and a non-null cache_read -- the divergence
    // case where the live path never records it (record_k_sample only
    // fires on the success finalize / natural stream EOS) but a
    // filter-less rebuild would replay it after a restart. An ok row in
    // the same window must still be admitted.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reuse_row(
        &db,
        "failed",
        100,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(42),
        "upstream_error",
    );
    insert_reuse_row(
        &db,
        "succeeded",
        110,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(7),
        "ok",
    );

    // Act
    let rows = read_reuse_samples_since(db.conn(), 0, 100).expect("read");

    // Assert: only the ok row survives.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts_start_ms, 110);
    assert_eq!(rows[0].cache_read, 7);
}

/// Insert a calibration row: an optional `would_trim_k_floor` (None ->
/// uncalibrated, still counts as future reuse) plus the (session_id,
/// provider_kind, model) triple and a `cache_read` snapshot. `ts_start`
/// drives the remaining-future ordering.
#[allow(clippy::too_many_arguments)]
fn insert_calib_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: &str,
    provider_kind: &str,
    model: &str,
    k_floor: Option<f64>,
    cache_read: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, would_trim_k_floor, cache_read) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, 'paid', ?4, ?5, \
             1, 'ok', 5, 0, 0, 1, 0, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider_kind,
                session_id,
                k_floor,
                cache_read,
            ],
        )
        .expect("insert calib row");
}

#[test]
fn k_calibration_coverage_uses_remaining_future_not_whole_session() {
    // Arrange: ONE session whose reuse is concentrated EARLY. Under the
    // old whole-session comparison every calibrated row would see the
    // group's total of 2 hits and all three would be "covered". Under the
    // remaining-future comparison a LATE over-prediction is correctly a
    // miss, because no reuse remains after it.
    //   r1 ts=100 hit,  floor=1.0  -> 1 future hit (r2)  -> covered (1>=1)
    //   r2 ts=200 hit,  UNCALIBRATED (feeds future reuse, not population)
    //   r3 ts=300 miss, floor=2.0  -> 0 future hits      -> MISS (0<2)
    //   r4 ts=400 miss, floor=0.5  -> 0 future hits      -> MISS (0<0.5)
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "r1", 100, "s1", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "r2", 200, "s1", "anth", "m1", None, 5);
    insert_calib_row(&db, "r3", 300, "s1", "anth", "m1", Some(2.0), 0);
    insert_calib_row(&db, "r4", 400, "s1", "anth", "m1", Some(0.5), 0);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: population is the 3 calibrated rows; remaining-future
    // coverage is 1/3 (whole-session would have been 3/3).
    assert_eq!(cal.n, 3, "only the calibrated rows form the population");
    assert!(
        (cal.coverage - 1.0 / 3.0).abs() < 1e-9,
        "remaining-future coverage must be 1/3, got {}",
        cal.coverage
    );
    // Per-row normalized errors: |1-1|/2=0, |2-0|/1=2, |0.5-0|/1=0.5;
    // sorted [0, 0.5, 2] -> median 0.5.
    assert!(
        (cal.accuracy - 0.5).abs() < 1e-9,
        "per-row-normalized median accuracy must be 0.5, got {}",
        cal.accuracy
    );
}

#[test]
fn k_calibration_hazard_decay_is_negative_for_decaying_session() {
    // Arrange: a 4-turn session whose reuse decays -- both first-half
    // turns reused, neither second-half turn did. first_rate=1.0,
    // second_rate=0.0 -> delta = -1.0. All rows calibrated so n>0 and the
    // main path computes hazard_decay.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "d0", 100, "sd", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "d1", 200, "sd", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "d2", 300, "sd", "anth", "m1", Some(1.0), 0);
    insert_calib_row(&db, "d3", 400, "sd", "anth", "m1", Some(1.0), 0);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: a material negative decay -- the age-conditioning trigger.
    assert!(
        (cal.hazard_decay + 1.0).abs() < 1e-9,
        "decaying session must yield hazard_decay = -1.0, got {}",
        cal.hazard_decay
    );
}

#[test]
fn k_calibration_hazard_decay_is_zero_for_flat_session() {
    // Arrange: a 4-turn session with a CONSTANT (flat) reuse rate -- every
    // turn reused. Both halves rate 1.0 -> delta 0.0.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for (i, ts) in [100, 200, 300, 400].into_iter().enumerate() {
        insert_calib_row(&db, &format!("f{i}"), ts, "sf", "anth", "m1", Some(1.0), 5);
    }

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert
    assert_eq!(cal.hazard_decay, 0.0, "flat reuse -> zero decay");
}

#[test]
fn k_calibration_hazard_decay_is_zero_when_no_group_has_enough_rows() {
    // Arrange: a session with fewer than HAZARD_DECAY_MIN_GROUP_ROWS rows
    // -- no group qualifies, so the halves would be too noisy to inform
    // the age-conditioning decision.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "g0", 100, "sg", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "g1", 200, "sg", "anth", "m1", Some(1.0), 0);
    insert_calib_row(&db, "g2", 300, "sg", "anth", "m1", Some(1.0), 5);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: no qualifying group -> hazard_decay defaults to 0.0.
    assert_eq!(cal.hazard_decay, 0.0);
}

#[test]
fn k_calibration_empty_db_is_all_zero_including_hazard_decay() {
    // Arrange: no rows at all.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: the n==0 early return zeroes every field.
    assert_eq!(cal.n, 0);
    assert_eq!(cal.coverage, 0.0);
    assert_eq!(cal.accuracy, 0.0);
    assert_eq!(cal.hazard_decay, 0.0);
}

/// Insert a row with an explicit `outcome` and (nullable) `resolved_class`
/// so the errors-by-class breakdown's classify/NULL paths are testable.
/// `model` is fixed so the group key is `(provider, upstream, alias)`.
#[allow(clippy::too_many_arguments)]
fn insert_class_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    provider: &str,
    upstream: &str,
    alias: &str,
    outcome: &str,
    resolved_class: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             resolved_class) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, 'm', ?4, ?5, 0, ?6, 5, \
             0, 0, 1, 0, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                alias,
                provider,
                upstream,
                outcome,
                resolved_class,
            ],
        )
        .expect("insert class row");
}

#[test]
fn errors_by_class_sums_to_errors_per_group_and_at_totals() {
    use std::collections::HashMap;

    // Arrange: two groups with a mix of ok / client_disconnect (excluded)
    // and classified / NULL-class error rows.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Group A (pa, ua): 4 errors across 3 classes incl. an unclassified.
    insert_class_row(&db, "a-ok", 100, "pa", "ua", "al", "ok", None);
    insert_class_row(
        &db,
        "a-cd",
        105,
        "pa",
        "ua",
        "al",
        "client_disconnect",
        None,
    );
    insert_class_row(
        &db,
        "a-e1",
        110,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("http-5xx"),
    );
    insert_class_row(
        &db,
        "a-e2",
        120,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("http-5xx"),
    );
    insert_class_row(&db, "a-e3", 130, "pa", "ua", "al", "gate_blocked", None);
    insert_class_row(
        &db,
        "a-e4",
        140,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("timeout"),
    );
    // Group B (pb, ub): 1 classified error.
    insert_class_row(
        &db,
        "b-e1",
        150,
        "pb",
        "ub",
        "al",
        "upstream_error",
        Some("rate-limited"),
    );

    // Act
    let agg = aggregate(&db, 0, 1000).expect("aggregate");
    let breakdown = errors_by_class(&db, 0, 1000).expect("errors_by_class");

    // Assert: per-group class counts sum EXACTLY to that group's errors.
    let mut per_group: HashMap<GroupKey, i64> = HashMap::new();
    for (key, _class, count) in &breakdown {
        *per_group.entry(key.clone()).or_default() += *count;
    }
    for row in &agg {
        let class_sum = per_group.get(&row.key).copied().unwrap_or(0);
        assert_eq!(
            class_sum, row.errors,
            "group {:?} class sum {class_sum} != errors {}",
            row.key, row.errors
        );
    }
    // Group A breakdown: http-5xx=2, unclassified=1, timeout=1.
    let a_key = agg
        .iter()
        .find(|r| r.key.provider.as_deref() == Some("pa"))
        .expect("group A")
        .key
        .clone();
    let a_classes: std::collections::BTreeMap<String, i64> = breakdown
        .iter()
        .filter(|(k, _, _)| *k == a_key)
        .map(|(_, c, n)| (c.clone(), *n))
        .collect();
    assert_eq!(a_classes.get("http-5xx"), Some(&2));
    assert_eq!(a_classes.get("unclassified"), Some(&1));
    assert_eq!(a_classes.get("timeout"), Some(&1));

    // Totals: the breakdown sums to the summed errors across all groups.
    let total_errors: i64 = agg.iter().map(|r| r.errors).sum();
    let total_breakdown: i64 = breakdown.iter().map(|(_, _, n)| *n).sum();
    assert_eq!(total_breakdown, total_errors);
    assert_eq!(total_breakdown, 5);
}

#[test]
fn errors_by_class_empty_window_returns_no_rows() {
    // Arrange: an ok row and a client_disconnect row (neither is an error),
    // plus an out-of-window error row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_class_row(&db, "ok", 100, "p", "u", "a", "ok", None);
    insert_class_row(&db, "cd", 110, "p", "u", "a", "client_disconnect", None);
    insert_class_row(
        &db,
        "out",
        5,
        "p",
        "u",
        "a",
        "upstream_error",
        Some("http-5xx"),
    );

    // Act: window [100, 1000) has zero qualifying error rows.
    let breakdown = errors_by_class(&db, 100, 1000).expect("errors_by_class");

    // Assert
    assert!(breakdown.is_empty());
}

#[test]
fn errors_by_class_uses_ts_start_index() {
    // The breakdown must ride idx_requests_ts_start for its window range,
    // not degrade to a full table scan. If this ever fails, add a covering
    // index rather than accepting the scan (see the decision doc).
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for i in 0..64 {
        insert_class_row(
            &db,
            &format!("e{i}"),
            100 + i,
            "p",
            "u",
            "a",
            "upstream_error",
            Some("http-5xx"),
        );
    }

    let plan: Vec<String> = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {ERRORS_BY_CLASS_SQL}"))
        .expect("prepare explain")
        .query_map([0_i64, 1000_i64], |row| row.get::<_, String>(3))
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    assert!(
        plan.iter().any(|d| d.contains("idx_requests_ts_start")),
        "breakdown query must use idx_requests_ts_start; plan was {plan:?}"
    );
}
