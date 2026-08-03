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
        |_row| RowCost::Subscription,
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Subscription);
    assert_eq!(m.cost_usd, None);
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
            Some("u-sub") => RowCost::Subscription,
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

/// Seed the hand-computed fixture: one model served over two upstreams, mixing
/// a streaming success with a wide generation window, a streaming success with a
/// narrow one, a non-streaming cache-reporting success, and a pre-token error.
fn seed_hand_computed(db: &UsageDb) {
    insert(
        db,
        &Fixture {
            request_id: "r1",
            upstream: Some("u1"),
            stream: 1,
            latency_ms: 1100,
            ttfb_ms: Some(100),
            input_tokens: Some(1000),
            output_tokens: Some(200),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "r2",
            upstream: Some("u1"),
            stream: 1,
            latency_ms: 800,
            ttfb_ms: Some(300),
            input_tokens: Some(3000),
            output_tokens: Some(250),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "r3",
            upstream: Some("u2"),
            latency_ms: 600,
            input_tokens: Some(2000),
            output_tokens: Some(100),
            cache_read: Some(500),
            cache_write_5m: Some(100),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "r4",
            upstream: Some("u2"),
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 200,
            ..Fixture::default()
        },
    );
}

#[test]
fn hand_computed_fixture_matches_every_derived_metric() {
    // Arrange
    let (_dir, db) = open_db();
    seed_hand_computed(&db);

    // Act: u1 is priced at $1 per 1,000 input tokens; u2 has no price.
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |row| {
            if row.key.upstream.as_deref() == Some("u1") {
                RowCost::Priced(row.input_tokens as f64 / 1000.0)
            } else {
                RowCost::Unpriced
            }
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: one coarse group folded from the two fine (upstream) rows.
    assert_eq!(result.groups.len(), 1);
    let m = &group(&result, "m1").metrics;

    // Additive: 4 requests, 3 ok, 1 error (client disconnects excluded).
    assert_eq!(m.requests, 4);
    assert_eq!(m.ok, 3);
    assert_eq!(m.errors, 1);
    assert_eq!(m.client_disconnect_total, 0);
    // 1000 + 3000 + 2000 + NULL(0); 200 + 250 + 100 + NULL(0).
    assert_eq!(m.input_tokens, 6000);
    assert_eq!(m.output_tokens, 550);
    assert_eq!(m.cache_read_billed, 500);
    assert_eq!(m.cache_write_5m, 100);
    // r1, r2, r4 carry stream = 1.
    assert_eq!(m.stream_count, 3);

    // ttft: r1 and r2 are the only streaming successes with a TTFB, so both
    // figures share that population -- p50 = (100 + 300) / 2, p95 = MAX.
    assert_eq!(m.ttft_p50_ms, Some(200));
    assert_eq!(m.ttft_p95_ms, Some(300));
    // latency p50 = (1100 + 800 + 600 + 200) / 4 requests = 2700 / 4.
    assert_eq!(m.latency_p50_ms, Some(675));
    assert_eq!(m.latency_p95_ms, Some(1100));
    // throughput: r1 = 200 tok / 1000 ms = 200 tok/s, r2 = 250 / 500 ms = 500
    // tok/s; r3 is non-streaming and r4 has no TTFB, so the request-weighted
    // mean is over 2 eligible rows.
    assert_close(m.throughput_tok_s, 350.0);
    // ctx: 6000 input tokens over the 3 rows that reported one; peak = 3000.
    assert_eq!(m.ctx_avg, Some(2000));
    assert_eq!(m.ctx_peak, Some(3000));
    // cache-hit: only r3 reports a cache read. Prompt total = 2000 input + 500
    // read + 100 write = 2600, so the single eligible row's share is 500/2600.
    assert_close(m.cache_hit_pct, 500.0 / 2600.0 * 100.0);
    // cost: u1's fine row summed 4000 input tokens -> $4.00; u2 is unpriced, so
    // the group is partial and reports the priced subtotal only.
    assert_eq!(m.cost_status, CostStatus::Partial);
    assert_close(m.cost_usd, 4.0);

    // Totals are folded from the group rows, so they match exactly.
    assert_eq!(&result.totals, m);
}

#[test]
fn coarse_grouping_maxes_across_fine_rows_without_averaging_averages() {
    // Arrange: the same fixture grouped by alias, which folds BOTH upstreams
    // and both providers into one row.
    let (_dir, db) = open_db();
    seed_hand_computed(&db);
    insert(
        &db,
        &Fixture {
            request_id: "r5",
            model: Some("m2"),
            provider: Some("pb"),
            upstream: Some("u3"),
            stream: 1,
            latency_ms: 5000,
            ttfb_ms: Some(4000),
            input_tokens: Some(9000),
            output_tokens: Some(1000),
            ..Fixture::default()
        },
    );

    // Act
    let by_model = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");
    let by_alias = query(&db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");

    // Assert: the maxima are a MAX over the fine maxima, not a re-derivation.
    assert_eq!(by_model.groups.len(), 2);
    assert_eq!(by_alias.groups.len(), 1);
    let alias_metrics = &by_alias.groups[0].metrics;
    assert_eq!(alias_metrics.latency_p95_ms, Some(5000));
    assert_eq!(alias_metrics.ttft_p95_ms, Some(4000));
    assert_eq!(alias_metrics.ctx_peak, Some(9000));
    // Both groupings cover the same rows, so the totals must agree.
    assert_eq!(by_model.totals, by_alias.totals);
    assert_eq!(alias_metrics, &by_alias.totals);
}

#[test]
fn fallback_served_counts_rows_that_needed_a_fallback_and_sums_to_totals() {
    // Arrange: two models over three upstreams. Two rows needed a fallback --
    // one of them three attempts deep, which must still count as ONE served
    // request -- and two were served first try.
    let (_dir, db) = open_db();
    for (id, model, upstream, fallback_count) in [
        ("a", "m1", "u1", 1),
        ("b", "m1", "u2", 3),
        ("c", "m1", "u2", 0),
        ("d", "m2", "u3", 0),
    ] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                model: Some(model),
                upstream: Some(upstream),
                fallback_count,
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: a count of rows, not a sum of fallback attempts, folded per group
    // and reconciling at totals.
    assert_eq!(group(&result, "m1").metrics.fallback_served, 2);
    assert_eq!(group(&result, "m2").metrics.fallback_served, 0);
    assert_eq!(result.totals.fallback_served, 2);
    assert_eq!(
        result.totals.fallback_served,
        result
            .groups
            .iter()
            .map(|g| g.metrics.fallback_served)
            .sum::<i64>()
    );
}

/// The same window as [`spec`], bucketed on a `width_ms` grid anchored at its
/// lower bound.
fn bucketed(group_by: GroupDim, width_ms: i64, count: usize) -> QuerySpec {
    QuerySpec {
        bucket: Some(BucketSpec { width_ms, count }),
        ..spec(group_by)
    }
}

fn series(result: &QueryResult) -> &QuerySeries {
    result.series.as_ref().expect("series present")
}

/// Every additive metric, in one tuple, so a test can compare a bucket sum to
/// the totals across all of them at once.
fn additive(m: &QueryMetrics) -> [i64; 13] {
    [
        m.requests,
        m.ok,
        m.errors,
        m.input_tokens,
        m.output_tokens,
        m.reasoning_tokens,
        m.cache_read_billed,
        m.cache_write_5m,
        m.cache_write_1h,
        m.server_tool_calls,
        m.stream_count,
        m.client_disconnect_total,
        m.fallback_served,
    ]
}

#[test]
fn bucket_absent_yields_no_series_and_leaves_the_groups_untouched() {
    // Arrange
    let (_dir, db) = open_db();
    seed_hand_computed(&db);

    // Act
    let result = query(&db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: the unbucketed path answers exactly as it did before the series
    // existed, with the series explicitly absent rather than empty.
    assert!(result.series.is_none());
    assert_eq!(result.totals.requests, 4);
    assert_eq!(result.groups.len(), 1);
}

#[test]
fn bucketed_query_folds_the_same_rows_into_groups_and_a_dense_series() {
    // Arrange: three rows spread over a 10-bucket grid of 1000 ms each.
    let (_dir, db) = open_db();
    for (id, ts) in [("a", 100), ("b", 1500), ("c", 9500)] {
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
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 10),
        unpriced,
        no_deadline(),
    )
    .expect("query");

    // Assert: the coarse fold is unchanged, and the series covers the whole grid
    // with each row landing in the bucket its timestamp falls in.
    assert_eq!(result.totals.requests, 3);
    assert_eq!(group(&result, "m1").metrics.requests, 3);
    let s = series(&result);
    assert_eq!(s.bucket_ms, 1000);
    assert_eq!(s.buckets.len(), 10);
    let starts: Vec<i64> = s.buckets.iter().map(|b| b.start_ms).collect();
    assert_eq!(starts, (0..10).map(|i| i * 1000).collect::<Vec<_>>());
    let counts: Vec<i64> = s.buckets.iter().map(|b| b.metrics.requests).collect();
    assert_eq!(counts, vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
}

#[test]
fn every_additive_bucket_metric_sums_to_the_window_totals() {
    // Arrange: six rows, one per bucket of a six-bucket grid, populating EVERY
    // additive field with a distinct non-zero total -- a dropped or zeroed field
    // in either fold has to change one of the thirteen expected numbers.
    let (_dir, db) = open_db();
    let rows = [
        Fixture {
            request_id: "r1",
            ts_start: 100,
            outcome: "ok",
            stream: 1,
            latency_ms: 1100,
            ttfb_ms: Some(100),
            fallback_count: 2,
            input_tokens: Some(1000),
            output_tokens: Some(200),
            reasoning_tokens: Some(50),
            cache_read: Some(400),
            cache_write_5m: Some(100),
            cache_write_1h: Some(700),
            server_tool_use: Some(r#"{"web_search":3,"code_exec":2}"#),
            ..Fixture::default()
        },
        Fixture {
            request_id: "r2",
            ts_start: 1500,
            outcome: "ok",
            stream: 1,
            latency_ms: 500,
            ttfb_ms: Some(60),
            fallback_count: 1,
            input_tokens: Some(11),
            output_tokens: Some(13),
            reasoning_tokens: Some(17),
            cache_read: Some(19),
            cache_write_5m: Some(23),
            cache_write_1h: Some(29),
            server_tool_use: Some(r#"{"web_search":4}"#),
            ..Fixture::default()
        },
        Fixture {
            request_id: "r3",
            ts_start: 2500,
            outcome: "ok",
            stream: 1,
            latency_ms: 300,
            input_tokens: Some(31),
            output_tokens: Some(37),
            reasoning_tokens: Some(41),
            cache_read: Some(43),
            cache_write_5m: Some(47),
            cache_write_1h: Some(53),
            server_tool_use: Some(r#"{"web_search":6}"#),
            ..Fixture::default()
        },
        Fixture {
            request_id: "r4",
            ts_start: 3500,
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 200,
            fallback_count: 3,
            input_tokens: Some(61),
            output_tokens: Some(67),
            reasoning_tokens: Some(71),
            cache_read: Some(73),
            cache_write_5m: Some(79),
            cache_write_1h: Some(83),
            server_tool_use: Some(r#"{"web_search":7}"#),
            ..Fixture::default()
        },
        Fixture {
            request_id: "r5",
            ts_start: 4500,
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 150,
            fallback_count: 4,
            input_tokens: Some(89),
            output_tokens: Some(97),
            reasoning_tokens: Some(101),
            cache_read: Some(103),
            cache_write_5m: Some(107),
            cache_write_1h: Some(109),
            server_tool_use: Some(r#"{"web_search":8}"#),
            ..Fixture::default()
        },
        Fixture {
            request_id: "r6",
            ts_start: 5500,
            outcome: "client_disconnect",
            latency_ms: 30,
            input_tokens: Some(113),
            output_tokens: Some(127),
            reasoning_tokens: Some(131),
            cache_read: Some(137),
            cache_write_5m: Some(139),
            cache_write_1h: Some(149),
            server_tool_use: Some(r#"{"web_search":9}"#),
            ..Fixture::default()
        },
    ];
    for row in &rows {
        insert(&db, row);
    }

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 6),
        |_row| RowCost::Priced(0.5),
        no_deadline(),
    )
    .expect("query");

    // Assert: the hand-computed totals first, so the reconciliation below is
    // anchored on known non-zero figures rather than on whatever both folds
    // happened to agree about.
    let expected = [
        6,    // requests
        3,    // ok
        2,    // errors, client_disconnect excluded
        1305, // input_tokens
        541,  // output_tokens
        411,  // reasoning_tokens
        775,  // cache_read_billed
        495,  // cache_write_5m
        1123, // cache_write_1h
        39,   // server_tool_calls, summed over the server_tool_use values
        5,    // stream_count
        1,    // client_disconnect_total
        4,    // fallback_served
    ];
    assert_eq!(additive(&result.totals), expected);

    // Both folds saw the same rows, so every additive field reconciles
    // bucket-wise with the totals it was folded beside.
    let s = series(&result);
    let summed = s.buckets.iter().fold([0_i64; 13], |mut acc, b| {
        for (slot, value) in acc.iter_mut().zip(additive(&b.metrics)) {
            *slot += value;
        }
        acc
    });
    assert_eq!(summed, expected);
}

#[test]
fn a_bucket_with_no_rows_reports_zero_requests_and_no_derived_metrics() {
    // Arrange: one row in the first bucket only.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "a",
            ts_start: 100,
            stream: 1,
            latency_ms: 1000,
            ttfb_ms: Some(200),
            input_tokens: Some(500),
            output_tokens: Some(100),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 3),
        |_row| RowCost::Priced(1.0),
        no_deadline(),
    )
    .expect("query");

    // Assert: the empty buckets are honest zeros, never a fabricated
    // measurement -- and never a cost of 0 for a window that priced nothing.
    let s = series(&result);
    assert_eq!(s.buckets.len(), 3);
    for empty in &s.buckets[1..] {
        let m = &empty.metrics;
        assert_eq!(m.requests, 0);
        assert_eq!(m.ttft_p50_ms, None);
        assert_eq!(m.ttft_p95_ms, None);
        assert_eq!(m.latency_p50_ms, None);
        assert_eq!(m.latency_p95_ms, None);
        assert_eq!(m.throughput_tok_s, None);
        assert_eq!(m.ctx_avg, None);
        assert_eq!(m.ctx_peak, None);
        assert_eq!(m.cache_hit_pct, None);
        assert_eq!(m.cost_usd, None);
        assert_eq!(m.cost_status, CostStatus::Unpriced);
    }
}

#[test]
fn an_empty_window_yields_an_empty_series_rather_than_synthetic_zero_buckets() {
    // Arrange: a ledger with nothing in the window at all.
    let (_dir, db) = open_db();

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 10),
        unpriced,
        no_deadline(),
    )
    .expect("query");

    // Assert: no rows means nothing to plot -- ten zero buckets would present an
    // empty ledger as a measured flat line.
    assert!(result.groups.is_empty());
    assert!(series(&result).buckets.is_empty());
    assert_eq!(series(&result).bucket_ms, 1000);
}

#[test]
fn per_bucket_maxima_and_cost_status_describe_that_bucket_only() {
    // Arrange: a slow priced row in the first bucket and a fast unpriced one in
    // the second, so neither bucket's figures can be the window's.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "slow",
            ts_start: 100,
            upstream: Some("u-priced"),
            stream: 1,
            latency_ms: 4000,
            ttfb_ms: Some(900),
            input_tokens: Some(8000),
            output_tokens: Some(400),
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "fast",
            ts_start: 1200,
            upstream: Some("u-unpriced"),
            stream: 1,
            latency_ms: 300,
            ttfb_ms: Some(50),
            input_tokens: Some(100),
            output_tokens: Some(20),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 2),
        |row| {
            if row.key.upstream.as_deref() == Some("u-priced") {
                RowCost::Priced(3.0)
            } else {
                RowCost::Unpriced
            }
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: each bucket carries its OWN maxima and its own cost tri-state,
    // while the window totals mix both.
    let s = series(&result);
    let first = &s.buckets[0].metrics;
    let second = &s.buckets[1].metrics;
    assert_eq!(first.latency_p95_ms, Some(4000));
    assert_eq!(first.ttft_p95_ms, Some(900));
    assert_eq!(first.ctx_peak, Some(8000));
    assert_eq!(first.cost_status, CostStatus::Priced);
    assert_close(first.cost_usd, 3.0);
    assert_eq!(second.latency_p95_ms, Some(300));
    assert_eq!(second.ttft_p95_ms, Some(50));
    assert_eq!(second.ctx_peak, Some(100));
    assert_eq!(second.cost_status, CostStatus::Unpriced);
    assert_eq!(second.cost_usd, None);
    assert_eq!(result.totals.cost_status, CostStatus::Partial);
    assert_eq!(result.totals.latency_p95_ms, Some(4000));
}

/// Seed the hand-computed SERIES fixture: traffic in the FIRST bucket of the
/// grid and in the LAST one, with the middle of the window empty. The first
/// bucket holds two streaming successes over one upstream (so they aggregate
/// into a single fine row); the last holds a streaming success over a PRICED
/// upstream beside a failed, fallback-served row over an UNPRICED one, so that
/// bucket's cost tri-state and the window's differ.
fn seed_hand_computed_series(db: &UsageDb) {
    insert(
        db,
        &Fixture {
            request_id: "b1",
            ts_start: 100,
            upstream: Some("u-priced"),
            stream: 1,
            latency_ms: 1000,
            ttfb_ms: Some(200),
            input_tokens: Some(1000),
            output_tokens: Some(300),
            cache_read: Some(500),
            cache_write_5m: Some(100),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "b2",
            ts_start: 1900,
            upstream: Some("u-priced"),
            stream: 1,
            latency_ms: 600,
            ttfb_ms: Some(400),
            input_tokens: Some(3000),
            output_tokens: Some(100),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "b3",
            ts_start: 8000,
            upstream: Some("u-priced"),
            stream: 1,
            latency_ms: 500,
            ttfb_ms: Some(100),
            input_tokens: Some(2000),
            output_tokens: Some(200),
            cache_read: Some(300),
            cache_write_1h: Some(200),
            ..Fixture::default()
        },
    );
    insert(
        db,
        &Fixture {
            request_id: "b4",
            ts_start: 9500,
            upstream: Some("u-unpriced"),
            outcome: "upstream_error",
            stream: 1,
            latency_ms: 300,
            fallback_count: 2,
            ..Fixture::default()
        },
    );
}

/// `u-priced` costs $1 per 1,000 input tokens of the fine row; `u-unpriced` has
/// no price at all.
fn price_per_thousand_input(row: &AggRow) -> RowCost {
    if row.key.upstream.as_deref() == Some("u-priced") {
        RowCost::Priced(row.input_tokens as f64 / 1000.0)
    } else {
        RowCost::Unpriced
    }
}

#[test]
fn hand_computed_series_matches_the_edge_buckets_metrics_and_costs() {
    // Arrange: the window [0, 10_000) on a five-bucket 2000 ms grid, so the
    // fixture's traffic lands in bucket 0 ([0, 2000)) and bucket 4
    // ([8000, 10_000)) with three empty buckets between them.
    let (_dir, db) = open_db();
    seed_hand_computed_series(&db);

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 2000, 5),
        price_per_thousand_input,
        no_deadline(),
    )
    .expect("query");

    // Assert
    let s = series(&result);
    assert_eq!(s.bucket_ms, 2000);
    assert_eq!(s.buckets.len(), 5);
    let starts: Vec<i64> = s.buckets.iter().map(|b| b.start_ms).collect();
    assert_eq!(starts, vec![0, 2000, 4000, 6000, 8000]);

    // FIRST bucket -- b1 (ttfb 200) and b2 (ttfb 400), both streaming
    // successes over `u-priced`.
    let first = &s.buckets[0].metrics;
    assert_eq!(first.requests, 2);
    // Request-weighted ttft: (200 + 400) / 2 eligible rows = 300; the p95 is
    // the MAX of the same population.
    assert_eq!(first.ttft_p50_ms, Some(300));
    assert_eq!(first.ttft_p95_ms, Some(400));
    // Only b1 reports a cache read. Its cache-inclusive prompt total is
    // 1000 input + 500 read + 100 write_5m = 1600, so the single eligible
    // row's share is 500 / 1600 = 0.3125 -> 31.25 %.
    assert_close(first.cache_hit_pct, 31.25);
    // Cost: b1 and b2 share the (model, provider, upstream, alias, bucket)
    // grain, so they price as ONE fine row of 1000 + 3000 = 4000 input tokens
    // -> $4.00, and every row in the bucket is priced.
    assert_eq!(first.cost_status, CostStatus::Priced);
    assert_close(first.cost_usd, 4.0);
    // Latency p50 is request-weighted over the bucket's 2 requests:
    // (1000 + 600) / 2 = 800.
    assert_eq!(first.latency_p50_ms, Some(800));

    // LAST bucket -- b3 (priced, streaming ok) beside b4 (unpriced, failed,
    // fallback-served), two DISTINCT fine rows in one bucket.
    let last = &s.buckets[4].metrics;
    assert_eq!(last.requests, 2);
    assert_eq!(last.ok, 1);
    assert_eq!(last.errors, 1);
    assert_eq!(last.fallback_served, 1);
    // b4 stamped no ttfb, so only b3 is ttft-eligible: 100 / 1 = 100.
    assert_eq!(last.ttft_p50_ms, Some(100));
    assert_eq!(last.ttft_p95_ms, Some(100));
    // b3's prompt total is 2000 input + 300 read + 200 write_1h = 2500, so its
    // share is 300 / 2500 = 0.12 -> 12 %. b4 reports no cache read at all.
    assert_close(last.cache_hit_pct, 12.0);
    // Cost: b3's fine row is 2000 input tokens -> $2.00, and b4 is unpriced, so
    // the bucket is partial and reports the priced subtotal only.
    assert_eq!(last.cost_status, CostStatus::Partial);
    assert_close(last.cost_usd, 2.0);
    // Latency p50 over the bucket's 2 requests: (500 + 300) / 2 = 400.
    assert_eq!(last.latency_p50_ms, Some(400));

    // The three interior buckets saw nothing, and say so honestly.
    for empty in &s.buckets[1..4] {
        assert_eq!(empty.metrics.requests, 0);
        assert_eq!(empty.metrics.ttft_p50_ms, None);
        assert_eq!(empty.metrics.cache_hit_pct, None);
        assert_eq!(empty.metrics.cost_usd, None);
        assert_eq!(empty.metrics.cost_status, CostStatus::Unpriced);
    }

    // The window totals fold the same three fine rows: 4 requests, ttft
    // (200 + 400 + 100) / 3 = 233 (integer), cache-hit (0.3125 + 0.12) / 2
    // = 0.21625 -> 21.625 %, and $4.00 + $2.00 = $6.00 priced beside one
    // unpriced row.
    let t = &result.totals;
    assert_eq!(t.requests, 4);
    assert_eq!(t.ttft_p50_ms, Some(233));
    assert_eq!(t.ttft_p95_ms, Some(400));
    assert_close(t.cache_hit_pct, 21.625);
    assert_eq!(t.cost_status, CostStatus::Partial);
    assert_close(t.cost_usd, 6.0);
}

#[test]
fn a_widened_grid_still_covers_the_window_and_reprices_it_whole() {
    // Arrange: the SAME fixture the five-bucket grid measured, read back on a
    // WIDE grid -- the shape the cap's widening produces, built directly rather
    // than through the caller's resolution. Widening changes the GRAIN, never
    // the coverage.
    let (_dir, db) = open_db();
    seed_hand_computed_series(&db);

    // Act
    let widened = query(
        &db,
        &bucketed(GroupDim::Model, 10_000, 1),
        price_per_thousand_input,
        no_deadline(),
    )
    .expect("query");
    let narrow = query(
        &db,
        &bucketed(GroupDim::Model, 2000, 5),
        price_per_thousand_input,
        no_deadline(),
    )
    .expect("query");

    // Assert: the resolved width is reported as the WIDE one, and its single
    // bucket carries exactly the window totals -- no row was dropped on the way.
    let s = series(&widened);
    assert_eq!(s.bucket_ms, 10_000);
    assert_eq!(s.buckets.len(), 1);
    assert_eq!(s.buckets[0].start_ms, 0);
    assert_eq!(&s.buckets[0].metrics, &widened.totals);
    assert_eq!(s.buckets[0].metrics.requests, 4);
    // Coarsening merges the fine rows the narrow grid split by bucket, so the
    // per-bucket cost sums are unchanged: $4.00 + $2.00 = $6.00.
    assert_close(s.buckets[0].metrics.cost_usd, 6.0);
    assert_eq!(s.buckets[0].metrics.cost_status, CostStatus::Partial);
    // The groups and totals are a property of the row SET, not of the grain, so
    // both grids must agree on them exactly.
    assert_eq!(widened.totals, narrow.totals);
    assert_eq!(widened.groups, narrow.groups);
}

/// The bucketed statement must ride `idx_requests_ts_start` for its window
/// range rather than degrading to a full table scan -- the series grain widens
/// the GROUP BY, and a scan under that would put the 100k-row cost check
/// (`a_day_series_over_a_hundred_thousand_rows_stays_inside_the_query_budget`,
/// in the shell's query tests) past its budget. If this ever fails, add a
/// covering index rather than accepting the scan.
#[test]
fn the_series_statement_uses_the_ts_start_index() {
    // Arrange
    let (_dir, db) = open_db();
    seed_bulk(&db, 64);

    // Act
    let plan: Vec<String> = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {SERIES_AGG_SQL}"))
        .expect("prepare explain")
        .query_map(
            rusqlite::params![0_i64, 10_000_i64, None::<&str>, None::<&str>, 1000_i64],
            |row| row.get::<_, String>(3),
        )
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    // Assert
    assert!(
        plan.iter().any(|d| d.contains("idx_requests_ts_start")),
        "the series query must use idx_requests_ts_start; plan was {plan:?}"
    );
}

#[test]
fn an_unusable_bucket_grid_is_refused_before_any_statement_runs() {
    // Arrange: the three grids the SQL and the densification cannot answer -- a
    // zero width would divide by zero in SQL (a silent NULL, not an error), and
    // a count outside the cap would densify an unbounded vector.
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    for (width_ms, count) in [(0, 10), (-1000, 10), (1000, 0), (1000, 1001)] {
        // Act
        let refused = query(
            &db,
            &bucketed(GroupDim::Model, width_ms, count),
            unpriced,
            no_deadline(),
        );

        // Assert: an error, never a panic -- this path is network-reachable and
        // the release profile aborts on panic.
        assert!(
            matches!(refused, Err(QueryError::InvalidBucket)),
            "expected width {width_ms} count {count} to be refused, got {refused:?}"
        );
    }

    // The cap itself is accepted, so the refusal is a bound and not an off-by-one.
    assert!(
        query(
            &db,
            &bucketed(GroupDim::Model, 1000, 1000),
            unpriced,
            no_deadline()
        )
        .is_ok()
    );
}

#[test]
fn a_grid_whose_last_bucket_start_overflows_is_refused() {
    // Arrange: a width and count that pass the width/cap checks but whose last
    // bucket start does not fit an i64.
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());
    let overflowing = bucketed(GroupDim::Model, i64::MAX / 2 + 10, 3);

    // Act
    let refused = query(&db, &overflowing, unpriced, no_deadline());

    // Assert: an error rather than a wrapped `start_ms` in release or a panic
    // under overflow checks.
    assert!(
        matches!(refused, Err(QueryError::InvalidBucket)),
        "expected the overflowing grid to be refused, got {refused:?}"
    );

    // The widest grid that still fits is accepted, so the refusal is a bound.
    assert!(
        query(
            &db,
            &bucketed(GroupDim::Model, i64::MAX, 2),
            unpriced,
            no_deadline()
        )
        .is_ok()
    );
}

