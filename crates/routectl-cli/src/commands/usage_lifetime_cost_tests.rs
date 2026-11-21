// Cache-write cost per lifetime: each lifetime prices at its own rate, a used
// lifetime with no rate leaves the row unpriced, written tokens with no
// reported lifetime are never priced (the rest of the row is, as a subtotal),
// and a split above its total leaves the row unpriced whatever priced it. `include!`d into `usage_tests.rs`, so these compile into THAT
// module and its helpers stay in scope; all imports live in the host file --
// do not add `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// Every seeded row carries 1M input + 1M output tokens plus the cache writes
// its case names. Rates: input $3.00, output $15.00, the 5-minute write $3.75
// and, where a case prices it, the 30-minute write $5.00, all per million:
//
//   base (input + output)          3.00 + 15.00         = $18.00
//   base + 1M written at 30m       18.00 + 5.00         = $23.00
//   base + 1M written at 5m        18.00 + 3.75         = $21.75
//   base + 600k written at 5m      18.00 + 2.25         = $20.25

const LIFETIME_WITH_30M_USD: f64 = 23.00;
const LIFETIME_WITH_5M_USD: f64 = 21.75;
const LIFETIME_BASE_USD: f64 = 18.00;
const LIFETIME_PARTIAL_5M_USD: f64 = 20.25;

const LIFETIME_PROVIDER: &str = "lt";
const LIFETIME_UPSTREAM: &str = "up-lifetime";
const LIFETIME_MODEL: &str = "lifetime-model";

/// How one seeded row reports its cache writes.
struct WriteCase {
    total: Option<i64>,
    split: Option<&'static str>,
}

/// 1M written, all at 30 minutes.
const SPLIT_30M: WriteCase = WriteCase {
    total: Some(1_000_000),
    split: Some(r#"{"1800":1000000}"#),
};

/// 1M written, all at 5 minutes.
const SPLIT_5M: WriteCase = WriteCase {
    total: Some(1_000_000),
    split: Some(r#"{"300":1000000}"#),
};

/// 1M written, 600k of it at 5 minutes and 400k with no reported lifetime.
const PARTIAL_SPLIT: WriteCase = WriteCase {
    total: Some(1_000_000),
    split: Some(r#"{"300":600000}"#),
};

/// 1M written with no lifetime split at all.
const TOTAL_ONLY: WriteCase = WriteCase {
    total: Some(1_000_000),
    split: None,
};

/// A split summing above its own total.
const SPLIT_ABOVE_TOTAL: WriteCase = WriteCase {
    total: Some(100),
    split: Some(r#"{"300":200}"#),
};

fn lifetime_rates(with_30m: bool) -> PricingConfig {
    PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        cache_write_5m_per_mtok: Some(3.75),
        cache_write_per_mtok: with_30m.then(|| BTreeMap::from([("30m".to_string(), 5.0)])),
        ..Default::default()
    }
}

fn lifetime_config(api_key_ref: &str, pricing: PricingConfig) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        LIFETIME_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(api_key_ref),
    );
    price_models(
        &mut config,
        &[LIFETIME_MODEL],
        LIFETIME_PROVIDER,
        LIFETIME_UPSTREAM,
        &pricing,
    );
    config
}

fn insert_lifetime_row(db: &UsageDb, writes: &WriteCase) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, cache_write_tokens, \
             cache_write_lifetimes) \
             VALUES (1000, 1000, 'r', 'anthropic', 'req-model', 'al', ?1, ?2, ?3, \
             'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000, ?4, ?5)",
            rusqlite::params![
                LIFETIME_MODEL,
                LIFETIME_PROVIDER,
                LIFETIME_UPSTREAM,
                writes.total,
                writes.split,
            ],
        )
        .expect("insert lifetime row");
}

