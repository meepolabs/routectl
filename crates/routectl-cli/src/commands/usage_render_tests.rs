// Render-layer, quota-line, `--since` title, and `--by model` tests for the
// `routectl usage` read surface. Split from `usage_tests.rs` to keep each
// file under the size ceiling; `include!`d into the same `tests` module so
// all helpers there (`temp_db`, `insert_row`, `cost_config`, `find`,
// `fixed_now`, ...) are in scope without duplication. All imports come from
// the enclosing `usage_tests.rs` (its `use super::*`); do not add `use` lines
// here.

// --- render layer ---

/// Compact wrapper over `insert_row` for the common priced-row shape
/// (model "m", provider "paid", upstream "up-paid", alias "al", outcome
/// "ok"). Keeps the render-layer tests dense and readable.
fn paid_row(db: &UsageDb, request_id: &str, input: Option<i64>, output: Option<i64>) {
    insert_row(
        db, request_id, 1000, "m", "paid", "up-paid", "al", "ok", input, output, 5,
    );
}

fn report_all(db: &UsageDb, config: &Config, by: Option<GroupDim>, detail: bool) -> WindowReport {
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    build_window_report(db, config, "t".into(), bounds, by, detail).unwrap()
}

/// A `paid` row with an explicit model and outcome, for grouping/error tests.
fn paid_model_row(db: &UsageDb, id: &str, model: &str, outcome: &str, input: i64, output: i64) {
    insert_row(
        db,
        id,
        1000,
        model,
        "paid",
        "up-paid",
        "al",
        outcome,
        Some(input),
        Some(output),
        5,
    );
}

#[test]
fn render_non_detail_header_has_expected_columns() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "h1", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);
    let header = out.lines().nth(1).expect("header line present");

    // Assert: default grouping is by model; standard columns follow. Dropped
    // and renamed columns must be absent.
    assert!(header.contains("model"));
    assert!(header.contains("reqs"));
    assert!(header.contains("err"));
    assert!(header.contains("input"));
    assert!(header.contains("output"));
    assert!(header.contains("reasoning"));
    assert!(header.contains("cache_read"));
    assert!(header.contains("cost"));
    for dropped in ["cache_rd", "scope", "p95_ms", "max_ms", "wall_ms"] {
        assert!(!header.contains(dropped), "header should not contain {dropped}");
    }
}

#[test]
fn render_by_provider_uses_provider_key_header() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "h2", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), false);

    // Act
    let out = render_report(&report);
    let header = out.lines().nth(1).expect("header line present");

    // Assert
    assert!(header.contains("provider"));
    assert!(!header.contains("scope"));
}

#[test]
fn render_data_row_shows_rolled_up_values_and_priced_cost() {
    // Arrange: 1_000_000 in @ $3 + 1_000_000 out @ $15 = $18.00.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "r1", Some(1_000_000), Some(1_000_000));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), false);

    // Act
    let out = render_report(&report);
    let data_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("paid"))
        .expect("data row present");

    // Assert: tokens are humanized (1_000_000 -> "1M") and cost is priced.
    assert!(data_line.contains("1M"));
    assert!(data_line.contains("$18.00"));
}

#[test]
fn render_detail_adds_extra_columns_non_detail_omits_them() {
    // Arrange: a streaming row so the detail columns have data.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "d1", 1000, "m", 120, 620, Some(50), Some(5), Some(7));
    let config = cost_config();
    let detail = report_all(&db, &config, None, true);
    let plain = report_all(&db, &config, None, false);

    // Act
    let detail_out = render_report(&detail);
    let plain_out = render_report(&plain);
    let detail_header = detail_out.lines().nth(1).expect("header");
    let plain_header = plain_out.lines().nth(1).expect("header");

    // Assert: detail header carries the new derived columns; non-detail omits
    // them, and the dropped latency columns appear nowhere.
    for col in [
        "ttft_p50",
        "ttft_p95",
        "tok/s",
        "cache_wr_5m",
        "cache_wr_1h",
        "srv_tools",
    ] {
        assert!(detail_header.contains(col), "detail header missing {col}");
        assert!(
            !plain_header.contains(col),
            "non-detail header should not contain {col}"
        );
    }
    for dropped in ["max_ms", "wall_ms", "p95_ms"] {
        assert!(
            !detail_header.contains(dropped),
            "detail header should not contain {dropped}"
        );
    }
    // ttft is rendered via human_ms (120ms), tok/s is present, and the
    // detail-only latency summary line appears.
    assert!(detail_out.contains("120ms"));
    assert!(detail_out.contains("latency: TTFT"));
    assert!(detail_out.contains("tok/s"));
}

