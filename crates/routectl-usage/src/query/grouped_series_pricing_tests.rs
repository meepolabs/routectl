// Groups and totals are priced over the whole window whether or not a series
// is requested; each series bucket is priced over that bucket alone. Split
// from `grouped_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so the fixtures there stay in
// scope. All imports come from the host -- do not add `use` lines here.

const HOUR_MS: i64 = 3_600_000;

/// One ledger row of the pricing-grain fixture.
struct PricedRow {
    request_id: &'static str,
    ts_start: i64,
    model: &'static str,
    provider: &'static str,
    alias: &'static str,
    seat: Option<&'static str>,
    outcome: &'static str,
    resolved_class: Option<&'static str>,
    input_tokens: i64,
    cache_read: Option<i64>,
    cache_write_tokens: Option<i64>,
    cache_write_lifetimes: Option<&'static str>,
}

const OK_ROW: PricedRow = PricedRow {
    request_id: "",
    ts_start: 0,
    model: "m-plain",
    provider: "paid",
    alias: "a2",
    seat: Some("s2"),
    outcome: "ok",
    resolved_class: None,
    input_tokens: 0,
    cache_read: None,
    cache_write_tokens: None,
    cache_write_lifetimes: None,
};

/// Three days of traffic: a fine group whose cache write has a known lifetime
/// on day 0 and an unknown one on day 1 (and reports cache reads only on
/// day 0), a plainly priced group on days 0 and 2 with one classified error,
/// and a subscription group reporting cache reads on days 1 and 2.
const PRICING_GRAIN_ROWS: [PricedRow; 7] = [
    PricedRow {
        request_id: "cache-day0",
        ts_start: HOUR_MS,
        model: "m-cache",
        alias: "a1",
        seat: Some("s1"),
        input_tokens: 1_000,
        cache_read: Some(400),
        cache_write_tokens: Some(100),
        cache_write_lifetimes: Some(r#"{"300":100}"#),
        ..OK_ROW
    },
    PricedRow {
        request_id: "cache-day1",
        ts_start: DAY_MS + 2 * HOUR_MS,
        model: "m-cache",
        alias: "a1",
        seat: Some("s1"),
        input_tokens: 1_000,
        cache_write_tokens: Some(50),
        ..OK_ROW
    },
    PricedRow {
        request_id: "plain-day0",
        ts_start: 3 * HOUR_MS,
        input_tokens: 2_000,
        ..OK_ROW
    },
    PricedRow {
        request_id: "plain-day2",
        ts_start: 2 * DAY_MS + 5 * HOUR_MS,
        input_tokens: 2_000,
        ..OK_ROW
    },
    PricedRow {
        request_id: "plain-error-day2",
        ts_start: 2 * DAY_MS + 6 * HOUR_MS,
        outcome: "upstream_error",
        resolved_class: Some("http-5xx"),
        ..OK_ROW
    },
    PricedRow {
        request_id: "sub-day1",
        ts_start: DAY_MS + 4 * HOUR_MS,
        model: "m-sub",
        provider: "sub",
        alias: "a1",
        seat: None,
        input_tokens: 3_000,
        cache_read: Some(900),
        ..OK_ROW
    },
    PricedRow {
        request_id: "sub-day2",
        ts_start: 2 * DAY_MS + 7 * HOUR_MS,
        model: "m-sub",
        provider: "sub",
        alias: "a1",
        seat: None,
        input_tokens: 500,
        cache_read: Some(0),
        ..OK_ROW
    },
];

fn insert_priced(db: &UsageDb, r: &PricedRow) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, seat, stream, outcome, \
             resolved_class, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, cache_read, cache_write_tokens, \
             cache_write_lifetimes) \
             VALUES (?1, ?1, ?2, 'anthropic', ?3, ?4, ?3, ?5, 'u1', ?6, 0, ?7, ?8, 10, \
             0, 0, 1, 0, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                r.ts_start,
                r.request_id,
                r.model,
                r.alias,
                r.provider,
                r.seat,
                r.outcome,
                r.resolved_class,
                r.input_tokens,
                r.cache_read,
                r.cache_write_tokens,
                r.cache_write_lifetimes,
            ],
        )
        .expect("insert pricing-grain row");
}