/// The verdict for a one-row ledger written as `writes`, priced under `config`.
fn lifetime_verdict(writes: &WriteCase, config: &Config) -> RowCost {
    let (_dir, _path, db) = temp_db();
    insert_lifetime_row(&db, writes);
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");
    assert_eq!(rows.len(), 1, "the fixture folds to one fine row");
    cost_for_row(config, &no_prices(), &rows[0])
}

fn assert_priced(verdict: RowCost, expected: f64, name: &str) {
    let RowCost::Priced(usd) = verdict else {
        panic!("{name}: expected Priced({expected}), got {verdict:?}");
    };
    assert!((usd - expected).abs() < 1e-9, "{name}: {usd} != {expected}");
}

fn assert_equivalent(verdict: RowCost, expected: f64, name: &str) {
    let RowCost::Subscription(Some(usd), _) = verdict else {
        panic!("{name}: expected an equivalent of {expected}, got {verdict:?}");
    };
    assert!((usd - expected).abs() < 1e-9, "{name}: {usd} != {expected}");
}

#[test]
fn a_priced_lifetime_prices_the_row() {
    // Arrange
    let config = lifetime_config("env://LT_KEY", lifetime_rates(true));

    // Act
    let verdict = lifetime_verdict(&SPLIT_30M, &config);

    // Assert
    assert_priced(verdict, LIFETIME_WITH_30M_USD, "30m priced");
}

#[test]
fn a_used_lifetime_with_no_price_leaves_the_row_unpriced() {
    // Arrange: the 5m write is priced, the 30m one is not.
    let config = lifetime_config("env://LT_KEY", lifetime_rates(false));

    // Act
    let unpriced = lifetime_verdict(&SPLIT_30M, &config);
    let control = lifetime_verdict(&SPLIT_5M, &config);

    // Assert: the same config prices a row whose lifetime it covers.
    assert_eq!(unpriced, RowCost::Unpriced(labels(&["cache write 30m"])));
    assert_priced(control, LIFETIME_WITH_5M_USD, "5m control");
}

fn assert_priced_part(verdict: RowCost, expected: f64, name: &str) {
    let RowCost::PricedPart(usd, missing) = verdict else {
        panic!("{name}: expected PricedPart({expected}), got {verdict:?}");
    };
    assert!((usd - expected).abs() < 1e-9, "{name}: {usd} != {expected}");
    assert_eq!(missing, labels(&[UNKNOWN_LIFETIME_LABEL]), "{name}");
}

#[test]
fn writes_with_no_reported_lifetime_price_only_the_rest_of_the_row() {
    // Every lifetime the rows report is priced, so only the unknown
    // remainder is left out.
    let config = lifetime_config("env://LT_KEY", lifetime_rates(true));
    let cases = [
        (
            "split covering part of the total",
            PARTIAL_SPLIT,
            LIFETIME_PARTIAL_5M_USD,
        ),
        ("total with no split", TOTAL_ONLY, LIFETIME_BASE_USD),
    ];
    for (name, writes, expected) in cases {
        // Act
        let verdict = lifetime_verdict(&writes, &config);

        // Assert
        assert_priced_part(verdict, expected, name);
    }
}

#[test]
fn a_split_above_its_total_leaves_the_row_unpriced() {
    // Arrange: the only lifetime the split names is priced.
    let config = lifetime_config("env://LT_KEY", lifetime_rates(true));

    // Act
    let verdict = lifetime_verdict(&SPLIT_ABOVE_TOTAL, &config);

    // Assert
    assert_eq!(
        verdict,
        RowCost::Unpriced(labels(&[UNKNOWN_LIFETIME_LABEL]))
    );
}

