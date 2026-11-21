// The API-equivalent value of subscription rows: the weld against the priced
// arm, the complete-or-absent rule, and the cost cell that renders both
// channels. `include!`d into `usage_tests.rs`, so these compile into THAT module
// and its helpers stay in scope; all imports live in the host file -- do not add
// `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// Rates on `up-equiv` (the operator's `[models.m.pricing]` -- an operator's
// declared API price IS the equivalence basis): input $3.00, output $15.00, cache_read
// $0.30, cache_write_5m $3.75, cache_write_1h $6.00, all per million tokens.
//
// The seeded ledger is TWO rows under one fine key, each carrying 500k input,
// 500k output, 1M cache_read, 500k cache_write_5m, 500k cache_write_1h. The
// aggregate is therefore 1M input, 1M output, 2M BILLED cache reads (a summed
// flow) against a 1M cache-read PEAK, 1M of each cache-write bucket:
//
//   input       1_000_000 * 3.00 / 1e6 =  $3.00
//   output      1_000_000 * 15.00 / 1e6 = $15.00
//   cache_read  2_000_000 * 0.30 / 1e6 =  $0.60
//   cw_5m       1_000_000 * 3.75 / 1e6 =  $3.75
//   cw_1h       1_000_000 * 6.00 / 1e6 =  $6.00
//                                         ------
//                                         $28.35
//
// Pricing the cache-read PEAK instead of the billed volume would yield $28.05,
// so the figure distinguishes the two bases rather than merely being non-zero.

/// The complete-rate total for the seeded ledger, per the derivation above.
const EQUIV_COMPLETE_USD: f64 = 28.35;

/// What the same ledger would total if cache reads were priced off the PEAK
/// rather than the summed billed volume. Asserted against, never for.
const EQUIV_PEAK_BASIS_USD: f64 = 28.05;

/// What the same ledger totals with the cache dimensions dropped -- the
/// base-token-only subtotal the complete-or-absent rule exists to refuse. Asserted against, never for.
const EQUIV_BASE_ONLY_USD: f64 = 18.00;

const EQUIV_PROVIDER: &str = "equiv";
const EQUIV_UPSTREAM: &str = "up-equiv";

/// Full per-million rates covering every dimension the seeded rows use.
fn complete_rates() -> PricingConfig {
    PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        cache_read_per_mtok: Some(0.30),
        cache_write_5m_per_mtok: Some(3.75),
        cache_write_1h_per_mtok: Some(6.0),
        cache_write_per_mtok: None,
    }
}

/// Config serving `EQUIV_PROVIDER` under `api_key_ref`, priced by `pricing`.
/// The credential reference is the ONLY difference from
/// [`equiv_config_subscription`], which is what makes the weld test a
/// comparison of the two arms rather than of two rate tables.
fn equiv_config(api_key_ref: &str, pricing: PricingConfig) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        EQUIV_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(api_key_ref),
    );
    price_models(
        &mut config,
        &["m"],
        EQUIV_PROVIDER,
        EQUIV_UPSTREAM,
        &pricing,
    );
    config
}

fn equiv_config_api_key(pricing: PricingConfig) -> Config {
    equiv_config("env://EQUIV_KEY", pricing)
}

fn equiv_config_subscription(pricing: PricingConfig) -> Config {
    equiv_config("oauth://anthropic", pricing)
}

/// Insert one cache-active row under the shared fine key. Token counts are
/// parameters so a test can zero a dimension out and watch the completeness
/// rule stop caring about its rate.
fn insert_equiv_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, cache_read, \
             cache_write_5m, cache_write_1h) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', ?3, ?4, \
             'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 500000, 500000, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                EQUIV_PROVIDER,
                EQUIV_UPSTREAM,
                cache_read,
                cache_write_5m,
                cache_write_1h,
            ],
        )
        .expect("insert equivalence row");
}

/// Seed the two-row cache-active ledger the oracle above is computed from.
fn seed_equiv_ledger(db: &UsageDb) {
    insert_equiv_row(
        db,
        "e1",
        1_000,
        Some(1_000_000),
        Some(500_000),
        Some(500_000),
    );
    insert_equiv_row(
        db,
        "e2",
        2_000,
        Some(1_000_000),
        Some(500_000),
        Some(500_000),
    );
}