/// A pricer with the real one's lifetime rule: a row with any cache-write
/// token of no known lifetime has no price. Rates are powers of two so every
/// sum is exact.
fn lifetime_pricer(row: &AggRow) -> RowCost {
    if row.key.provider.as_deref() == Some("sub") {
        return RowCost::Subscription(Some(row.input_tokens as f64 / 1024.0), Vec::new());
    }
    let lifetimes = row
        .cache_write_lifetimes
        .as_ref()
        .expect("every priced row carries its lifetimes");
    if lifetimes.unknown_tokens > 0 || lifetimes.invalid_split {
        return RowCost::Unpriced(vec!["cache_write_unknown_lifetime".to_string()]);
    }
    let written: i64 = lifetimes.tokens_by_lifetime.values().sum();
    RowCost::Priced(row.input_tokens as f64 / 1024.0 + written as f64 / 512.0)
}

fn three_day_spec(group_by: GroupDim, bucket: Option<BucketSpec>) -> QuerySpec {
    QuerySpec {
        from_ms: 0,
        to_ms: 3 * DAY_MS,
        group_by,
        alias_filter: None,
        provider_filter: None,
        bucket,
    }
}

/// The two series grains the dashboard requests over the fixture's window.
const GRAINS: [(&str, BucketSpec); 2] = [
    (
        "hour",
        BucketSpec {
            width_ms: HOUR_MS,
            count: 72,
        },
    ),
    (
        "day",
        BucketSpec {
            width_ms: DAY_MS,
            count: 3,
        },
    ),
];

fn seeded_pricing_grain_db() -> (TempDir, UsageDb) {
    let (dir, db) = open_db();
    for row in &PRICING_GRAIN_ROWS {
        insert_priced(&db, row);
    }
    (dir, db)
}

#[test]
fn groups_and_totals_are_identical_with_and_without_a_series() {
    // Arrange
    let (_dir, mut db) = seeded_pricing_grain_db();

    for dim in GroupDim::ALL {
        let plain = query(
            &mut db,
            &three_day_spec(dim, None),
            lifetime_pricer,
            no_deadline(),
        )
        .expect("unbucketed query");
        for (grain, bucket) in GRAINS {
            // Act
            let series = query(
                &mut db,
                &three_day_spec(dim, Some(bucket)),
                lifetime_pricer,
                no_deadline(),
            )
            .expect("bucketed query");

            // Assert: every field of every group and of the totals.
            let case = format!("group_by={} bucket={grain}", dim.as_str());
            assert_eq!(series.groups, plain.groups, "groups: {case}");
            assert_eq!(series.totals, plain.totals, "totals: {case}");
        }
    }
}

#[test]
fn the_pricing_grain_fixture_prices_its_unknown_lifetime_row_only_per_bucket() {
    // The control for the test above: the fixture can tell the two grains
    // apart only if the whole-window figures differ from what the day-0 slice
    // alone would give.
    let (_dir, mut db) = seeded_pricing_grain_db();

    // Act
    let plain = query(
        &mut db,
        &three_day_spec(GroupDim::Model, None),
        lifetime_pricer,
        no_deadline(),
    )
    .expect("unbucketed query");

    // Assert: over the window the unknown-lifetime group is unpriced, and its
    // day-1 prompt (no cache read reported that day) is in the hit share.
    let cache = &group(&plain, "m-cache").metrics;
    assert_eq!(cache.cost_status, CostStatus::Unpriced);
    assert_eq!(cache.cost_usd, None);
    assert_close(cache.cache_hit_pct, 400.0 * 100.0 / 2_550.0);
    assert_eq!(plain.totals.cost_status, CostStatus::Partial);
    assert_close(plain.totals.cost_usd, 4_000.0 / 1024.0);
}

#[test]
fn each_series_bucket_is_priced_over_its_own_rows() {
    // Arrange
    let (_dir, mut db) = seeded_pricing_grain_db();
    let (_, day) = GRAINS[1];

    // Act
    let result = query(
        &mut db,
        &three_day_spec(GroupDim::Model, Some(day)),
        lifetime_pricer,
        no_deadline(),
    )
    .expect("bucketed query");

    // Assert: day 0 alone holds only known lifetimes, so it is fully priced;
    // day 1's hit share excludes the unknown-lifetime row's prompt, which
    // reported no cache read that day.
    let buckets = &result.series.as_ref().expect("series present").buckets;
    let day0 = &buckets[0].metrics;
    assert_eq!(day0.cost_status, CostStatus::Priced);
    assert_close(
        day0.cost_usd,
        1_000.0 / 1024.0 + 100.0 / 512.0 + 2_000.0 / 1024.0,
    );
    let day1 = &buckets[1].metrics;
    assert_eq!(day1.cost_status, CostStatus::Partial);
    assert_close(day1.cache_hit_pct, 900.0 * 100.0 / 3_900.0);
}
