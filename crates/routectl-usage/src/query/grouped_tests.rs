//! Tests for the grouped, priced, deadline-bounded aggregate query.

use super::*;
use crate::db::open;
use std::time::Duration;
use tempfile::TempDir;

/// A far-future deadline: the query under test is expected to complete.
fn no_deadline() -> Instant {
    Instant::now() + Duration::from_mins(10)
}

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    (dir, db)
}

/// One ledger row's tunable columns. Defaults describe a minimal non-streaming
/// success with no token counters reported, so each test sets only the columns
/// its assertion depends on.
struct Fixture {
    request_id: &'static str,
    ts_start: i64,
    model: Option<&'static str>,
    provider: Option<&'static str>,
    upstream: Option<&'static str>,
    alias: &'static str,
    outcome: &'static str,
    stream: i64,
    latency_ms: i64,
    ttfb_ms: Option<i64>,
    fallback_count: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
    /// The raw `server_tool_use` JSON object, whose integer values sum into
    /// `server_tool_calls`.
    server_tool_use: Option<&'static str>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            request_id: "r",
            ts_start: 100,
            model: Some("m1"),
            provider: Some("pa"),
            upstream: Some("u1"),
            alias: "al",
            outcome: "ok",
            stream: 0,
            latency_ms: 10,
            ttfb_ms: None,
            fallback_count: 0,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            server_tool_use: None,
        }
    }
}

fn insert(db: &UsageDb, f: &Fixture) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, reasoning_tokens, \
             cache_read, cache_write_5m, cache_write_1h, server_tool_use) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, ?7, ?8, \
             ?9, ?10, 0, 0, 1, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![
                f.ts_start,
                f.request_id,
                f.alias,
                f.model,
                f.provider,
                f.upstream,
                f.stream,
                f.outcome,
                f.latency_ms,
                f.ttfb_ms,
                f.fallback_count,
                f.input_tokens,
                f.output_tokens,
                f.reasoning_tokens,
                f.cache_read,
                f.cache_write_5m,
                f.cache_write_1h,
                f.server_tool_use,
            ],
        )
        .expect("insert fixture row");
}

fn spec(group_by: GroupDim) -> QuerySpec {
    QuerySpec {
        from_ms: 0,
        to_ms: 10_000,
        group_by,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    }
}

/// Every row unpriced -- the pricing dimension is out of scope for the test.
fn unpriced(_row: &AggRow) -> RowCost {
    RowCost::Unpriced
}

fn group<'a>(result: &'a QueryResult, label: &str) -> &'a QueryGroup {
    result
        .groups
        .iter()
        .find(|g| g.label == label)
        .expect("group present")
}

fn assert_close(actual: Option<f64>, expected: f64) {
    let got = actual.expect("metric present");
    assert!(
        (got - expected).abs() < 1e-9,
        "expected {expected}, got {got}"
    );
}

// The hand-computed, series, grid and robustness groups live in sibling files
// to keep each file under the size ceiling. They compile into THIS module via
// `include!`, so the fixtures above stay in scope and no test's module path
// changes.

#[test]
fn empty_ledger_yields_no_groups_and_zeroed_totals() {
    // Arrange: an open ledger with no rows at all.
    let (_dir, db) = open_db();

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: zero groups, additive totals at zero, every derived metric
    // absent, and no column-type failure from an all-NULL aggregate row.
    assert!(result.groups.is_empty());
    assert_eq!(result.totals, QueryTotals::default());
    assert_eq!(result.totals.requests, 0);
    assert_eq!(result.totals.ttft_p50_ms, None);
    assert_eq!(result.totals.ttft_p95_ms, None);
    assert_eq!(result.totals.latency_p50_ms, None);
    assert_eq!(result.totals.latency_p95_ms, None);
    assert_eq!(result.totals.throughput_tok_s, None);
    assert_eq!(result.totals.ctx_avg, None);
    assert_eq!(result.totals.ctx_peak, None);
    assert_eq!(result.totals.cache_hit_pct, None);
    assert_eq!(result.totals.cost_usd, None);
    assert_eq!(result.totals.cost_status, CostStatus::Unpriced);
}

#[test]
fn rows_reporting_no_metrics_yield_absent_derived_metrics() {
    // Arrange: two non-streaming rows with NULL ttfb / tokens / cache columns.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            latency_ms: 40,
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "b",
            latency_ms: 60,
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: only the latency pair is derivable; every metric whose eligible
    // row count is zero is None rather than 0.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.requests, 2);
    assert_eq!(m.latency_p50_ms, Some(50));
    assert_eq!(m.latency_p95_ms, Some(60));
    assert_eq!(m.ttft_p50_ms, None);
    assert_eq!(m.ttft_p95_ms, None);
    assert_eq!(m.throughput_tok_s, None);
    assert_eq!(m.ctx_avg, None);
    assert_eq!(m.ctx_peak, None);
    assert_eq!(m.cache_hit_pct, None);
    assert_eq!(m.input_tokens, 0);
    assert_eq!(m.output_tokens, 0);
    assert_eq!(m.cache_read_billed, 0);
}

