// What a metric set says about the prices it is missing: the (model,
// dimension) pairs behind `missing_price_dimensions`, and the cache-write
// tokens no lifetime was reported for. Split from `grouped_tests.rs` to keep
// each file under the size ceiling; `include!`d into the same `tests` module so
// the fixtures there and in its sibling files stay in scope. All imports come
// from the host -- do not add `use` lines here.

fn missing(model: &str, dimension: &str) -> MissingPrice {
    MissingPrice {
        model: model.to_string(),
        dimension: dimension.to_string(),
        config_key: None,
    }
}

/// Two fine models under one alias: `m1` on two upstreams naming overlapping
/// dimensions (one unpriced, one a subscription), `m2` naming one, plus a
/// priced `m3` row that names none.
fn seed_missing_pairs(db: &UsageDb) {
    for (id, model, upstream) in [
        ("a", "m1", "u-unpriced"),
        ("b", "m1", "u-sub"),
        ("c", "m2", "u-unpriced"),
        ("d", "m3", "u-priced"),
    ] {
        insert(
            db,
            &Fixture {
                request_id: id,
                model: Some(model),
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }
}

fn pairs_pricer(row: &AggRow) -> RowCost {
    let owned = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect();
    match (row.key.model.as_deref(), row.key.upstream.as_deref()) {
        (Some("m1"), Some("u-unpriced")) => {
            RowCost::Unpriced(owned(&["output", "cache write 30m"]))
        }
        (Some("m1"), Some("u-sub")) => RowCost::Subscription(None, owned(&["output", "input"])),
        (Some("m2"), _) => RowCost::Unpriced(owned(&["cache write 30m"])),
        _ => RowCost::Priced(1.0),
    }
}

#[test]
fn missing_prices_pair_each_dimension_with_the_fine_model_that_lacked_it() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_missing_pairs(&db);

    // Act: grouped by alias, so the group label is not the model.
    let result =
        query(&mut db, &spec(GroupDim::Alias), pairs_pricer, no_deadline()).expect("query");

    // Assert: deduplicated across m1's two rows, sorted by model then
    // dimension, the priced m3 absent, and no config key from this crate.
    let expected = vec![
        missing("m1", "cache write 30m"),
        missing("m1", "input"),
        missing("m1", "output"),
        missing("m2", "cache write 30m"),
    ];
    assert_eq!(group(&result, "al").metrics.missing_prices, expected);
    assert_eq!(result.totals.missing_prices, expected);
    assert_eq!(
        result.totals.missing_price_dimensions,
        vec!["cache write 30m", "input", "output"]
    );
}

#[test]
fn a_fully_priced_set_names_no_missing_price() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_missing_pairs(&db);

    // Act
    let result =
        query(&mut db, &spec(GroupDim::Model), pairs_pricer, no_deadline()).expect("query");

    // Assert: m3 is the priced one; m2 is the control that a group does name.
    assert!(group(&result, "m3").metrics.missing_prices.is_empty());
    assert_eq!(
        group(&result, "m2").metrics.missing_prices,
        vec![missing("m2", "cache write 30m")]
    );
}