#[test]
fn a_row_outside_the_bucket_grid_fails_rather_than_vanishing_from_the_series() {
    // Arrange: a grid that covers only the first 2000 ms of the 10_000 ms window,
    // and a row beyond it. Counting that row in the groups while densification
    // drops it would make the two folds disagree in silence.
    let (_dir, db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "beyond",
            ts_start: 9500,
            ..Fixture::default()
        },
    );

    // Act
    let refused = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 2),
        unpriced,
        no_deadline(),
    );

    // Assert
    assert!(
        matches!(refused, Err(QueryError::InvalidBucket)),
        "expected the out-of-grid row to be refused, got {refused:?}"
    );

    // A grid that does cover the window answers normally.
    let covered = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 10),
        unpriced,
        no_deadline(),
    )
    .expect("query");
    assert_eq!(covered.totals.requests, 1);
    assert_eq!(series(&covered).buckets[9].metrics.requests, 1);
}

#[test]
fn an_expired_deadline_during_the_series_fold_yields_no_partial_series() {
    // Arrange: enough rows that the bucketed scan runs past one progress
    // callback interval, plus an already-expired deadline.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");

    // Act
    let interrupted = query(&db, &bucketed(GroupDim::Model, 1000, 10), unpriced, expired);
    let after = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 10),
        unpriced,
        no_deadline(),
    );

    // Assert: the interrupt sheds as its own variant rather than densifying a
    // half-folded series, and the connection is left usable.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    let recovered = after.expect("query");
    assert_eq!(recovered.totals.requests, 4000);
    assert_eq!(series(&recovered).buckets.len(), 10);
}

