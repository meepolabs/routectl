// The bucketed series fold: densification, per-bucket additive
// reconciliation, empty buckets, and per-bucket maxima. Split from
// `grouped_tests.rs` to keep each file under the size ceiling; `include!`d into
// the same `tests` module so the fixtures there stay in scope. All imports come
// from the host -- do not add `use` lines here.

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
fn both_cost_channels_reconcile_bucket_wise_with_the_window_totals() {
    // Arrange: four rows on a four-bucket grid, alternating a priced upstream
    // with a subscription one, so each channel has contributions in two
    // separate buckets and neither can be reconstructed from the other.
    let (_dir, db) = open_db();
    for (id, ts, upstream) in [
        ("r1", 100, "u-priced"),
        ("r2", 1100, "u-sub"),
        ("r3", 2100, "u-priced"),
        ("r4", 3100, "u-sub"),
    ] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                ts_start: ts,
                upstream: Some(upstream),
                input_tokens: Some(1000),
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 4),
        |row| match row.key.upstream.as_deref() {
            Some("u-priced") => RowCost::Priced(1.25),
            _ => RowCost::Subscription(Some(2.5)),
        },
        no_deadline(),
    )
    .expect("query");

    // Assert: the hand-computed totals first -- real spend counts only the two
    // priced rows, notional value only the two subscription ones.
    assert_close(result.totals.cost_usd, 2.5);
    assert_close(result.totals.equivalent_cost_usd, 5.0);

    // Both cost channels are strictly additive, so the buckets sum to the
    // totals they were folded beside -- separately, and never into each other.
    let s = series(&result);
    let summed_cost: f64 = s.buckets.iter().filter_map(|b| b.metrics.cost_usd).sum();
    let summed_equivalent: f64 = s
        .buckets
        .iter()
        .filter_map(|b| b.metrics.equivalent_cost_usd)
        .sum();
    assert_close(Some(summed_cost), result.totals.cost_usd.unwrap());
    assert_close(
        Some(summed_equivalent),
        result.totals.equivalent_cost_usd.unwrap(),
    );
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