/// The single aggregate row the seeded ledger folds to.
fn equiv_agg_row(db: &UsageDb) -> AggRow {
    let bounds = window_bounds(
        &crate::local_time::LocalZone::utc(),
        WindowFlag::All,
        fixed_now(),
    );
    let rows = routectl_usage::aggregate_with_cache_lifetimes(db, bounds.from_ms, bounds.to_ms)
        .expect("aggregate");
    assert_eq!(rows.len(), 1, "the fixture folds to one fine row");
    rows.into_iter().next().unwrap()
}

/// The verdict for the seeded ledger's one fine row under `config`.
fn equiv_verdict(db: &UsageDb, config: &Config) -> RowCost {
    cost_for_row(config, &no_prices(), &equiv_agg_row(db))
}

// --- the weld -----------------------------------------------------------

#[test]
fn a_subscription_equivalent_equals_what_the_same_row_would_cost_priced() {
    // Arrange: ONE ledger, two configs differing only in the credential the
    // provider carries. Under complete rates the equivalent has no licence to
    // differ from the real price by a cent -- both go through one cost body.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);

    // Act
    let priced = equiv_verdict(&db, &equiv_config_api_key(complete_rates()));
    let subscription = equiv_verdict(&db, &equiv_config_subscription(complete_rates()));

    // Assert
    assert_eq!(priced, RowCost::Priced(EQUIV_COMPLETE_USD));
    assert_eq!(
        subscription,
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD), Vec::new())
    );
    let RowCost::Subscription(Some(equivalent), _) = subscription else {
        panic!("the subscription arm resolved an equivalent");
    };
    assert_ne!(
        equivalent, EQUIV_PEAK_BASIS_USD,
        "cache reads price off the summed BILLED volume, never the peak"
    );
}

#[test]
fn an_operator_model_price_is_the_equivalence_basis_for_a_subscription_row() {
    // A `[models.X.pricing]` block on an oauth provider is the operator's
    // declared API price, so it is exactly what their subscription usage is
    // valued at.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let halved = PricingConfig {
        input_per_mtok: Some(1.5),
        ..complete_rates()
    };

    let verdict = equiv_verdict(&db, &equiv_config_subscription(halved));

    // $3.00 of input becomes $1.50; every other dimension is unchanged.
    assert_eq!(
        verdict,
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD - 1.50), Vec::new())
    );
}

// --- complete or absent -------------------------------------------------

#[test]
fn a_cache_active_row_without_a_cache_read_rate_reads_absent_not_base_only() {
    // Arrange: the SAME cache-active ledger, priced by rates that cover the
    // base dimensions but not cache reads -- the shape a table row with no
    // cache prices has.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let no_cache_read = PricingConfig {
        cache_read_per_mtok: None,
        ..complete_rates()
    };

    // Act
    let absent = equiv_verdict(&db, &equiv_config_subscription(no_cache_read));
    // The positive control: the same ledger, the same arm, complete rates. It
    // proves the fixture CAN resolve, so the absence above is the rule firing
    // rather than the fixture failing to price at all.
    let present = equiv_verdict(&db, &equiv_config_subscription(complete_rates()));

    // Assert
    assert_eq!(absent, RowCost::Subscription(None, labels(&["cache read"])));
    assert_eq!(
        present,
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD), Vec::new())
    );
    assert_ne!(
        absent,
        RowCost::Subscription(Some(EQUIV_BASE_ONLY_USD), Vec::new()),
        "a base-token-only subtotal understates a cached workload by most of \
         its value and must never be surfaced as the equivalent"
    );
}