/// The dimension list a metric set carries is exactly the distinct dimensions
/// of its pairs.
fn assert_dimensions_are_the_pairs_dimensions(at: &str, m: &QueryMetrics) {
    let from_pairs: Vec<String> = m
        .missing_prices
        .iter()
        .map(|p| p.dimension.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(m.missing_price_dimensions, from_pairs, "{at}");
}

/// Every metric set of `result`: each group, the totals, and each bucket.
fn every_metric_set(result: &QueryResult) -> Vec<(String, &QueryMetrics)> {
    let mut sets: Vec<(String, &QueryMetrics)> = result
        .groups
        .iter()
        .map(|g| (format!("group {}", g.label), &g.metrics))
        .collect();
    sets.push(("totals".to_string(), &result.totals));
    if let Some(series) = &result.series {
        for (ix, bucket) in series.buckets.iter().enumerate() {
            sets.push((format!("bucket {ix}"), &bucket.metrics));
        }
    }
    sets
}

#[test]
fn missing_price_dimensions_are_the_distinct_pair_dimensions_on_every_metric_set() {
    // Arrange: the pricing-grain fixture (an unknown-lifetime row unpriced
    // over the window, priced and subscription rows beside it), the dense
    // sliced fixture (unpriced and subscription lanes naming dimensions), and
    // the pairs fixture, where two models name the same dimension in one set.
    let (_grain_dir, mut grain_db) = seeded_pricing_grain_db();
    let (_dense_dir, mut dense_db) = open_db();
    seed_dense(&dense_db, "pairs");
    let (_pairs_dir, mut pairs_db) = open_db();
    seed_missing_pairs(&pairs_db);
    let mut shared = 0;
    let mut named = 0;
    let mut checked = 0;

    for dim in GroupDim::ALL {
        for (grain, bucket) in GRAINS {
            // Act
            let grain_result = query(
                &mut grain_db,
                &three_day_spec(dim, Some(bucket)),
                lifetime_pricer,
                no_deadline(),
            )
            .expect("pricing-grain query");
            let dense_spec = QuerySpec {
                bucket: Some(BucketSpec {
                    width_ms: SLICE_WIDTH_MS,
                    count: 4,
                }),
                ..sliced_spec(dim)
            };
            let dense_result = read(&mut dense_db, &dense_spec, &forced(1));
            let pairs_result = query(
                &mut pairs_db,
                &bucketed(dim, 1_000, 2),
                pairs_pricer,
                no_deadline(),
            )
            .expect("pairs query");

            // Assert
            for (fixture, result) in [
                ("grain", &grain_result),
                ("dense", &dense_result),
                ("pairs", &pairs_result),
            ] {
                for (set, metrics) in every_metric_set(result) {
                    let at = format!("{fixture} {} {grain} {set}", dim.as_str());
                    assert_dimensions_are_the_pairs_dimensions(&at, metrics);
                    checked += 1;
                    named += usize::from(!metrics.missing_prices.is_empty());
                    shared += usize::from(
                        metrics.missing_prices.len() > metrics.missing_price_dimensions.len(),
                    );
                }
            }
        }
    }

    // Positive control: the property is checked on sets that name pairs, on
    // sets that name none, and on sets where two models share a dimension.
    assert!(named > 0, "no metric set named a missing price");
    assert!(checked > named, "every metric set named a missing price");
    assert!(
        shared > 0,
        "no metric set had two models naming one dimension"
    );
}

/// One new-era ledger row with a cache-write total and an optional split.
fn insert_split_write(
    db: &UsageDb,
    id: &'static str,
    ts: i64,
    model: &'static str,
    total: i64,
    split: Option<&'static str>,
) {
    insert_priced(
        db,
        &PricedRow {
            request_id: id,
            ts_start: ts,
            model,
            cache_write_tokens: Some(total),
            cache_write_lifetimes: split,
            ..OK_ROW
        },
    );
}

/// `m-known` holds a partly split write (40 unknown) and an unsplit one (50
/// unknown), in two buckets; `m-legacy` holds only a 5m/1h-column write;
/// `m-over` holds only a write whose split sums above its total.
fn seed_unknown_writes(db: &UsageDb) {
    insert_split_write(db, "part", 100, "m-known", 100, Some(r#"{"300":60}"#));
    insert_split_write(db, "none", 1_500, "m-known", 50, None);
    insert_split_write(db, "over", 200, "m-over", 10, Some(r#"{"300":99}"#));
    insert(
        db,
        &Fixture {
            request_id: "legacy",
            ts_start: 300,
            model: Some("m-legacy"),
            cache_write_5m: Some(30),
            cache_write_1h: Some(20),
            ..Fixture::default()
        },
    );
}

#[test]
fn cache_write_unknown_tokens_sum_each_rows_unreported_share() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_unknown_writes(&db);

    // Act
    let result = query(
        &mut db,
        &bucketed(GroupDim::Model, 1_000, 2),
        lifetime_pricer,
        no_deadline(),
    )
    .expect("query");

    // Assert: the groups, the totals, then each bucket over its own rows.
    assert_eq!(
        group(&result, "m-known").metrics.cache_write_unknown_tokens,
        90
    );
    assert_eq!(result.totals.cache_write_unknown_tokens, 90);
    let buckets = &series(&result).buckets;
    assert_eq!(buckets[0].metrics.cache_write_unknown_tokens, 40);
    assert_eq!(buckets[1].metrics.cache_write_unknown_tokens, 50);
}

#[test]
fn legacy_column_writes_have_no_unknown_share() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_unknown_writes(&db);

    // Act
    let result = query(
        &mut db,
        &spec(GroupDim::Model),
        lifetime_pricer,
        no_deadline(),
    )
    .expect("query");

    // Assert: the write is counted, none of it is unknown, and it prices.
    let legacy = &group(&result, "m-legacy").metrics;
    assert_eq!(legacy.cache_write_tokens, 50);
    assert_eq!(legacy.cache_write_unknown_tokens, 0);
    assert_eq!(legacy.cost_status, CostStatus::Priced);
}

#[test]
fn a_split_above_its_total_adds_no_unknown_share_and_stays_unpriced() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_unknown_writes(&db);

    // Act
    let result = query(
        &mut db,
        &spec(GroupDim::Model),
        lifetime_pricer,
        no_deadline(),
    )
    .expect("query");

    // Assert: the over-split write is counted, adds no unknown tokens, and
    // the group still reads the invalid split as unpriced, as before.
    let over = &group(&result, "m-over").metrics;
    assert_eq!(over.cache_write_tokens, 10);
    assert_eq!(over.cache_write_unknown_tokens, 0);
    assert_eq!(over.cost_status, CostStatus::Unpriced);
    assert_eq!(
        over.missing_prices,
        vec![missing("m-over", "cache_write_unknown_lifetime")]
    );
}
