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
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
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
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
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
             cache_read, cache_write_5m) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, ?7, ?8, \
             ?9, ?10, 0, 0, 1, 0, ?11, ?12, ?13, ?14, ?15)",
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
                f.input_tokens,
                f.output_tokens,
                f.reasoning_tokens,
                f.cache_read,
                f.cache_write_5m,
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