#[test]
fn each_cache_write_bucket_alone_can_withhold_the_equivalent() {
    // Both write buckets are used by the fixture, so dropping EITHER rate on
    // its own must be enough -- a check that only covered the 5m bucket would
    // pass a 1h-only gap through.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);

    for (pricing, missing) in [
        (
            PricingConfig {
                cache_write_5m_per_mtok: None,
                ..complete_rates()
            },
            "cache write 5m",
        ),
        (
            PricingConfig {
                cache_write_1h_per_mtok: None,
                ..complete_rates()
            },
            "cache write 1h",
        ),
    ] {
        assert_eq!(
            equiv_verdict(&db, &equiv_config_subscription(pricing)),
            RowCost::Subscription(None, labels(&[missing]))
        );
    }
}

#[test]
fn a_known_zero_rate_on_a_used_dimension_keeps_the_equivalent() {
    // `Some(0.0)` is a known price: the dimension is free, so the figure is
    // complete and that dimension contributes nothing. Presence decides
    // completeness, never the sign of the rate.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let free_cache_reads = PricingConfig {
        cache_read_per_mtok: Some(0.0),
        ..complete_rates()
    };

    let verdict = equiv_verdict(&db, &equiv_config_subscription(free_cache_reads));

    // $28.35 less the $0.60 the cache reads cost at $0.30.
    assert_eq!(
        verdict,
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD - 0.60), Vec::new())
    );
}

#[test]
fn a_negative_rate_on_a_used_dimension_withholds_the_equivalent() {
    // A negative rate is not a price: it would credit real usage back and
    // understate the equivalent below its base-token value. Config refuses
    // one in `[models.X.pricing]` and the tables drop one; this pins the rule
    // for any path that still carries one.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let credited_cache_reads = PricingConfig {
        cache_read_per_mtok: Some(-0.30),
        ..complete_rates()
    };

    let verdict = equiv_verdict(&db, &equiv_config_subscription(credited_cache_reads));

    assert_eq!(
        verdict,
        RowCost::Subscription(None, labels(&["cache read"]))
    );
}

#[test]
fn a_dimension_the_row_never_used_needs_no_rate() {
    // Arrange: a ledger with no cache activity at all, priced by base rates
    // only. Requiring a cache rate here would withhold the equivalent from
    // every uncached row for no reason.
    let (_dir, _path, db) = temp_db();
    insert_equiv_row(&db, "e1", 1_000, None, None, None);
    insert_equiv_row(&db, "e2", 2_000, Some(0), Some(0), Some(0));
    let base_only = PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        ..Default::default()
    };

    // Act
    let verdict = equiv_verdict(&db, &equiv_config_subscription(base_only));

    // Assert: a reported zero and a NULL are both "unused", not "unpriced".
    assert_eq!(
        verdict,
        RowCost::Subscription(Some(EQUIV_BASE_ONLY_USD), Vec::new())
    );
}

#[test]
fn a_used_unpriced_dimension_withholds_the_figure_on_both_arms() {
    // The same rate gap withholds the priced figure and the equivalent alike:
    // a missing rate is never counted as zero, so neither arm reports the
    // base-only subtotal.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let no_cache_rates = PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        ..Default::default()
    };

    let priced = equiv_verdict(&db, &equiv_config_api_key(no_cache_rates.clone()));
    let subscription = equiv_verdict(&db, &equiv_config_subscription(no_cache_rates));

    let missing = labels(&["cache read", "cache write 1h", "cache write 5m"]);
    assert_eq!(priced, RowCost::Unpriced(missing.clone()));
    assert_ne!(priced, RowCost::Priced(EQUIV_BASE_ONLY_USD));
    assert_eq!(subscription, RowCost::Subscription(None, missing));
}