#[test]
fn render_humanizes_large_cache_read_and_honors_not_reported() {
    // Arrange: a large reported cache_read (4_637_884 -> "4.6M") on a row that
    // reports reasoning=0, plus a second model whose reasoning is NULL.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "c1", 1000, "big", 100, 600, Some(10), Some(0), Some(4_637_884));
    insert_stream_row(&db, "c2", 1100, "nulls", 100, 600, Some(10), None, None);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let big_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("big"))
        .expect("big row present");
    let null_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("nulls"))
        .expect("nulls row present");

    // Assert: humanized cache_read; reported-0 reasoning shows "0"; NULL
    // reasoning shows "-".
    assert!(big_line.contains("4.6M"));
    assert!(big_line.contains(" 0 "), "reported-0 reasoning should show 0: {big_line:?}");
    assert!(null_line.contains(" - "), "NULL reasoning should show -: {null_line:?}");
}

#[test]
fn render_detail_latency_summary_uses_window_total_not_alias_named_total() {
    // Arrange: under `--by alias`, an alias literally named "total" must not be
    // mistaken for the injected window-total row. Two streaming rows under that
    // alias (ttfb 100, 300) plus one under another alias (ttfb 500). The window
    // total p95 (nearest-rank over all three: 100,300,500 -> rank 3 -> 500) must
    // differ from the "total" alias group's own p95 (100,300 -> rank 2 -> 300).
    let (_dir, _path, db) = temp_db();
    let insert_stream_alias = |id: &str, alias: &str, ttfb: i64| {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, output_tokens) \
                 VALUES (1000, 1000, ?1, 'openai', 'req-model', ?2, 'm', 'paid', \
                 'up-paid', 1, 'ok', ?3, ?4, 0, 0, 1, 0, 1)",
                rusqlite::params![id, alias, ttfb + 1000, ttfb],
            )
            .expect("insert stream alias row");
    };
    insert_stream_alias("ta", "total", 100);
    insert_stream_alias("tb", "total", 300);
    insert_stream_alias("oc", "other", 500);

    let report = report_all(&db, &cost_config(), Some(GroupDim::Alias), true);

    // Act
    let out = render_report(&report);
    let summary = out
        .lines()
        .find(|l| l.starts_with("latency: TTFT"))
        .expect("latency summary line present");

    // Assert: the summary's p95 reflects the window-wide value (500ms), not the
    // "total" alias group's own p95 (300ms) -- the injected window total is
    // selected by position, not by the label colliding with the alias name.
    assert!(
        summary.contains("p95 500ms"),
        "summary p95 should be the window total, not the alias named total: {summary:?}"
    );
}

#[test]
fn render_includes_total_row() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "t1", "m", "ok", 10, 20);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);

    // Assert: a total row is always present.
    assert!(out.lines().any(|l| l.trim_start().starts_with("total")));
}

#[test]
fn render_footer_shows_cache_hit_rate() {
    // Arrange: cache_read 0, input 300 => rate 0.0%.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "f1", Some(300), Some(10));
    paid_model_row(&db, "f2", "m", "upstream_error", 0, 0);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert: errors moved to a column; the footer is the cache-hit line only.
    assert!(out.contains("cache hit 0.0%"));
    assert!(!out.contains("errors:"));
}

#[test]
fn render_footer_na_rate_when_no_tokens() {
    // Arrange: NULL input, no cache => denominator 0 => rate None => "n/a".
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "f3", None, None);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert
    assert!(out.contains("cache hit n/a"));
}

#[test]
fn render_table_left_aligns_key_and_right_aligns_numeric() {
    // Arrange: a short label ("paid") under a wider header ("provider") forces
    // padding so alignment direction is observable.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "a1", Some(7), Some(9));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), false);

    // Act
    let out = render_report(&report);
    let data_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("paid"))
        .expect("data row");

    // Assert: column 0 is left-aligned -- the short label sits at the line start
    // and is right-padded to the "provider" width (so trailing spaces follow).
    assert!(data_line.starts_with("paid"));
    assert!(
        data_line.starts_with("paid    "),
        "column 0 should be left-aligned and padded: {data_line:?}"
    );
    // Numeric column is right-aligned: the reqs value (1) renders flush against
    // the header width "reqs" (4) -- i.e. preceded by padding, not trailing.
    assert!(
        data_line.contains("   1 "),
        "reqs cell should be right-justified: {data_line:?}"
    );
}

