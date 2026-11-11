// Cache-write cost per lifetime: each lifetime prices at its own rate, a used
// lifetime with no rate leaves the row unpriced, and written tokens with no
// reported lifetime (or a split above its total) leave it unpriced whatever
// priced it. `include!`d into `usage_tests.rs`, so these compile into THAT
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

const LIFETIME_BASE_USD: f64 = 18.00;
const LIFETIME_WITH_30M_USD: f64 = 23.00;
const LIFETIME_WITH_5M_USD: f64 = 21.75;

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

/// Where the rates come from: `[models.X.pricing]` (complete-or-absent) or
/// a `[registry]` row (an absent rate counts as `0`).
enum RateSource {
    ModelConfig,
    Registry,
}

fn lifetime_config(api_key_ref: &str, source: RateSource, pricing: PricingConfig) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        LIFETIME_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(api_key_ref),
    );
    let mut model = routectl_router::ModelEntry::new(LIFETIME_PROVIDER, LIFETIME_UPSTREAM);
    match source {
        RateSource::ModelConfig => model.pricing = Some(pricing),
        RateSource::Registry => {
            config.registry.insert(
                LIFETIME_UPSTREAM.to_string(),
                RegistryEntry {
                    pricing: Some(pricing),
                    provider: None,
                },
            );
        }
    }
    config.models.insert(LIFETIME_MODEL.to_string(), model);
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
    let rows = aggregate_with_cache_lifetimes(&db, 0, i64::MAX).expect("aggregate");
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
    let RowCost::Subscription(Some(usd)) = verdict else {
        panic!("{name}: expected an equivalent of {expected}, got {verdict:?}");
    };
    assert!((usd - expected).abs() < 1e-9, "{name}: {usd} != {expected}");
}

#[test]
fn a_priced_lifetime_prices_the_row() {
    // Arrange
    let config = lifetime_config(
        "env://LT_KEY",
        RateSource::ModelConfig,
        lifetime_rates(true),
    );

    // Act
    let verdict = lifetime_verdict(&SPLIT_30M, &config);

    // Assert
    assert_priced(verdict, LIFETIME_WITH_30M_USD, "30m priced");
}

#[test]
fn a_used_lifetime_with_no_price_leaves_the_row_unpriced() {
    // Arrange: the 5m write is priced, the 30m one is not.
    let config = lifetime_config(
        "env://LT_KEY",
        RateSource::ModelConfig,
        lifetime_rates(false),
    );

    // Act
    let unpriced = lifetime_verdict(&SPLIT_30M, &config);
    let control = lifetime_verdict(&SPLIT_5M, &config);

    // Assert: the same config prices a row whose lifetime it covers.
    assert_eq!(unpriced, RowCost::Unpriced);
    assert_priced(control, LIFETIME_WITH_5M_USD, "5m control");
}

#[test]
fn writes_with_no_reported_lifetime_leave_the_row_unpriced() {
    // Every lifetime the rows report is priced, so only the unknown
    // remainder can refuse them.
    let config = lifetime_config(
        "env://LT_KEY",
        RateSource::ModelConfig,
        lifetime_rates(true),
    );
    let cases = [
        ("split covering part of the total", PARTIAL_SPLIT),
        ("total with no split", TOTAL_ONLY),
    ];
    for (name, writes) in cases {
        // Act
        let verdict = lifetime_verdict(&writes, &config);

        // Assert
        assert_eq!(verdict, RowCost::Unpriced, "{name}");
    }
}

#[test]
fn a_split_above_its_total_leaves_the_row_unpriced() {
    // Arrange: the only lifetime the split names is priced.
    let config = lifetime_config(
        "env://LT_KEY",
        RateSource::ModelConfig,
        lifetime_rates(true),
    );

    // Act
    let verdict = lifetime_verdict(&SPLIT_ABOVE_TOTAL, &config);

    // Assert
    assert_eq!(verdict, RowCost::Unpriced);
}

#[test]
fn a_subscription_with_unknown_lifetime_writes_has_no_equivalent() {
    // Arrange
    let config = lifetime_config(
        "oauth://anthropic",
        RateSource::ModelConfig,
        lifetime_rates(true),
    );

    // Act
    let unknown = lifetime_verdict(&PARTIAL_SPLIT, &config);
    let control = lifetime_verdict(&SPLIT_30M, &config);

    // Assert: the same subscription values a fully split row.
    assert_eq!(unknown, RowCost::Subscription(None));
    assert_equivalent(control, LIFETIME_WITH_30M_USD, "subscription control");
}

#[test]
fn a_registry_row_counts_an_unpriced_lifetime_as_zero_but_not_unknown_writes() {
    // Arrange: a `[registry]` row with no 30m price.
    let config = lifetime_config("env://LT_KEY", RateSource::Registry, lifetime_rates(false));

    // Act
    let unpriced_lifetime = lifetime_verdict(&SPLIT_30M, &config);
    let unknown_remainder = lifetime_verdict(&PARTIAL_SPLIT, &config);

    // Assert
    assert_priced(unpriced_lifetime, LIFETIME_BASE_USD, "registry 30m at 0");
    assert_eq!(unknown_remainder, RowCost::Unpriced);
}