#[test]
fn a_kindless_subscription_row_on_a_removed_provider_resolves_no_equivalent() {
    // Arrange: the serving entry is gone from config and no model entry
    // prices the row, so only the kind persisted with a row can select a table
    // provider. The kinded sibling is the positive control proving the table
    // path resolves for this fixture.
    let (_dir, _path, db) = temp_db();
    insert_table_row(&db, "kindless", TABLE_PROVIDER, TABLE_UPSTREAM, None, None);
    insert_table_row(
        &db,
        "kinded",
        TABLE_PROVIDER,
        TABLE_UPSTREAM,
        Some("anthropic-api"),
        None,
    );
    let mut config = Config::default();
    config.providers.insert(
        "still-here".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );

    // Act
    let verdicts: Vec<RowCost> = routectl_usage::aggregate_with_cache_lifetimes(&db, 0, i64::MAX)
        .expect("aggregate")
        .iter()
        .map(|row| cost_for_row(&config, &both_tables(), row))
        .collect();

    // Assert: a removed provider is not a subscription any more, so the kinded
    // row prices and the kindless one does not.
    assert_eq!(verdicts.len(), 2);
    assert!(verdicts.contains(&RowCost::Unpriced(labels(&["input", "output"]))));
    assert!(verdicts.contains(&RowCost::Priced(REFRESHED_USD)));
}

// --- rollup + render ----------------------------------------------------