#[test]
fn zero_generation_window_is_excluded_from_throughput() {
    // Arrange: a streaming success whose latency equals its TTFB -- a zero
    // generation window, which must never become a division.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            stream: 1,
            latency_ms: 500,
            ttfb_ms: Some(500),
            output_tokens: Some(100),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: no eligible row for throughput; the TTFB metrics still resolve.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.throughput_tok_s, None);
    assert_eq!(m.ttft_p50_ms, Some(500));
    assert_eq!(m.ttft_p95_ms, Some(500));
}

#[test]
fn zero_prompt_total_is_excluded_from_cache_hit_rate() {
    // Arrange: a row that REPORTS a cache read of 0 with no other prompt
    // dimension -- a zero denominator that must not divide.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            cache_read: Some(0),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert
    assert_eq!(group(&result, "m1").metrics.cache_hit_pct, None);
}

#[test]
fn null_group_column_rolls_up_under_unattributed() {
    // Arrange: a pre-dispatch row with no served provider.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            provider: None,
            upstream: None,
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Provider), unpriced, no_deadline()).expect("query");

    // Assert
    assert_eq!(result.groups.len(), 1);
    assert_eq!(result.groups[0].label, "(unattributed)");
}

#[test]
fn alias_and_provider_filters_narrow_the_result() {
    // Arrange: two aliases across two providers.
    let (_dir, db) = open_db();
    for (id, alias, provider) in [
        ("a", "one", "pa"),
        ("b", "one", "pb"),
        ("c", "two", "pa"),
        ("d", "two", "pb"),
    ] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                alias,
                provider: Some(provider),
                ..Fixture::default()
            },
        );
    }

    // Act
    let by_alias = QuerySpec {
        alias_filter: Some("one".to_string()),
        ..spec(GroupDim::Provider)
    };
    let filtered = query(&db, &by_alias, unpriced, no_deadline()).expect("query");
    let both = QuerySpec {
        alias_filter: Some("one".to_string()),
        provider_filter: Some("pb".to_string()),
        ..spec(GroupDim::Provider)
    };
    let narrowed = query(&db, &both, unpriced, no_deadline()).expect("query");

    // Assert
    assert_eq!(filtered.totals.requests, 2);
    assert_eq!(filtered.groups.len(), 2);
    assert_eq!(narrowed.totals.requests, 1);
    assert_eq!(narrowed.groups.len(), 1);
    assert_eq!(narrowed.groups[0].label, "pb");
}

#[test]
fn out_of_window_rows_are_excluded() {
    // Arrange: one row before the window, one inside, one at the exclusive end.
    let (_dir, db) = open_db();
    for (id, ts) in [("a", 99), ("b", 150), ("c", 200)] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                ts_start: ts,
                ..Fixture::default()
            },
        );
    }

    // Act
    let windowed = QuerySpec {
        from_ms: 100,
        to_ms: 200,
        ..spec(GroupDim::Model)
    };
    let result = query(&db, &windowed, unpriced, no_deadline()).expect("query");

    // Assert: half-open window keeps only the middle row.
    assert_eq!(result.totals.requests, 1);
}

#[test]
fn all_priced_group_reports_priced_with_full_cost() {
    // Arrange
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            input_tokens: Some(1000),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Priced(1.5),
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Priced);
    assert_close(m.cost_usd, 1.5);
    assert_eq!(result.totals.cost_status, CostStatus::Priced);
    assert_close(result.totals.cost_usd, 1.5);
}

#[test]
fn all_subscription_group_reports_subscription_without_cost() {
    // Arrange
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Subscription(None),
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Subscription);
    assert_eq!(m.cost_usd, None);
    assert_eq!(m.equivalent_cost_usd, None);
}

#[test]
fn a_resolved_subscription_equivalent_never_reaches_the_priced_channel() {
    // Arrange
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Subscription(Some(12.34)),
        no_deadline(),
    )
    .expect("query");

    // Assert: the equivalent lands in its own field; real spend stays absent
    // and the status vocabulary is untouched by the payload.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Subscription);
    assert_eq!(m.cost_usd, None);
    assert_close(m.equivalent_cost_usd, 12.34);
    assert_eq!(result.totals.cost_usd, None);
    assert_close(result.totals.equivalent_cost_usd, 12.34);
}

#[test]
fn an_overflowed_equivalent_degrades_without_touching_the_priced_channel() {
    // Arrange: one priced row plus two subscription rows whose per-row
    // equivalents are each finite but whose SUM overflows -- only an
    // extreme-magnitude configured rate produces figures this large.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u-priced"), ("b", "u-sub-1"), ("c", "u-sub-2")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |row| match row.key.upstream.as_deref() {
            Some("u-priced") => RowCost::Priced(3.5),
            _ => RowCost::Subscription(Some(f64::MAX)),
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: the equivalent channel degrades to absent; the priced subtotal
    // and the status token are unaffected.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.equivalent_cost_usd, None);
    assert_close(m.cost_usd, 3.5);
    assert_eq!(m.cost_status, CostStatus::Partial);
}