/// Seed `count` minimal rows on consecutive `ts_start` values -- enough of them
/// that a full aggregate scan runs past at least one progress-callback interval.
fn seed_bulk(db: &UsageDb, count: i64) {
    db.conn().execute_batch("BEGIN").expect("begin");
    for i in 0..count {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
                 VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm1', 'pa', 'u1', 0, \
                 'ok', 10, 0, 0, 1, 0)",
                rusqlite::params![100 + i, format!("bulk-{i}")],
            )
            .expect("insert bulk row");
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

#[test]
fn a_panicking_price_closure_still_detaches_the_progress_handler() {
    // Arrange: a ledger big enough that a full scan runs past a progress
    // callback interval, plus an ALREADY-EXPIRED deadline. The panicking query
    // is windowed down to a single row, so its own statement finishes inside
    // one interval and the handler never gets to fire on it.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");
    let one_row = QuerySpec {
        from_ms: 100,
        to_ms: 101,
        ..spec(GroupDim::Model)
    };

    // Act: the pricing panic unwinds out of the fold the way a caught panel
    // panic would.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query(&db, &one_row, |_row| panic!("pricing blew up"), expired)
    }));
    std::panic::set_hook(hook);

    // Assert: the connection is left clean, so a following full scan by a
    // caller that installs NO handler of its own still completes. A retained
    // expired handler would interrupt it -- `query` itself cannot show this,
    // since it overwrites the stale handler with its own on entry.
    assert!(panicked.is_err(), "expected the price panic to propagate");
    let scanned = crate::query::aggregate(&db, 0, 10_000).expect("aggregate after the panic");
    assert_eq!(scanned.iter().map(|r| r.requests).sum::<i64>(), 4000);
}

