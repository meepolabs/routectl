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
    let (_dir, mut db) = open_db();
    seed_hand_computed(&db);

    // Act: u1 is priced at $1 per 1,000 input tokens; u2 has no price.
    let result = query(
        &mut db,
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
    // figures share that population -- avg = (100 + 300) / 2, max = MAX.
    assert_eq!(m.ttft_avg_ms, Some(200));
    assert_eq!(m.ttft_max_ms, Some(300));
    // latency avg = (1100 + 800 + 600 + 200) / 4 requests = 2700 / 4.
    assert_eq!(m.latency_avg_ms, Some(675));
    assert_eq!(m.latency_max_ms, Some(1100));
    // throughput: r1 generated 200 tokens over 1000 ms and r2 250 over 500 ms;
    // r3 is non-streaming and r4 has no TTFB. (200 + 250) * 1000 / 1500 = 300.
    assert_close(m.throughput_tok_s, 300.0);
    // ctx: r1 1000, r2 3000, r3 2000 + 500 read + 100 write = 2600; r4
    // reported nothing. (1000 + 3000 + 2600) / 3 = 2200; peak = 3000.
    assert_eq!(m.ctx_avg, Some(2200));
    assert_eq!(m.ctx_peak, Some(3000));
    // cache-hit: only u2's fine row (r3, r4) reports a cache read. Its prompt
    // total = 2000 input + 500 read + 100 write = 2600, so the share is
    // 500/2600; u1's fine row reports none and stays out of both sides.
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
    let (_dir, mut db) = open_db();
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
    let by_model = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");
    let by_alias = query(&mut db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");

    // Assert: the maxima are a MAX over the fine maxima, not a re-derivation.
    assert_eq!(by_model.groups.len(), 2);
    assert_eq!(by_alias.groups.len(), 1);
    let alias_metrics = &by_alias.groups[0].metrics;
    assert_eq!(alias_metrics.latency_max_ms, Some(5000));
    assert_eq!(alias_metrics.ttft_max_ms, Some(4000));
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
    let (_dir, mut db) = open_db();
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
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

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

/// Seed one model over two providers with every terminal outcome class the
/// readers distinguish: pa carries ok, ok, upstream_error, cancelled; pb carries
/// ok, client_disconnect, cancelled. Each provider is its own fine row, so the
/// coarse model group is folded from two of them.
fn seed_outcome_mix(db: &UsageDb) {
    let rows = [
        ("o1", "pa", "ok"),
        ("o2", "pa", "ok"),
        ("o3", "pa", "upstream_error"),
        ("o4", "pa", "cancelled"),
        ("o5", "pb", "ok"),
        ("o6", "pb", "client_disconnect"),
        ("o7", "pb", "cancelled"),
    ];
    for (request_id, provider, outcome) in rows {
        insert(
            db,
            &Fixture {
                request_id,
                provider: Some(provider),
                outcome,
                ..Fixture::default()
            },
        );
    }
}

/// `(requests, ok, errors, client_disconnect_total, cancelled_total)`.
fn outcome_counts(m: &QueryMetrics) -> (i64, i64, i64, i64, i64) {
    (
        m.requests,
        m.ok,
        m.errors,
        m.client_disconnect_total,
        m.cancelled_total,
    )
}

#[test]
fn cancelled_rows_are_excluded_from_errors_and_reconcile_per_group_and_totals() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_outcome_mix(&db);

    // Act
    let by_model = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");
    let by_provider =
        query(&mut db, &spec(GroupDim::Provider), unpriced, no_deadline()).expect("query");

    // Assert: 7 = 3 ok + 1 upstream_error + 1 client_disconnect + 2 cancelled,
    // and only the upstream_error counts as an error.
    let m = &group(&by_model, "m1").metrics;
    assert_eq!(outcome_counts(m), (7, 3, 1, 1, 2), "group m1");
    assert_eq!(outcome_counts(&by_model.totals), (7, 3, 1, 1, 2), "totals");
    // pa = 2 ok + 1 upstream_error + 1 cancelled; pb = 1 ok + 1
    // client_disconnect + 1 cancelled.
    let pa = &group(&by_provider, "pa").metrics;
    let pb = &group(&by_provider, "pb").metrics;
    assert_eq!(outcome_counts(pa), (4, 2, 1, 0, 1), "group pa");
    assert_eq!(outcome_counts(pb), (3, 1, 0, 1, 1), "group pb");
    for metrics in [m, &by_model.totals, pa, pb] {
        assert_eq!(
            metrics.requests,
            metrics.ok + metrics.errors + metrics.client_disconnect_total + metrics.cancelled_total
        );
    }
}

#[test]
fn throughput_is_summed_output_over_summed_generation_time() {
    // Arrange: a short fast stream (100 tokens over 100 ms = 1000 tok/s) and a
    // long slow one (1000 tokens over 10_000 ms = 100 tok/s), on two upstreams
    // so the fold adds across fine rows as well as inside one.
    let (_dir, mut db) = open_db();
    for (request_id, upstream, latency_ms, output) in
        [("fast", "u1", 200, 100), ("slow", "u2", 10_100, 1000)]
    {
        insert(
            &db,
            &Fixture {
                request_id,
                upstream: Some(upstream),
                stream: 1,
                latency_ms,
                ttfb_ms: Some(100),
                output_tokens: Some(output),
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: (100 + 1000) * 1000 / (100 + 10_000) ms, not (1000 + 100) / 2.
    let ratio_of_sums = 1100.0 * 1000.0 / 10_100.0;
    let mean_of_rates = f64::midpoint(1000.0, 100.0);
    let m = &group(&result, "m1").metrics;
    assert_close(m.throughput_tok_s, ratio_of_sums);
    assert!(
        (m.throughput_tok_s.expect("throughput present") - mean_of_rates).abs() > 1.0,
        "throughput must not be the mean of per-request rates"
    );
    assert_close(result.totals.throughput_tok_s, ratio_of_sums);
}

#[test]
fn cache_heavy_row_counts_its_cached_prompt_toward_context() {
    // Arrange: a fresh-input-only row of 1000, and a cache-heavy row with 100
    // fresh input, 5000 read from cache and 900 written: a 6000-token prompt.
    let (_dir, mut db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "fresh",
            upstream: Some("u1"),
            input_tokens: Some(1000),
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "cached",
            upstream: Some("u2"),
            input_tokens: Some(100),
            cache_read: Some(5000),
            cache_write_1h: Some(900),
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: (1000 + 6000) / 2 and the cache-heavy row's 6000 as the peak.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.ctx_avg, Some(3500));
    assert_eq!(m.ctx_peak, Some(6000));
}

#[test]
fn context_counts_a_row_that_reports_only_cache_reads() {
    // Arrange: a row with no input-token count but a cache read, beside a row
    // reporting no prompt dimension at all.
    let (_dir, mut db) = open_db();
    insert(
        &db,
        &Fixture {
            request_id: "cache-only",
            cache_read: Some(4000),
            ..Fixture::default()
        },
    );
    insert(
        &db,
        &Fixture {
            request_id: "silent",
            ..Fixture::default()
        },
    );

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert: the silent row is not a measured zero, so the mean is 4000.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.ctx_avg, Some(4000));
    assert_eq!(m.ctx_peak, Some(4000));
}

#[test]
fn presence_and_pre_dispatch_counts_match_hand_computed_values() {
    // Arrange: per row, (cache read, server-tool map, outcome, served model).
    //   p1: read 0,  tools {web:1}, ok,                served
    //   p2: read 40, -,             client_disconnect, served
    //   p3: -,       tools {},      client_disconnect, never dispatched
    //   p4: -,       -,             client_disconnect, never dispatched
    // cache_read_present = p1 + p2 = 2 (a reported zero counts);
    // server_tool_present = p1 + p3 = 2 (an empty map is still a report);
    // client_disconnect_total = 3, of which pre-dispatch (NULL model) = 2.
    let (_dir, mut db) = open_db();
    let rows = [
        ("p1", Some(0), Some(r#"{"web":1}"#), "ok", Some("m1")),
        ("p2", Some(40), None, "client_disconnect", Some("m1")),
        ("p3", None, Some("{}"), "client_disconnect", None),
        ("p4", None, None, "client_disconnect", None),
    ];
    for (request_id, cache_read, server_tool_use, outcome, model) in rows {
        insert(
            &db,
            &Fixture {
                request_id,
                model,
                outcome,
                cache_read,
                server_tool_use,
                ..Fixture::default()
            },
        );
    }

    // Act: by provider, so the served and never-dispatched rows share a group.
    let result = query(&mut db, &spec(GroupDim::Provider), unpriced, no_deadline()).expect("query");

    // Assert
    let m = &group(&result, "pa").metrics;
    assert_eq!(m.cache_read_present, 2);
    assert_eq!(m.server_tool_present, 2);
    assert_eq!(m.client_disconnect_total, 3);
    assert_eq!(m.client_disconnect_pre_dispatch, 2);
    assert_eq!(
        (
            result.totals.cache_read_present,
            result.totals.server_tool_present,
            result.totals.client_disconnect_pre_dispatch
        ),
        (2, 2, 2)
    );
}