#[test]
fn a_non_finite_per_row_equivalent_degrades_instead_of_panicking() {
    // Arrange: a pricing closure that hands the fold an already-infinite
    // per-row equivalent. This fold runs in debug builds under test and in a
    // network-reachable release build that aborts on panic, so the only
    // acceptable outcome in either profile is the channel's own degrade.
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Subscription(Some(f64::INFINITY)),
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.equivalent_cost_usd, None);
    assert_eq!(m.cost_status, CostStatus::Subscription);
    assert_eq!(result.totals.equivalent_cost_usd, None);
}

#[test]
fn an_overflowed_priced_sum_degrades_without_touching_the_equivalent_channel() {
    // Arrange: the mirror of the test above -- the priced channel overflows
    // while the equivalent resolves cleanly.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u-priced-1"), ("b", "u-priced-2"), ("c", "u-sub")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |row| match row.key.upstream.as_deref() {
            Some("u-sub") => RowCost::Subscription(Some(9.0)),
            _ => RowCost::Priced(f64::MAX),
        },
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_usd, None);
    assert_close(m.equivalent_cost_usd, 9.0);
}

#[test]
fn a_mixed_group_keeps_real_spend_and_notional_value_in_separate_fields() {
    // Arrange: one priced API row, one subscription row whose equivalent
    // resolved, one subscription row whose equivalent did not.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u-priced"), ("b", "u-sub-ok"), ("c", "u-sub-dark")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |row| match row.key.upstream.as_deref() {
            Some("u-priced") => RowCost::Priced(4.0),
            Some("u-sub-ok") => RowCost::Subscription(Some(7.5)),
            _ => RowCost::Subscription(None),
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: cost_usd is the priced subtotal ALONE -- 4.0, never 11.5 -- and
    // the equivalent counts only the subscription row that resolved.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.requests, 3);
    assert_eq!(m.cost_status, CostStatus::Partial);
    assert_close(m.cost_usd, 4.0);
    assert_close(m.equivalent_cost_usd, 7.5);
}

#[test]
fn mixed_pricing_group_reports_partial_with_priced_subtotal() {
    // Arrange: one model served by three upstreams -- one priced, one on a
    // subscription, one with no price at all.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u-priced"), ("b", "u-sub"), ("c", "u-unpriced")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act: pricing is decided per FINE row, i.e. per upstream.
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |row| match row.key.upstream.as_deref() {
            Some("u-priced") => RowCost::Priced(2.25),
            Some("u-sub") => RowCost::Subscription(None),
            _ => RowCost::Unpriced,
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: the mixed group reports the priced subtotal only.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.requests, 3);
    assert_eq!(m.cost_status, CostStatus::Partial);
    assert_close(m.cost_usd, 2.25);
    assert_eq!(result.totals.cost_status, CostStatus::Partial);
    assert_close(result.totals.cost_usd, 2.25);
}

#[test]
fn unpriced_group_reports_no_cost_rather_than_zero() {
    // Arrange
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Unpriced);
    assert_eq!(m.cost_usd, None);
}

#[test]
fn failed_streaming_row_with_a_stamped_ttfb_is_excluded_from_both_ttft_figures() {
    // Arrange: a streaming success at 100 ms and a stream that stamped a
    // first-byte time at 900 ms before failing mid-stream.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "ok-stream",
            stream: 1,
            latency_ms: 1100,
            ttfb_ms: Some(100),
            output_tokens: Some(200),
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "failed-stream",
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 1000,
            ttfb_ms: Some(900),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: both figures see only the ok stream, so they stay consistent --
    // the unfiltered mean would have read 500 against a p95 of 100.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.requests, 2);
    assert_eq!(m.ttft_p50_ms, Some(100));
    assert_eq!(m.ttft_p95_ms, Some(100));
    assert!(m.ttft_p50_ms <= m.ttft_p95_ms);
}

#[test]
fn group_with_no_ttft_eligible_row_reports_neither_ttft_figure() {
    // Arrange: a failed stream and a non-streaming success, both carrying a
    // stamped first-byte time -- neither is TTFT-eligible.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "failed-stream",
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 1000,
            ttfb_ms: Some(900),
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "non-stream",
            latency_ms: 500,
            ttfb_ms: Some(400),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: absent, never a mean over an ineligible population.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.requests, 2);
    assert_eq!(m.ttft_p50_ms, None);
    assert_eq!(m.ttft_p95_ms, None);
    assert_eq!(result.totals.ttft_p50_ms, None);
    assert_eq!(result.totals.ttft_p95_ms, None);
}

include!("grouped_hand_computed_tests.rs");
include!("grouped_series_tests.rs");
include!("grouped_series_grid_tests.rs");
include!("grouped_robustness_tests.rs");
