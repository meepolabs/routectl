// The hand-computed SERIES fixture and the bucket-grid contract: widening,
// the index pin, refused grids, overflowing grids, and rows outside the grid.
// Split from `grouped_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so the fixtures there stay in scope.
// All imports come from the host -- do not add `use` lines here.

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
/// the GROUP BY, and a scan under that would put the shell's large-series
/// day-over-100k-row cost check past its query budget. If this ever fails, add
/// a covering index rather than accepting the scan.
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