#[test]
fn a_cost_sum_that_overflows_to_infinity_reads_as_unpriced_rather_than_panicking() {
    // Arrange: one model over two upstreams, each priced at a magnitude an
    // operator could reach with an extreme `[registry.*.pricing]` rate, so the
    // group's SUM overflows f64 to infinity.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u1"), ("b", "u2")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act: the fold is network-reachable and the release profile aborts on
    // panic, so a non-finite total must be a VALUE, never an assertion.
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Priced(f64::MAX),
        no_deadline(),
    )
    .expect("query");

    // Assert: no cost claimed, and the status says so rather than reporting a
    // meaningless infinity.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_usd, None);
    assert_eq!(m.cost_status, CostStatus::Unpriced);
    assert_eq!(result.totals.cost_usd, None);
    assert_eq!(result.totals.cost_status, CostStatus::Unpriced);
    // The rest of the group is untouched: only the cost degraded.
    assert_eq!(m.requests, 2);
}

#[test]
fn a_finite_cost_still_prices_normally_beside_the_overflow_guard() {
    // Arrange: the guard must not swallow ordinary large-but-finite costs.
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Priced(1e300),
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Priced);
    assert_eq!(m.cost_usd, Some(1e300));
}

#[test]
fn interrupt_error_maps_to_its_own_variant() {
    // Arrange: the SQLite code a fired progress handler produces.
    let interrupted = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
        None,
    );
    let corrupt = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        None,
    );

    // Act + Assert: a deadline interrupt is distinguishable from a real fault.
    assert!(matches!(classify(interrupted), QueryError::Interrupted));
    assert!(matches!(classify(corrupt), QueryError::Sqlite(_)));
}

#[test]
fn expired_deadline_interrupts_the_query_and_leaves_the_connection_usable() {
    // Arrange: enough rows that the aggregate scan runs past one progress
    // callback interval.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");

    // Act
    let interrupted = query(&db, &spec(GroupDim::Model), unpriced, expired);
    let after = query(&db, &spec(GroupDim::Model), unpriced, no_deadline());

    // Assert: the expired deadline sheds as its own variant, and the handler is
    // uninstalled afterwards so the next query on the same connection runs.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    assert_eq!(after.expect("query").totals.requests, 4000);
}
