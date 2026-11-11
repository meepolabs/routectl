//! Cache writes read back identically on the three surfaces the operator reads
//! them from: the `routectl usage` report, the `/status/usage` panel and the
//! `/status/query` aggregate. One ledger mixes rows written before the ledger
//! recorded a write total (the 5-minute and 1-hour columns) with rows carrying
//! the total, so a surface that reads only one era reports a different figure.

use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use routectl_router::Config;
use routectl_usage::{GroupDim, QueryResult, QuerySpec, RowCost, open, query};
use tempfile::TempDir;

use super::*;
use crate::commands::pricing::PriceSnapshot;
use crate::commands::usage::{GroupDim as ReportDim, UsageArgs, build_output};

const FROM_MS: i64 = 0;
const TO_MS: i64 = 1_000_000;
const ROW_TS: i64 = 1_000;

/// One ledger row's cache-write columns, each NULL when `None`.
#[derive(Default)]
struct Writes {
    legacy_5m: Option<i64>,
    legacy_1h: Option<i64>,
    total: Option<i64>,
    lifetimes: Option<&'static str>,
}

fn insert(db: &UsageDb, request_id: &str, model: &str, writes: &Writes) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, cache_write_5m, cache_write_1h, \
             cache_write_tokens, cache_write_lifetimes) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, 'p', 'u', 0, \
             'ok', 5, 0, 0, 1, 0, 10, 10, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ROW_TS,
                request_id,
                model,
                writes.legacy_5m,
                writes.legacy_1h,
                writes.total,
                writes.lifetimes,
            ],
        )
        .expect("insert cache-write row");
}

/// The three surfaces over one ledger: the `routectl usage --all --by model
/// --detail` text, the `/status/usage` panel and the `/status/query` result.
fn three_surfaces(db: &UsageDb) -> (String, UsagePanel, QueryResult) {
    let now = Local
        .timestamp_millis_opt(TO_MS - 1)
        .single()
        .expect("a representable instant");
    let args = UsageArgs {
        window: WindowFlag::All,
        since: None,
        until: None,
        by: Some(ReportDim::Model),
        detail: true,
        db: None,
        k_calibration: false,
    };
    let report = build_output(
        db,
        &Config::default(),
        &PriceSnapshot::default(),
        &args,
        now,
    )
    .expect("usage report");

    let bounds = WindowBounds {
        from_ms: FROM_MS,
        to_ms: TO_MS,
    };
    let deadline = Instant::now() + Duration::from_mins(10);
    let panel = collect(db, "all", bounds, LedgerHealth::default(), deadline).expect("usage panel");
    let spec = QuerySpec {
        from_ms: FROM_MS,
        to_ms: TO_MS,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let result = query(db, &spec, |_| RowCost::Unpriced, deadline).expect("query");
    (report, panel, result)
}

/// The `cache_wr` cell of the report row labelled `label`, located through the
/// header so a column reorder cannot silently move the assertion.
fn report_cache_wr(report: &str, label: &str) -> String {
    let mut lines = report.lines().skip(1);
    let header: Vec<&str> = lines
        .next()
        .expect("report header")
        .split_whitespace()
        .collect();
    let column = header
        .iter()
        .position(|h| *h == "cache_wr")
        .expect("detail header carries cache_wr");
    let row = lines
        .find(|l| l.split_whitespace().next() == Some(label))
        .unwrap_or_else(|| panic!("report row `{label}` present: {report}"));
    row.split_whitespace()
        .nth(column)
        .unwrap_or_else(|| panic!("row `{label}` has a cache_wr cell: {row}"))
        .to_string()
}

fn panel_group<'a>(panel: &'a UsagePanel, model: &str) -> &'a UsageGroup {
    panel
        .groups
        .iter()
        .find(|g| g.model.as_deref() == Some(model))
        .unwrap_or_else(|| panic!("panel group `{model}` present"))
}

fn query_group<'a>(result: &'a QueryResult, label: &str) -> &'a routectl_usage::QueryMetrics {
    &result
        .groups
        .iter()
        .find(|g| g.label == label)
        .unwrap_or_else(|| panic!("query group `{label}` present"))
        .metrics
}

#[test]
fn every_surface_reports_the_same_both_era_cache_write_total() {
    // Arrange: older-era rows carry 30 + 20 and 7 in the per-lifetime columns;
    // current rows carry totals of 400 (with a split) and 2000 (without one).
    // Both-era total = 50 + 7 + 400 + 2000 = 2457, below the report's compact
    // floor so it renders exactly. The older-era columns alone sum to 57 and the
    // current totals alone to 2400, so a one-era reader shows neither 2457.
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open ledger");
    let older = |five, one| Writes {
        legacy_5m: five,
        legacy_1h: one,
        ..Writes::default()
    };
    let current = |total, lifetimes| Writes {
        total: Some(total),
        lifetimes,
        ..Writes::default()
    };
    insert(&db, "older-both", "m", &older(Some(30), Some(20)));
    insert(&db, "older-5m-only", "m", &older(Some(7), None));
    insert(
        &db,
        "current-split",
        "m",
        &current(400, Some(r#"{"300":400}"#)),
    );
    insert(&db, "current-total-only", "n", &current(2000, None));

    // Act
    let (report, panel, result) = three_surfaces(&db);

    // Assert
    assert_eq!(report_cache_wr(&report, "total"), "2457", "{report}");
    assert_eq!(panel.totals.cache_write_tokens, 2457, "usage panel totals");
    assert_eq!(panel.totals.cache_write_present, 4, "usage panel totals");
    assert_eq!(
        result.totals.cache_write_tokens, 2457,
        "status query totals"
    );
    assert_eq!(result.totals.cache_write_present, 4, "status query totals");
    assert_eq!(report_cache_wr(&report, "m"), "457", "{report}");
    assert_eq!(panel_group(&panel, "m").cache_write_tokens, 457);
    assert_eq!(query_group(&result, "m").cache_write_tokens, 457);
}

#[test]
fn absent_writes_render_dash_and_an_observed_zero_renders_zero() {
    // Arrange: model `absent` reports no write in either era; `zero` reports an
    // observed zero total; `zero-older` an observed zero in an older-era column.
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open ledger");
    insert(&db, "a", "absent", &Writes::default());
    insert(
        &db,
        "z",
        "zero",
        &Writes {
            total: Some(0),
            ..Writes::default()
        },
    );
    insert(
        &db,
        "zo",
        "zero-older",
        &Writes {
            legacy_1h: Some(0),
            ..Writes::default()
        },
    );

    // Act
    let (report, panel, result) = three_surfaces(&db);

    // Assert
    for (label, cell, present) in [("absent", "-", 0), ("zero", "0", 1), ("zero-older", "0", 1)] {
        assert_eq!(
            report_cache_wr(&report, label),
            cell,
            "report {label}: {report}"
        );
        let group = panel_group(&panel, label);
        assert_eq!(group.cache_write_present, present, "panel {label}");
        assert_eq!(group.cache_write_tokens, 0, "panel {label}");
        let metrics = query_group(&result, label);
        assert_eq!(metrics.cache_write_present, present, "query {label}");
        assert_eq!(metrics.cache_write_tokens, 0, "query {label}");
    }
}
