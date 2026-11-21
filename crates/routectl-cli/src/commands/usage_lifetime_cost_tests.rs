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

/// 500 written whose split names one lifetime with zero tokens: nothing is
/// written at a known lifetime, so the whole total has no reported lifetime.
const ZERO_ONLY_SPLIT: WriteCase = WriteCase {
    total: Some(500),
    split: Some(r#"{"3600":0}"#),
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
        // The 1-hour lifetime has no rate here, so pricing its zero entry as
        // used would leave the row unpriced rather than partial.
        (
            "split whose only entry is zero",
            ZERO_ONLY_SPLIT,
            LIFETIME_BASE_USD,
        ),
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

#[test]
fn a_split_whose_only_entry_is_zero_prices_no_writes_and_counts_the_whole_total_unknown() {
    // Arrange: the 1-hour rate exists, so a zero entry priced as used would
    // still add nothing; the unknown count is what tells the cases apart.
    let config = lifetime_config("env://LT_KEY", covered_rates(true));

    // Act
    let (verdict, metrics) = covered_outcome(&config, r#"{"3600":0}"#, 500);

    // Assert: 3.00 + 15.00 + 0.30, no write priced, all 500 unknown.
    let base = 18.30;
    assert_priced_part(verdict, base, "zero-only split verdict");
    assert_usd(metrics.cost_usd, base, "group cost");
    assert_eq!(metrics.cost_status, CostStatus::Partial);
    assert_eq!(metrics.cache_write_unknown_tokens, 500);
}

// --- the same covered-split row through a bucketed query -----------------

/// The covered-split row (at ts 1000) queried over two 5 s buckets, so it
/// lands in the first and the second sees no traffic.
fn covered_bucketed(config: &Config, split: &str, total: i64) -> routectl_usage::QueryResult {
    let (_dir, _path, mut db) = temp_db();
    insert_covered_row(&db, split, total);
    let bucket = BucketSpec {
        width_ms: 5_000,
        count: 2,
    };
    priced_query(&mut db, config, GroupDim::Model, Some(bucket))
}

/// The cost channel a case reads: the real cost for an API-key row, the
/// equivalent for a subscription row.
fn case_amount(metrics: &routectl_usage::QueryMetrics, subscription: bool) -> Option<f64> {
    if subscription {
        metrics.equivalent_cost_usd
    } else {
        metrics.cost_usd
    }
}

#[test]
fn a_bucketed_query_reads_a_covered_split_partial_on_the_group_the_totals_and_its_bucket() {
    let cases = [
        (
            "api key, covered split",
            "env://LT_KEY",
            COVERED_TOTAL,
            CostStatus::Partial,
            EquivalentStatus::NoSubscription,
            COVERED_TOTAL - 7445,
        ),
        (
            "subscription, covered split",
            "oauth://anthropic",
            COVERED_TOTAL,
            CostStatus::Subscription,
            EquivalentStatus::Partial,
            COVERED_TOTAL - 7445,
        ),
        (
            "subscription, fully split control",
            "oauth://anthropic",
            7445,
            CostStatus::Subscription,
            EquivalentStatus::Complete,
            0,
        ),
    ];
    for (name, key_ref, total, cost_status, equivalent_status, unknown) in cases {
        // Arrange
        let config = lifetime_config(key_ref, covered_rates(true));
        let subscription = key_ref.starts_with("oauth://");

        // Act
        let result = covered_bucketed(&config, COVERED_SPLIT, total);

        // Assert: the group, the totals and the row's bucket agree; the
        // bucket that saw no traffic stays empty.
        let buckets = &result.series.as_ref().expect("series").buckets;
        assert_eq!(result.groups.len(), 1, "{name}: one model group");
        for (set, m) in [
            ("group", &result.groups[0].metrics),
            ("totals", &result.totals),
            ("bucket", &buckets[0].metrics),
        ] {
            assert_eq!(m.cost_status, cost_status, "{name} {set}");
            assert_eq!(m.equivalent_status, equivalent_status, "{name} {set}");
            assert_usd(
                case_amount(m, subscription),
                COVERED_USD,
                &format!("{name} {set}"),
            );
            assert_eq!(m.cache_write_unknown_tokens, unknown, "{name} {set}");
        }
        assert_eq!(buckets[1].metrics.requests, 0, "{name}: empty bucket");
    }
}

// --- the partial mark on the rendered report ----------------------------
//
// The covered-split ledger row above, and its fully split twin
// (`{"3600":16017}`), which also prices its 8572 remaining written tokens:
//
//   covered       $18.34467                         -> $18.34
//   fully split   18.30 + 16017 x 6.00 / 1M = $18.396102 -> $18.40
//   both          $36.740772                        -> $36.74

const FULL_SPLIT: &str = r#"{"3600":16017}"#;
const MARK_API_KEY: &str = "mark-key";
const MARK_SUBSCRIPTION: &str = "mark-sub";

/// An API-key and a subscription provider on one upstream, each serving a
/// `full` and a `part` model at the covered-split rates.
fn mark_config() -> Config {
    let mut config = Config::default();
    for (provider, credential) in [
        (MARK_API_KEY, "env://MARK_KEY"),
        (MARK_SUBSCRIPTION, "oauth://anthropic"),
    ] {
        config.providers.insert(
            provider.to_string(),
            ProviderEntry::anthropic_api(credential),
        );
    }
    price_models(
        &mut config,
        &["key-full", "key-part"],
        MARK_API_KEY,
        LIFETIME_UPSTREAM,
        &covered_rates(true),
    );
    price_models(
        &mut config,
        &["sub-full", "sub-part"],
        MARK_SUBSCRIPTION,
        LIFETIME_UPSTREAM,
        &covered_rates(true),
    );
    config
}

/// One ledger row: its model, its provider and its lifetime split.
type MarkRow = (&'static str, &'static str, &'static str);

/// One covered-split-shaped row served as `model` by `provider`.
fn insert_mark_row(db: &UsageDb, model: &str, provider: &str, split: &str) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, cache_read, \
             cache_write_tokens, cache_write_lifetimes) \
             VALUES (1000, 1000, ?1, 'anthropic', 'req-model', 'al', ?1, ?2, ?3, \
             'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000, 1000000, \
             ?4, ?5)",
            rusqlite::params![model, provider, LIFETIME_UPSTREAM, COVERED_TOTAL, split],
        )
        .expect("insert mark row");
}

/// The `--detail --by model` report over a ledger of the named `models`,
/// each `(model, provider, split)`, rendered.
fn rendered_mark_report(models: &[MarkRow]) -> String {
    let (_dir, _path, mut db) = temp_db();
    for (model, provider, split) in models {
        insert_mark_row(&db, model, provider, split);
    }
    let report = build_window_report(
        &mut db,
        &mark_config(),
        &no_prices(),
        "t".into(),
        window_bounds(&zone(), WindowFlag::All, fixed_now()),
        Some(GroupDim::Model),
        true,
    )
    .expect("window report");
    render_report(&report, &zone())
}

/// The rendered cell under `column` on the row labelled `label`. Columns are
/// separated by at least two spaces and a cell holds at most one in a row,
/// so a cost cell with words in it stays one cell.
fn rendered_cell(out: &str, label: &str, column: &str) -> String {
    let cells = |line: &str| -> Vec<String> {
        line.split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut lines = out.lines().skip(1);
    let header = cells(lines.next().expect("header"));
    let index = header
        .iter()
        .position(|h| h == column)
        .unwrap_or_else(|| panic!("header carries {column}: {out}"));
    let row = lines
        .map(cells)
        .find(|row| row.first().map(String::as_str) == Some(label))
        .unwrap_or_else(|| panic!("row `{label}` present: {out}"));
    row.get(index)
        .unwrap_or_else(|| panic!("row `{label}` has a {column} cell: {row:?}"))
        .clone()
}

#[test]
fn the_report_marks_a_subtotal_on_its_group_rows_and_on_the_total_row() {
    let key_part = ("key-part", MARK_API_KEY, COVERED_SPLIT);
    let key_full = ("key-full", MARK_API_KEY, FULL_SPLIT);
    let sub_part = ("sub-part", MARK_SUBSCRIPTION, COVERED_SPLIT);
    let sub_full = ("sub-full", MARK_SUBSCRIPTION, FULL_SPLIT);
    // (name, ledger, row label, cost cell)
    let cases: [(&str, &[MarkRow], &str, &str); 9] = [
        (
            "api key group: priced subtotal",
            &[key_part, key_full],
            "key-part",
            "$18.34 partial",
        ),
        (
            "api key group: complete (control)",
            &[key_part, key_full],
            "key-full",
            "$18.40",
        ),
        (
            "api key total: priced subtotal",
            &[key_part, key_full],
            "total",
            "$36.74 partial",
        ),
        (
            "subscription group: equivalent subtotal",
            &[sub_part, sub_full],
            "sub-part",
            "n/a (sub ~$18.34 partial)",
        ),
        (
            "subscription group: complete equivalent (control)",
            &[sub_part, sub_full],
            "sub-full",
            "n/a (sub ~$18.40)",
        ),
        (
            "subscription total: equivalent subtotal",
            &[sub_part, sub_full],
            "total",
            "n/a (sub ~$36.74 partial)",
        ),
        (
            "mixed total: both figures subtotals",
            &[key_part, sub_part],
            "total",
            "$18.34 partial (+sub ~$18.34 partial)",
        ),
        (
            "mixed total: a complete equivalent beside real spend is still partial spend",
            &[key_full, sub_full],
            "total",
            "$18.40 partial (+sub ~$18.40)",
        ),
        (
            "api key total: complete (control)",
            &[key_full],
            "total",
            "$18.40",
        ),
    ];
    for (name, ledger, label, expected) in cases {
        let out = rendered_mark_report(ledger);
        assert_eq!(rendered_cell(&out, label, "cost"), expected, "{name}");
    }
}
