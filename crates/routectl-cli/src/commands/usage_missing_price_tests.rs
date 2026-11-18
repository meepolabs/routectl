// Which used dimensions kept a row from a complete price, as the pricer names
// them and as the grouped query carries them in `missing_price_dimensions`.
// `include!`d into `usage_tests.rs`, so these compile into THAT module and
// its helpers stay in scope; all imports live in the host file -- do not add
// `use` lines here.

/// Owned labels, in the order a test expects them.
fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// A query over the whole ledger grouped by `dim`, priced by the CLI pricer.
fn priced_query(
    db: &mut UsageDb,
    config: &Config,
    dim: GroupDim,
    bucket: Option<BucketSpec>,
) -> routectl_usage::QueryResult {
    let spec = QuerySpec {
        from_ms: 0,
        to_ms: 10_000,
        group_by: dim,
        alias_filter: None,
        provider_filter: None,
        bucket,
    };
    let deadline = Instant::now() + Duration::from_mins(10);
    query(
        db,
        &spec,
        |row| cost_for_row(config, &no_prices(), row),
        deadline,
    )
    .expect("query")
}

#[test]
fn a_subscription_row_with_an_unpriced_30m_write_names_it_and_has_no_equivalent() {
    // Arrange: the 30m write is used and has no rate; the 5m rate exists.
    let (_dir, _path, mut db) = temp_db();
    insert_lifetime_row(&db, &SPLIT_30M);
    let config = lifetime_config("oauth://anthropic", lifetime_rates(false));

    // Act
    let verdict = cost_for_row(
        &config,
        &no_prices(),
        &routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate")[0],
    );
    let totals = priced_query(&mut db, &config, GroupDim::Model, None).totals;

    // Assert: the verdict, then the metric set it folds into.
    assert_eq!(
        verdict,
        RowCost::Subscription(None, labels(&["cache write 30m"]))
    );
    assert_eq!(
        totals.missing_price_dimensions,
        labels(&["cache write 30m"])
    );
    assert_eq!(totals.equivalent_cost_usd, None);
    assert_eq!(totals.cost_usd, None);
    assert_eq!(totals.cost_status, CostStatus::Subscription);
}

#[test]
fn a_complete_subscription_equivalent_never_reaches_cost_usd() {
    // Arrange: the same subscription row with the 30m write priced.
    let (_dir, _path, mut db) = temp_db();
    insert_lifetime_row(&db, &SPLIT_30M);
    let config = lifetime_config("oauth://anthropic", lifetime_rates(true));

    // Act
    let totals = priced_query(&mut db, &config, GroupDim::Model, None).totals;

    // Assert
    assert_eq!(totals.cost_usd, None);
    assert_eq!(totals.equivalent_cost_usd, Some(LIFETIME_WITH_30M_USD));
    assert!(totals.missing_price_dimensions.is_empty());
}

#[test]
fn the_same_row_on_an_api_key_is_unpriced_naming_the_30m_write() {
    // Arrange
    let (_dir, _path, mut db) = temp_db();
    insert_lifetime_row(&db, &SPLIT_30M);
    let config = lifetime_config("env://LT_KEY", lifetime_rates(false));

    // Act
    let totals = priced_query(&mut db, &config, GroupDim::Model, None).totals;

    // Assert
    assert_eq!(totals.cost_status, CostStatus::Unpriced);
    assert_eq!(totals.cost_usd, None);
    assert_eq!(
        totals.missing_price_dimensions,
        labels(&["cache write 30m"])
    );
}

/// Two models on one API-key provider: `lifetime-model` writes at 30m (no
/// rate) in the first bucket, `other-model` is fully priced in the second.
fn seed_priced_and_unpriced_models(db: &UsageDb) {
    insert_lifetime_row(db, &SPLIT_30M);
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens) \
             VALUES (6000, 6000, 'priced', 'anthropic', 'req-model', 'al', 'other-model', \
             ?1, ?2, 'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000)",
            rusqlite::params![LIFETIME_PROVIDER, LIFETIME_UPSTREAM],
        )
        .expect("insert priced row");
}