#[test]
fn a_mixed_group_never_folds_notional_value_into_real_spend() {
    // Arrange: one alias group spanning a priced API-key row, a resolvable
    // subscription row, and a subscription row whose rates leave it absent.
    let (_dir, _path, mut db) = temp_db();
    let mut config = equiv_config_api_key(complete_rates());
    config.providers.insert(
        "sub-ok".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    config.providers.insert(
        "sub-dark".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    // The dark row's upstream carries no model price and no price table
    // is loaded, so no rate resolves for it at all. The subscription row
    // carries HALF the priced row's tokens, so the two channels hold distinct
    // figures -- a cell that copied one channel into the other would show up.
    for (provider, upstream, request_id, tokens) in [
        (EQUIV_PROVIDER, EQUIV_UPSTREAM, "priced", 1_000_000),
        ("sub-ok", EQUIV_UPSTREAM, "sub-ok", 500_000),
        ("sub-dark", "up-nowhere", "sub-dark", 1_000_000),
    ] {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, provider_kind, \
                 stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, input_tokens, output_tokens) \
                 VALUES (1000, 1000, ?1, 'openai', 'req-model', 'shared', 'm', ?2, ?3, \
                 'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, ?4, ?4)",
                rusqlite::params![request_id, provider, upstream, tokens],
            )
            .expect("insert mixed row");
    }

    // Act
    let report = build_window_report(
        &mut db,
        &config,
        &no_prices(),
        "t".into(),
        window_bounds(
            &crate::local_time::LocalZone::utc(),
            WindowFlag::All,
            fixed_now(),
        ),
        Some(GroupDim::Alias),
        false,
    )
    .expect("window report");
    let row = find(&report, "shared");

    // Assert: real spend is the priced row alone and the equivalent counts only
    // the row that resolved -- two DISTINCT figures, never each other's value
    // and never their $27.00 sum.
    assert_eq!(row.metrics.cost_usd, Some(EQUIV_BASE_ONLY_USD));
    assert_eq!(
        row.metrics.equivalent_cost_usd,
        Some(EQUIV_BASE_ONLY_USD / 2.0)
    );
    assert!(row.any_subscription);
    assert_eq!(row.metrics.equivalent_status, EquivalentStatus::Partial);
    assert_eq!(
        cost_cell(row),
        format!(
            "${EQUIV_BASE_ONLY_USD:.2} partial (+sub ~${:.2} partial)",
            EQUIV_BASE_ONLY_USD / 2.0
        )
    );
}

/// A display row carrying only the cost figures `cost_cell` reads.
fn cost_row(priced: Option<f64>, any_subscription: bool, equivalent: Option<f64>) -> DisplayRow {
    DisplayRow {
        label: "g".to_string(),
        metrics: QueryMetrics {
            cost_usd: priced,
            equivalent_cost_usd: equivalent,
            ..QueryMetrics::default()
        },
        any_subscription,
    }
}

/// [`cost_row`] with the two completeness statuses the query folded.
fn cost_row_with(
    priced: Option<f64>,
    any_subscription: bool,
    equivalent: Option<f64>,
    cost_status: CostStatus,
    equivalent_status: EquivalentStatus,
) -> DisplayRow {
    let mut row = cost_row(priced, any_subscription, equivalent);
    row.metrics.cost_status = cost_status;
    row.metrics.equivalent_status = equivalent_status;
    row
}

#[test]
fn the_cost_cell_labels_the_equivalent_and_leaves_the_old_strings_untouched() {
    use CostStatus::{Partial, Priced, Subscription, Unpriced};
    use EquivalentStatus::{Complete, NoSubscription, Partial as EquivPartial};
    // Each cost cell shape is exercised straight from a display row, without
    // a ledger standing between the state and the string it renders.
    // (name, cost_usd, any_subscription, equivalent, cost_status, equivalent_status, cell)
    let cases = [
        // Resolvable subscription value: labelled as the approximation it
        // is, beside rather than inside the real figure.
        (
            "subscription equivalent",
            None,
            true,
            Some(12.34),
            Subscription,
            Complete,
            "n/a (sub ~$12.34)",
        ),
        (
            "priced beside a subscription equivalent",
            Some(5.0),
            true,
            Some(12.34),
            Priced,
            Complete,
            "$5.00 (+sub ~$12.34)",
        ),
        // Unresolvable, and the two subscription-free shapes: byte-identical
        // to what they rendered before the equivalent existed.
        (
            "subscription with no equivalent",
            None,
            true,
            None,
            Subscription,
            Complete,
            "n/a (subscription)",
        ),
        (
            "priced beside a subscription with no equivalent",
            Some(5.0),
            true,
            None,
            Priced,
            Complete,
            "$5.00+sub",
        ),
        (
            "priced only",
            Some(5.0),
            false,
            None,
            Priced,
            NoSubscription,
            "$5.00",
        ),
        (
            "unpriced only",
            None,
            false,
            None,
            Unpriced,
            NoSubscription,
            "n/a",
        ),
        // A figure that is a subtotal carries the mark; its complete twin
        // above does not.
        (
            "api key: priced plus unpriced is the priced subtotal",
            Some(5.0),
            false,
            None,
            Partial,
            NoSubscription,
            "$5.00 partial",
        ),
        (
            "subscription: equivalent leaves some usage unpriced",
            None,
            true,
            Some(12.34),
            Subscription,
            EquivPartial,
            "n/a (sub ~$12.34 partial)",
        ),
        (
            "mixed: both figures subtotals",
            Some(5.0),
            true,
            Some(12.34),
            Partial,
            EquivPartial,
            "$5.00 partial (+sub ~$12.34 partial)",
        ),
        (
            "mixed: complete equivalent beside the priced subtotal",
            Some(5.0),
            true,
            Some(12.34),
            Partial,
            Complete,
            "$5.00 partial (+sub ~$12.34)",
        ),
        (
            "mixed: no equivalent beside the priced subtotal",
            Some(5.0),
            true,
            None,
            Partial,
            EquivPartial,
            "$5.00 partial (+sub)",
        ),
        (
            "partial with no priced row has no subtotal to mark",
            None,
            false,
            None,
            Partial,
            NoSubscription,
            "n/a",
        ),
        // Below a dollar every figure keeps tenths of a cent, as the
        // dashboard shows it; a dollar and a zero keep cents.
        (
            "sub-dollar priced only",
            Some(0.243),
            false,
            None,
            Priced,
            NoSubscription,
            "$0.243",
        ),
        (
            "sub-dollar subscription equivalent",
            None,
            true,
            Some(0.005),
            Subscription,
            Complete,
            "n/a (sub ~$0.005)",
        ),
        (
            "mixed: sub-dollar subtotals on both channels",
            Some(0.243),
            true,
            Some(0.005),
            Partial,
            EquivPartial,
            "$0.243 partial (+sub ~$0.005 partial)",
        ),
        (
            "one dollar keeps cents",
            Some(1.0),
            true,
            Some(1.0),
            Priced,
            Complete,
            "$1.00 (+sub ~$1.00)",
        ),
        (
            "a zero keeps cents",
            Some(0.0),
            true,
            None,
            Priced,
            Complete,
            "$0.00+sub",
        ),
    ];
    for (name, priced, any_subscription, equivalent, cost_status, equivalent_status, expected) in
        cases
    {
        let row = cost_row_with(
            priced,
            any_subscription,
            equivalent,
            cost_status,
            equivalent_status,
        );
        assert_eq!(cost_cell(&row), expected, "{name}");
    }
}

/// The dashboard's `money()`, verbatim. If it changes, the precision rule the
/// table below encodes is stale: re-derive the rows from the new rule.
const DASHBOARD_MONEY: &str = "  function money(v) {
    if (!isFiniteNumber(v)) { return '-'; }
    if (v === 0) { return '0.00'; }
    return Math.abs(v) >= 1 ? v.toFixed(2) : v.toFixed(3);
  }";