// --- quota line ---

#[allow(clippy::too_many_arguments)]
fn insert_quota_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    quota_status: &str,
    quota_utilization: f64,
    quota_overage_status: &str,
    quota_reset_s: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, quota_status, quota_utilization, \
             quota_overage_status, quota_reset) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', 'sub', 'up-sub', 0, 'ok', \
             10, 0, 0, 1, 0, 100, 50, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                ts_start,
                request_id,
                quota_status,
                quota_utilization,
                quota_overage_status,
                quota_reset_s,
            ],
        )
        .expect("insert quota row");
}

#[test]
fn build_report_surfaces_latest_quota_snapshot() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    insert_quota_row(&db, "q1", 1000, "allowed", 0.22, "none", 1_800_000_000);

    // Act
    let report = report_all(&db, &cost_config(), None, false);

    // Assert
    let quota = report.quota.expect("quota snapshot present");
    assert_eq!(quota.status.as_deref(), Some("allowed"));
    assert_eq!(quota.utilization, Some(0.22));
    assert_eq!(quota.overage_status.as_deref(), Some("none"));
    assert_eq!(quota.reset, Some(1_800_000_000));
}

#[test]
fn render_report_includes_quota_line() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    insert_quota_row(&db, "q2", 1000, "allowed", 0.22, "none", 1_800_000_000);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);
    let quota_line = out
        .lines()
        .find(|l| l.starts_with("quota:"))
        .expect("quota line present");

    // Assert: 0.22 renders as a 0-decimal percent (22%).
    assert!(quota_line.contains("status=allowed"));
    assert!(quota_line.contains("utilization=22%"));
    assert!(quota_line.contains("overage=none"));
    assert!(quota_line.contains("reset="));
}

#[test]
fn format_reset_renders_local_timestamp_shape() {
    // Arrange: a fixed epoch-seconds value. The exact wall-clock depends on the
    // host TZ, so assert the canonical shape rather than a hardcoded string.
    let epoch_s = 1_800_000_000i64;

    // Act
    let rendered = format_reset(epoch_s);

    // Assert
    assert!(!rendered.is_empty());
    let bytes = rendered.as_bytes();
    // Shape: YYYY-MM-DD HH:MM (positions of digits and separators).
    assert_eq!(rendered.len(), 16, "unexpected reset format: {rendered:?}");
    assert!(bytes[4] == b'-' && bytes[7] == b'-');
    assert!(bytes[10] == b' ');
    assert!(bytes[13] == b':');
    assert!(rendered[0..4].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[5..7].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[8..10].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[11..13].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[14..16].chars().all(|c| c.is_ascii_digit()));
}

// --- --since title formatting ---

#[test]
fn since_with_until_builds_single_range_titled_block() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: Some("2026-06-01".to_string()),
        until: Some("2026-06-05".to_string()),
        by: None,
        detail: false,
        db: None,
    };

    // Act
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();

    // Assert
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("== 2026-06-01 .. 2026-06-05 =="));
}

#[test]
fn since_without_until_builds_single_open_ended_block() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: Some("2026-06-01".to_string()),
        until: None,
        by: None,
        detail: false,
        db: None,
    };

    // Act
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();

    // Assert
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("== since 2026-06-01 =="));
}

// --- --by model + (none) fallback ---

#[test]
fn by_model_groups_rows_sharing_a_model() {
    // Arrange: two rows with the same model "claude-x".
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "m1", "claude-x", "ok", 10, 20);
    paid_model_row(&db, "m2", "claude-x", "ok", 5, 7);

    // Act
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Assert: one model group plus the always-appended total row.
    assert_eq!(report.rows.len(), 2);
    let row = find(&report, "claude-x");
    assert_eq!(row.requests, 2);
    assert_eq!(row.input_tokens, 15);
    assert_eq!(row.output_tokens, 27);
}

#[test]
fn by_model_null_model_falls_back_to_unattributed_label() {
    // Arrange: a row with a NULL model column. group_label's fallback must
    // place it under the "(unattributed)" group.
    let (_dir, _path, db) = temp_db();
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens) \
             VALUES (1000, 1000, 'nm1', 'openai', 'req-model', 'al', NULL, 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, 8, 9)",
            [],
        )
        .expect("insert null-model row");

    // Act
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Assert
    let row = find(&report, "(unattributed)");
    assert_eq!(row.requests, 1);
    assert_eq!(row.input_tokens, 8);
}
