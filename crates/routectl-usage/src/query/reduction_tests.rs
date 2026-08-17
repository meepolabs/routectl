// The lossless-minifier (context-reduction) outcome reader tests. Split from
// `tests.rs` to keep each file under the size ceiling; `include!`d into the
// same `tests` module so the helpers there stay in scope. All imports come
// from the host `tests.rs` -- do not add `use` lines here.

/// Insert a row carrying the v15 reduction columns, or a pre-column / no-target
/// row when `decision` is `None` -- the latter must never contribute to
/// `reduction_summary` totals.
#[allow(clippy::too_many_arguments)]
fn insert_reduction_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    decision: Option<&str>,
    compressed: Option<i64>,
    skipped: Option<i64>,
    rejected: Option<i64>,
    bytes_saved: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, reduction_decision, \
             reduction_strings_compressed, reduction_strings_skipped, \
             reduction_strings_rejected, reduction_bytes_saved) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
             ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                decision,
                compressed,
                skipped,
                rejected,
                bytes_saved,
            ],
        )
        .expect("insert reduction row");
}

fn decision_count(summary: &ReductionSummary, token: &str) -> i64 {
    summary
        .decisions
        .iter()
        .find(|(t, _)| t == token)
        .map_or(0, |(_, c)| *c)
}

#[test]
fn reduction_summary_sums_counters_and_histograms_mixed_decisions() {
    // Arrange: three decided rows across two tokens, one NULL-decision row
    // (pre-column history) carrying counter values that must NOT be summed,
    // and one out-of-window decided row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(
        &db,
        "r-applied-1",
        100,
        Some("applied"),
        Some(4),
        Some(2),
        Some(0),
        Some(512),
    );
    insert_reduction_row(
        &db,
        "r-applied-2",
        110,
        Some("applied"),
        Some(1),
        Some(3),
        Some(0),
        Some(256),
    );
    insert_reduction_row(
        &db,
        "r-nothing",
        120,
        Some("skipped:nothing-to-strip"),
        Some(0),
        Some(7),
        Some(0),
        Some(0),
    );
    insert_reduction_row(
        &db,
        "r-old",
        130,
        None,
        Some(999),
        Some(999),
        Some(999),
        Some(999_999),
    );
    insert_reduction_row(
        &db,
        "r-out",
        5,
        Some("applied"),
        Some(50),
        Some(50),
        Some(50),
        Some(50_000),
    );

    // Act
    let s = reduction_summary(&db, 100, 1000).expect("summary");

    // Assert: only the three in-window decided rows contribute.
    assert_eq!(s.decided_requests, 3);
    assert_eq!(decision_count(&s, "applied"), 2);
    assert_eq!(decision_count(&s, "skipped:nothing-to-strip"), 1);
    assert_eq!(s.strings_compressed, 5);
    assert_eq!(s.strings_skipped, 12);
    assert_eq!(s.strings_rejected, 0);
    assert_eq!(s.bytes_saved, 768);
    // est_tokens is derived on read, never stored: 768 / 4.
    assert_eq!(s.est_tokens_saved(), 192);
}

#[test]
fn reduction_summary_excludes_null_decision_rows_from_the_histogram() {
    // Arrange: only NULL-decision rows, one of which carries counters.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(&db, "n1", 100, None, None, None, None, None);
    insert_reduction_row(&db, "n2", 110, None, Some(3), Some(3), Some(3), Some(3_000));

    // Act
    let s = reduction_summary(&db, 0, 1000).expect("summary");

    // Assert: a NULL decision is not an outcome, so nothing is reported.
    assert_eq!(s, ReductionSummary::default());
    assert!(s.decisions.is_empty());
    assert_eq!(s.est_tokens_saved(), 0);
}

#[test]
fn reduction_summary_counts_decided_rows_with_null_counters_as_zero() {
    // Arrange: a decided row whose four counters are NULL. It belongs in the
    // histogram (the decision is known) but contributes 0 to every sum.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(
        &db,
        "d-null-counters",
        100,
        Some("skipped:disabled"),
        None,
        None,
        None,
        None,
    );

    // Act
    let s = reduction_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s.decided_requests, 1);
    assert_eq!(decision_count(&s, "skipped:disabled"), 1);
    assert_eq!(s.strings_compressed, 0);
    assert_eq!(s.strings_skipped, 0);
    assert_eq!(s.strings_rejected, 0);
    assert_eq!(s.bytes_saved, 0);
}

#[test]
fn reduction_summary_reports_an_unknown_decision_token() {
    // Arrange: the token vocabulary is additive-forever, so a token this build
    // has never seen must still surface rather than be dropped or remapped.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(
        &db,
        "future",
        100,
        Some("skipped:some-future-reason"),
        Some(0),
        Some(1),
        Some(0),
        Some(0),
    );

    // Act
    let s = reduction_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s.decided_requests, 1);
    assert_eq!(decision_count(&s, "skipped:some-future-reason"), 1);
}

#[test]
fn reduction_summary_surfaces_a_nonzero_rejected_count() {
    // Arrange: `rejected` is structurally unreachable with the current
    // minifier, so a nonzero value is a defect signal. The reader must pass it
    // through verbatim rather than swallowing it.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(
        &db,
        "rej",
        100,
        Some("applied"),
        Some(2),
        Some(0),
        Some(3),
        Some(64),
    );

    // Act
    let s = reduction_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s.strings_rejected, 3);
}

#[test]
fn reduction_summary_on_empty_ledger_returns_all_zeros() {
    // Arrange: a healthy but EMPTY ledger.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act
    let s = reduction_summary(&db, 0, 1000).expect("summary over empty ledger");

    // Assert
    assert_eq!(s, ReductionSummary::default());
}

#[test]
fn reduction_summary_over_readonly_open_matches_seeded_results() {
    // Arrange: seed decided rows, then drop the writer handle.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reduction_row(
        &db,
        "ro1",
        100,
        Some("applied"),
        Some(2),
        Some(1),
        Some(0),
        Some(400),
    );
    insert_reduction_row(
        &db,
        "ro2",
        110,
        Some("skipped:no-tail"),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
    );
    drop(db);

    // Act
    let ro = open_readonly(&path).expect("open readonly");
    let s = reduction_summary(&ro, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s.decided_requests, 2);
    assert_eq!(decision_count(&s, "applied"), 1);
    assert_eq!(decision_count(&s, "skipped:no-tail"), 1);
    assert_eq!(s.bytes_saved, 400);
    assert_eq!(s.est_tokens_saved(), 100);
}