#[test]
fn money_text_follows_the_dashboard_money_rule() {
    // Arrange: the rule the rows below were derived from is the one the
    // dashboard ships.
    let dom = include_str!("../handlers/status/dash_50_dom.js");
    assert!(
        dom.contains(DASHBOARD_MONEY),
        "dash_50_dom.js money() changed; re-derive these rows"
    );
    // (amount, what the dashboard's money() shows for it)
    let rows = [
        (0.243, "0.243"),
        (0.005, "0.005"),
        (1.0, "1.00"),
        (0.0, "0.00"),
        (12.34, "12.34"),
        (0.9996, "1.000"),
        (-0.243, "-0.243"),
        // An exact binary tie: toFixed breaks it away from zero.
        (1.125, "1.13"),
        (-1.125, "-1.13"),
        (0.0625, "0.063"),
        (f64::NAN, "-"),
        (f64::INFINITY, "-"),
    ];

    for (amount, dashboard) in rows {
        // Act + Assert
        assert_eq!(money_text(amount), dashboard, "{amount}");
    }
}

// --- non-finite arithmetic ----------------------------------------------

#[test]
fn a_per_row_equivalent_that_overflows_reads_absent() {
    // Arrange: every rate finite and strictly positive -- so the completeness
    // rule is satisfied and the row is on the resolving path -- but the input
    // rate is extreme enough that the row's token math overflows to infinity.
    // An unfiltered infinity would reach the accumulator, where it poisons
    // every other subscription row's value in the same group.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let overflowing = PricingConfig {
        input_per_mtok: Some(f64::MAX),
        ..complete_rates()
    };

    // Act
    let overflowed = equiv_verdict(&db, &equiv_config_subscription(overflowing));
    // Positive control: the same ledger at sane rates resolves, so the absence
    // above is the finiteness filter and not a broken fixture.
    let sane = equiv_verdict(&db, &equiv_config_subscription(complete_rates()));

    // Assert
    assert_eq!(overflowed, RowCost::Subscription(None, Vec::new()));
    assert_eq!(
        sane,
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD), Vec::new())
    );
}

#[test]
fn a_non_finite_equivalent_total_degrades_to_absent() {
    // The aggregate-level defense, asserted through the report: no
    // configurable rate can drive the summed equivalent past the finite range
    // (each per-row figure is divided by a million), so the state is reachable
    // only from a future pricing bug -- which the shared fold must absorb
    // rather than hand the cost cell an "inf".
    let (_dir, _path, mut db) = temp_db();
    seed_equiv_ledger(&db);
    let spec = routectl_usage::QuerySpec {
        from_ms: 0,
        to_ms: i64::MAX,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        model_filter: None,
        bucket: None,
        entry_accounts: Default::default(),
    };
    let fold = |db: &mut UsageDb, usd: f64| {
        let deadline = Instant::now() + std::time::Duration::from_mins(10);
        routectl_usage::query(
            db,
            &spec,
            |_| RowCost::Subscription(Some(usd), Vec::new()),
            deadline,
        )
        .expect("query")
        .totals
    };

    let overflowed = fold(&mut db, f64::INFINITY);
    let control = fold(&mut db, 12.34);

    assert_eq!(overflowed.equivalent_cost_usd, None);
    let row = DisplayRow {
        metrics: overflowed,
        ..cost_row(None, true, None)
    };
    assert_eq!(cost_cell(&row), "n/a (subscription)");
    assert!(control.equivalent_cost_usd.is_some());
}