#[test]
fn a_subscription_with_unknown_lifetime_writes_values_only_the_rest() {
    // Arrange
    let config = lifetime_config("oauth://anthropic", lifetime_rates(true));

    // Act
    let unknown = lifetime_verdict(&PARTIAL_SPLIT, &config);
    let control = lifetime_verdict(&SPLIT_30M, &config);

    // Assert: the covered value, still naming the gap; a fully split row
    // names none.
    assert_eq!(
        unknown,
        RowCost::Subscription(
            Some(LIFETIME_PARTIAL_5M_USD),
            labels(&[UNKNOWN_LIFETIME_LABEL])
        )
    );
    assert_equivalent(control, LIFETIME_WITH_30M_USD, "subscription control");
}

// --- a split covering part of its total, through the grouped query --------
//
// One ledger row: 1M input, 1M output, 1M cache read, and 16017 written of
// which the split reports 7445 at 1 hour. Rates per million: input $3.00,
// output $15.00, cache read $0.30, 1-hour write $6.00. The 8572 written
// tokens with no lifetime are never priced:
//
//   3.00 + 15.00 + 0.30 + 7445 x 6.00 / 1M   = $18.34467

const COVERED_SPLIT: &str = r#"{"3600":7445}"#;
const COVERED_TOTAL: i64 = 16_017;
const COVERED_USD: f64 = 18.344_67;

fn covered_rates(with_input: bool) -> PricingConfig {
    PricingConfig {
        input_per_mtok: with_input.then_some(3.0),
        output_per_mtok: Some(15.0),
        cache_read_per_mtok: Some(0.30),
        cache_write_1h_per_mtok: Some(6.0),
        ..Default::default()
    }
}

fn insert_covered_row(db: &UsageDb, split: &str, total: i64) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, cache_read, \
             cache_write_tokens, cache_write_lifetimes) \
             VALUES (1000, 1000, 'r', 'anthropic', 'req-model', 'al', ?1, ?2, ?3, \
             'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000, 1000000, \
             ?4, ?5)",
            rusqlite::params![
                LIFETIME_MODEL,
                LIFETIME_PROVIDER,
                LIFETIME_UPSTREAM,
                total,
                split,
            ],
        )
        .expect("insert covered row");
}

/// The verdict for the one covered-split row and the metric set the grouped
/// query folds it into.
fn covered_outcome(
    config: &Config,
    split: &str,
    total: i64,
) -> (RowCost, routectl_usage::QueryMetrics) {
    let (_dir, _path, mut db) = temp_db();
    insert_covered_row(&db, split, total);
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");
    assert_eq!(rows.len(), 1, "the fixture folds to one fine row");
    let verdict = cost_for_row(config, &no_prices(), &rows[0]);
    let mut groups = priced_query(&mut db, config, GroupDim::Model, None).groups;
    assert_eq!(groups.len(), 1, "one model group");
    (verdict, groups.remove(0).metrics)
}

fn assert_usd(actual: Option<f64>, expected: f64, name: &str) {
    let usd = actual.unwrap_or_else(|| panic!("{name}: expected {expected}, got none"));
    assert!((usd - expected).abs() < 1e-9, "{name}: {usd} != {expected}");
}

#[test]
fn an_api_key_row_prices_its_covered_part_and_reads_partial() {
    // Arrange
    let config = lifetime_config("env://LT_KEY", covered_rates(true));

    // Act
    let (verdict, metrics) = covered_outcome(&config, COVERED_SPLIT, COVERED_TOTAL);

    // Assert: the 1-hour part and every other dimension priced, the
    // remainder named and never priced.
    assert_priced_part(verdict, COVERED_USD, "covered verdict");
    assert_usd(metrics.cost_usd, COVERED_USD, "group cost");
    assert_eq!(metrics.cost_status, CostStatus::Partial);
    assert_eq!(
        metrics.missing_price_dimensions,
        labels(&[UNKNOWN_LIFETIME_LABEL])
    );
    assert_eq!(metrics.cache_write_unknown_tokens, COVERED_TOTAL - 7445);
    assert_eq!(metrics.equivalent_cost_usd, None);
}

