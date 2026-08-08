// The hand-computed unbucketed fixture: every derived metric checked against
// figures computed by hand, the coarse-grouping fold, and the fallback-served
// count. Split from `grouped_tests.rs` to keep each file under the size
// ceiling; `include!`d into the same `tests` module so the fixtures there stay
// in scope. All imports come from the host -- do not add `use` lines here.

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
