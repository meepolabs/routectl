// The would-trim / M1-attribution / per-seat-quota / earliest-timestamp
// readers, plus the read-only-open equivalence pins. Split from `tests.rs` to
// keep each file under the size ceiling; `include!`d into the same `tests`
// module so the helpers there stay in scope. All imports come from the host
// `tests.rs` -- do not add `use` lines here.

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