#[test]
fn a_covered_split_row_with_another_missing_rate_is_wholly_unpriced() {
    // Arrange: the same row with no input rate.
    let config = lifetime_config("env://LT_KEY", covered_rates(false));

    // Act
    let (verdict, metrics) = covered_outcome(&config, COVERED_SPLIT, COVERED_TOTAL);

    // Assert
    assert_eq!(
        verdict,
        RowCost::Unpriced(labels(&[UNKNOWN_LIFETIME_LABEL, "input"]))
    );
    assert_eq!(metrics.cost_usd, None);
    assert_eq!(metrics.cost_status, CostStatus::Unpriced);
}

#[test]
fn a_split_above_its_total_is_unpriced_in_the_grouped_query() {
    // Arrange: every dimension the row uses is priced.
    let config = lifetime_config("env://LT_KEY", covered_rates(true));

    // Act: the split reports more than the row's total.
    let (verdict, metrics) = covered_outcome(&config, COVERED_SPLIT, 7000);

    // Assert
    assert_eq!(
        verdict,
        RowCost::Unpriced(labels(&[UNKNOWN_LIFETIME_LABEL]))
    );
    assert_eq!(metrics.cost_usd, None);
    assert_eq!(metrics.cost_status, CostStatus::Unpriced);
}

#[test]
fn a_subscription_row_values_its_covered_part_as_the_equivalent() {
    // Arrange
    let config = lifetime_config("oauth://anthropic", covered_rates(true));

    // Act
    let (verdict, metrics) = covered_outcome(&config, COVERED_SPLIT, COVERED_TOTAL);

    // Assert: real spend stays absent; the equivalent is the covered value.
    let RowCost::Subscription(equivalent, missing) = verdict else {
        panic!("expected a subscription verdict, got {verdict:?}");
    };
    assert_usd(equivalent, COVERED_USD, "row equivalent");
    assert_eq!(missing, labels(&[UNKNOWN_LIFETIME_LABEL]));
    assert_usd(metrics.equivalent_cost_usd, COVERED_USD, "group equivalent");
    assert_eq!(metrics.cost_usd, None);
    assert_eq!(metrics.cost_status, CostStatus::Subscription);
    assert_eq!(
        metrics.missing_price_dimensions,
        labels(&[UNKNOWN_LIFETIME_LABEL])
    );
}

#[test]
fn a_fully_split_row_reads_priced() {
    // Arrange: the split covers the whole total.
    let config = lifetime_config("env://LT_KEY", covered_rates(true));

    // Act
    let (verdict, metrics) = covered_outcome(&config, COVERED_SPLIT, 7445);

    // Assert: 3.00 + 15.00 + 0.30 + 7445 x 6.00 / 1M, every write priced.
    let full = 18.344_67;
    assert_priced(verdict, full, "fully split verdict");
    assert_usd(metrics.cost_usd, full, "group cost");
    assert_eq!(metrics.cost_status, CostStatus::Priced);
    assert!(metrics.missing_price_dimensions.is_empty());
}

#[test]
fn a_row_whose_only_use_is_unknown_lifetime_writes_stays_unpriced() {
    // Arrange: nothing but written tokens with no lifetime, so no part of
    // the row can be priced; a covered subtotal would read as a measured $0.
    let config = lifetime_config("env://LT_KEY", lifetime_rates(true));
    let (_dir, _path, db) = temp_db();
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, cache_write_tokens) \
             VALUES (1000, 1000, 'r', 'anthropic', 'req-model', 'al', ?1, ?2, ?3, \
             'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 500)",
            rusqlite::params![LIFETIME_MODEL, LIFETIME_PROVIDER, LIFETIME_UPSTREAM],
        )
        .expect("insert writes-only row");
    let rows = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");

    // Act
    let verdict = cost_for_row(&config, &no_prices(), &rows[0]);

    // Assert
    assert_eq!(
        verdict,
        RowCost::Unpriced(labels(&[UNKNOWN_LIFETIME_LABEL]))
    );
}