#[test]
fn a_fully_priced_group_is_empty_while_totals_and_buckets_carry_the_union() {
    // Arrange
    let (_dir, _path, mut db) = temp_db();
    seed_priced_and_unpriced_models(&db);
    let mut config = lifetime_config("env://LT_KEY", lifetime_rates(false));
    price_models(
        &mut config,
        &["other-model"],
        LIFETIME_PROVIDER,
        LIFETIME_UPSTREAM,
        &lifetime_rates(false),
    );
    let bucket = BucketSpec {
        width_ms: 5_000,
        count: 2,
    };

    // Act
    let result = priced_query(&mut db, &config, GroupDim::Model, Some(bucket));

    // Assert: the groups, then the totals, then each bucket.
    let by_label = |label: &str| {
        &result
            .groups
            .iter()
            .find(|g| g.label == label)
            .expect("group present")
            .metrics
    };
    let priced = by_label("other-model");
    assert_eq!(priced.cost_status, CostStatus::Priced);
    assert!(priced.missing_price_dimensions.is_empty());
    assert_eq!(
        by_label(LIFETIME_MODEL).missing_price_dimensions,
        labels(&["cache write 30m"])
    );
    assert_eq!(result.totals.cost_status, CostStatus::Partial);
    assert_eq!(
        result.totals.missing_price_dimensions,
        labels(&["cache write 30m"])
    );
    let buckets = &result.series.as_ref().expect("series").buckets;
    assert_eq!(
        buckets[0].metrics.missing_price_dimensions,
        labels(&["cache write 30m"])
    );
    assert!(buckets[1].metrics.missing_price_dimensions.is_empty());
}

/// One gemini row (disjoint reasoning) on `shift`, with output and reasoning
/// tokens, priced by `pricing`.
fn disjoint_reasoning_verdict(pricing: &PricingConfig) -> RowCost {
    let (_dir, _path, db) = temp_db();
    insert_era_row(
        &db,
        "g",
        1_000,
        Some("gemini"),
        Some(1_000_000),
        Some(1_000_000),
        Some(2_000_000),
    );
    let mut config = era_config(ProviderEntry::gemini("env://SHIFT_KEY"));
    config.models.get_mut("m").expect("model entry").pricing = Some(pricing.clone());
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");
    cost_for_row(&config, &no_prices(), &rows[0])
}

#[test]
fn disjoint_reasoning_without_an_output_rate_is_named_output() {
    // Arrange
    let no_output = PricingConfig {
        input_per_mtok: Some(3.0),
        ..Default::default()
    };

    // Act
    let verdict = disjoint_reasoning_verdict(&no_output);

    // Assert: reasoning bills at the output rate, so the missing rate is
    // output, named once.
    assert_eq!(verdict, RowCost::Unpriced(labels(&["output"])));
}

/// One row on `shift` with a NULL persisted kind, input and reasoning tokens
/// only, priced at input and output.
fn kindless_reasoning_verdict(config: &Config) -> RowCost {
    let (_dir, _path, db) = temp_db();
    insert_era_row(&db, "k", 1_000, None, Some(1_000_000), None, Some(500));
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");
    cost_for_row(config, &no_prices(), &rows[0])
}

#[test]
fn reasons_other_than_a_missing_rate_name_their_own_labels() {
    // (name, config, expected verdict)
    let unpriced_config = {
        let mut config = era_config(ProviderEntry::openai_compat(
            "https://shift.example.test/v1",
            "env://SHIFT_KEY",
        ));
        config.models.clear();
        config
    };
    let cases = [
        (
            "unknown kind with reasoning",
            config_shift_now_subsumed(),
            RowCost::Unpriced(labels(&[UNKNOWN_REASONING_LABEL])),
        ),
        (
            "no resolvable pricing names every used dimension",
            unpriced_config,
            RowCost::Unpriced(labels(&["input", UNKNOWN_REASONING_LABEL])),
        ),
    ];
    for (name, config, expected) in cases {
        // Act
        let verdict = kindless_reasoning_verdict(&config);

        // Assert
        assert_eq!(verdict, expected, "{name}");
    }
}

#[test]
fn a_row_with_no_provider_names_nothing() {
    // Arrange: a pre-dispatch row records no provider.
    let (_dir, _path, db) = temp_db();
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, input_tokens) \
             VALUES (1000, 1000, 'p', 'anthropic', 'req-model', 'al', 0, \
             'upstream_error', 10, 0, 0, 1, 0, 500)",
            [],
        )
        .expect("insert pre-dispatch row");
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");

    // Act
    let verdict = cost_for_row(&Config::default(), &no_prices(), &rows[0]);

    // Assert
    assert_eq!(verdict, RowCost::Unpriced(Vec::new()));
}
