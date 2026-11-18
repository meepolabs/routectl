// The report and the shared query agree on the cache-hit share: both weigh
// summed cache reads against the summed prompt of the fine rows that report
// cache reads. `include!`d into `usage_tests.rs`, so these compile into THAT
// module and its helpers stay in scope; all imports live in the host file --
// do not add `use` lines here.

/// One row with explicit prompt columns, each NULL when `None`.
fn insert_prompt_row(
    db: &UsageDb,
    request_id: &str,
    upstream: &str,
    input: i64,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, cache_read, cache_write_tokens) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', ?2, 0, \
             'ok', 5, 0, 0, 1, 0, ?3, ?4, ?5)",
            rusqlite::params![request_id, upstream, input, cache_read, cache_write],
        )
        .expect("insert prompt row");
}

#[test]
fn query_cache_hit_share_equals_the_report_footer_rule() {
    // Arrange: per fine row (upstream),
    //   u1: r1 input 1000 + read 3000, r2 input 500 with no cache report --
    //       one fine row that reports cache reads, so r2's input counts too;
    //   u2: r3 input 2000, no cache report -- out of both sides;
    //   u3: r4 input 100 + read 0 + write 400 -- a reported zero read.
    // Share = (3000 + 0) / ((1000 + 500 + 3000) + (100 + 0 + 400)) = 60 %.
    let (_dir, _path, mut db) = temp_db();
    insert_prompt_row(&db, "r1", "u1", 1000, Some(3000), None);
    insert_prompt_row(&db, "r2", "u1", 500, None, None);
    insert_prompt_row(&db, "r3", "u2", 2000, None, None);
    insert_prompt_row(&db, "r4", "u3", 100, Some(0), Some(400));
    let bounds = window_bounds(&zone(), WindowFlag::All, fixed_now());

    // Act
    let report = build_window_report(
        &mut db,
        &cost_config(),
        &no_prices(),
        "t".into(),
        bounds,
        None,
        false,
    )
    .unwrap();
    let spec = routectl_usage::QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by: routectl_usage::GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let deadline = Instant::now() + std::time::Duration::from_mins(10);
    let result = routectl_usage::query(&mut db, &spec, |_| RowCost::Unpriced, deadline).unwrap();

    // Assert
    let footer_pct = report
        .total
        .metrics
        .cache_hit_pct
        .expect("footer share present");
    let query_pct = result.totals.cache_hit_pct.expect("query share present");
    assert!((footer_pct - 60.0).abs() < 1e-9, "footer {footer_pct}");
    assert!((query_pct - footer_pct).abs() < 1e-9, "query {query_pct}");
    let row_pct = find(&report, "m").metrics.cache_hit_pct.expect("row share");
    let group_pct = result.groups[0].metrics.cache_hit_pct.expect("group share");
    assert!((group_pct - row_pct).abs() < 1e-9, "group {group_pct}");
}
