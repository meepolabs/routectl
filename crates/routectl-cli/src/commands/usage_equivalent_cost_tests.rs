// The API-equivalent value of subscription rows: the weld against the priced
// arm, the complete-or-absent rule, and the cost cell that renders both
// channels. `include!`d into `usage_tests.rs`, so these compile into THAT module
// and its helpers stay in scope; all imports live in the host file -- do not add
// `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// Rates on `up-equiv` (an operator `[registry]` row -- an operator's declared
// API price IS the equivalence basis): input $3.00, output $15.00, cache_read
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
/// base-token-only subtotal D1 exists to refuse. Asserted against, never for.
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
    config.registry.insert(
        EQUIV_UPSTREAM.to_string(),
        RegistryEntry {
            pricing: Some(pricing),
            provider: None,
        },
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
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let rows = aggregate(db, bounds.from_ms, bounds.to_ms).expect("aggregate");
    assert_eq!(rows.len(), 1, "the fixture folds to one fine row");
    rows.into_iter().next().unwrap()
}

/// The verdict for the seeded ledger's one fine row under `config`.
fn equiv_verdict(db: &UsageDb, config: &Config) -> RowCost {
    cost_for_row(config, &no_overlay(), &equiv_agg_row(db))
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
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD))
    );
    let RowCost::Subscription(Some(equivalent)) = subscription else {
        panic!("the subscription arm resolved an equivalent");
    };
    assert_ne!(
        equivalent, EQUIV_PEAK_BASIS_USD,
        "cache reads price off the summed BILLED volume, never the peak"
    );
}

#[test]
fn an_operator_registry_row_is_the_equivalence_basis_for_a_subscription_row() {
    // A `[registry]` row on an oauth provider was dead config before the
    // equivalent existed. It is the operator's declared API price, so it is
    // exactly what their subscription usage is valued at -- and it wins whole
    // over the baked catalog, as it does on the priced arm.
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
        RowCost::Subscription(Some(EQUIV_COMPLETE_USD - 1.50))
    );
}

// --- complete or absent (D1) --------------------------------------------

#[test]
fn a_cache_active_row_without_a_cache_read_rate_reads_absent_not_base_only() {
    // Arrange: the SAME cache-active ledger, priced by rates that cover the
    // base dimensions but not cache reads -- the exact shape catalog-only
    // pricing has, since the catalog never fills cache rates.
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
    assert_eq!(absent, RowCost::Subscription(None));
    assert_eq!(present, RowCost::Subscription(Some(EQUIV_COMPLETE_USD)));
    assert_ne!(
        absent,
        RowCost::Subscription(Some(EQUIV_BASE_ONLY_USD)),
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

    for pricing in [
        PricingConfig {
            cache_write_5m_per_mtok: None,
            ..complete_rates()
        },
        PricingConfig {
            cache_write_1h_per_mtok: None,
            ..complete_rates()
        },
    ] {
        assert_eq!(
            equiv_verdict(&db, &equiv_config_subscription(pricing)),
            RowCost::Subscription(None)
        );
    }
}

#[test]
fn a_rate_of_zero_on_a_used_dimension_withholds_the_equivalent() {
    // `Some(0.0)` is reserved for a genuine free tier on the priced arm, where
    // it is a real dollar figure. As an EQUIVALENCE basis a zero rate values
    // real usage at nothing, which is a fabricated figure, not a measured one.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let free_cache_reads = PricingConfig {
        cache_read_per_mtok: Some(0.0),
        ..complete_rates()
    };

    let verdict = equiv_verdict(&db, &equiv_config_subscription(free_cache_reads));

    assert_eq!(verdict, RowCost::Subscription(None));
}

#[test]
fn a_negative_rate_on_a_used_dimension_withholds_the_equivalent() {
    // The completeness test demands STRICTLY POSITIVE rates, not merely
    // nonzero: a negative rate would otherwise credit real usage back and
    // understate the equivalent below its base-token value.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let credited_cache_reads = PricingConfig {
        cache_read_per_mtok: Some(-0.30),
        ..complete_rates()
    };

    let verdict = equiv_verdict(&db, &equiv_config_subscription(credited_cache_reads));

    assert_eq!(verdict, RowCost::Subscription(None));
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
    assert_eq!(verdict, RowCost::Subscription(Some(EQUIV_BASE_ONLY_USD)));
}

#[test]
fn the_priced_arm_keeps_its_none_rate_contributes_zero_semantics() {
    // The completeness rule is scoped to the subscription arm. `cost_usd` is a
    // forever contract whose missing-rate-contributes-zero behavior predates
    // the equivalent, so the identical gap that withholds an equivalent must
    // still produce the base-only dollar figure on the priced arm.
    let (_dir, _path, db) = temp_db();
    seed_equiv_ledger(&db);
    let no_cache_rates = PricingConfig {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        ..Default::default()
    };

    let priced = equiv_verdict(&db, &equiv_config_api_key(no_cache_rates.clone()));
    let subscription = equiv_verdict(&db, &equiv_config_subscription(no_cache_rates));

    assert_eq!(priced, RowCost::Priced(EQUIV_BASE_ONLY_USD));
    assert_eq!(subscription, RowCost::Subscription(None));
}

#[test]
fn a_kindless_subscription_row_resolves_no_equivalent() {
    // Arrange: no `[registry]` row anywhere, so rates could only come from the
    // baked catalog -- which is keyed on the PERSISTED kind. The kindless row
    // identifies no cell; its sibling, recorded with a kind, is the positive
    // control proving the catalog path does resolve for this fixture.
    let (_dir, _path, db) = temp_db();
    let mut config = catalog_fill_config();
    config.providers.insert(
        UNPRICED_PROVIDER.to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    insert_catalog_row(&db, "kindless", 1_000, None);
    insert_catalog_row(&db, "kinded", 2_000, Some("anthropic-api"));

    // Act
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    let verdicts: Vec<RowCost> = aggregate(&db, bounds.from_ms, bounds.to_ms)
        .expect("aggregate")
        .iter()
        .map(|row| cost_for_row(&config, &no_overlay(), row))
        .collect();
    let report = catalog_fill_report(&db, &config);
    let row = find(&report, UNPRICED_PROVIDER);

    // Assert: one row resolved, one did not, and the group carries the
    // resolvable row's value alone with the subscription markers intact.
    assert!(verdicts.contains(&RowCost::Subscription(None)));
    assert!(verdicts.contains(&RowCost::Subscription(Some(CATALOG_FILL_USD))));
    assert!(row.any_subscription);
    assert_eq!(row.priced_total_usd, None);
    assert_eq!(row.equivalent_total_usd, Some(CATALOG_FILL_USD));
}

// --- rollup + render ----------------------------------------------------

#[test]
fn a_mixed_group_never_folds_notional_value_into_real_spend() {
    // Arrange: one alias group spanning a priced API-key row, a resolvable
    // subscription row, and a subscription row whose rates leave it absent.
    let (_dir, _path, db) = temp_db();
    let mut config = equiv_config_api_key(complete_rates());
    config.providers.insert(
        "sub-ok".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    config.providers.insert(
        "sub-dark".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    // The dark row's upstream carries no `[registry]` row and its kind matches
    // no baked cell, so no rate resolves for it at all. The subscription row
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
        &db,
        &config,
        &no_overlay(),
        "t".into(),
        window_bounds(WindowFlag::All, fixed_now()),
        Some(GroupDim::Alias),
        false,
    )
    .expect("window report");
    let row = find(&report, "shared");

    // Assert: real spend is the priced row alone and the equivalent counts only
    // the row that resolved -- two DISTINCT figures, never each other's value
    // and never their $27.00 sum.
    assert_eq!(row.priced_total_usd, Some(EQUIV_BASE_ONLY_USD));
    assert_eq!(row.equivalent_total_usd, Some(EQUIV_BASE_ONLY_USD / 2.0));
    assert!(row.any_subscription);
    assert_eq!(
        cost_cell(row),
        format!(
            "${EQUIV_BASE_ONLY_USD:.2} (+sub ~${:.2})",
            EQUIV_BASE_ONLY_USD / 2.0
        )
    );
}

#[test]
fn the_cost_cell_labels_the_equivalent_and_leaves_the_old_strings_untouched() {
    // A display row assembled straight from an accumulator, so each cost cell
    // shape is exercised without a ledger standing between the state and the
    // string it renders.
    fn cell(priced: Option<f64>, any_subscription: bool, equivalent: Option<f64>) -> String {
        let acc = Acc {
            any_priced: priced.is_some(),
            priced_usd: priced.unwrap_or_default(),
            any_subscription,
            any_equivalent: equivalent.is_some(),
            equivalent_usd: equivalent.unwrap_or_default(),
            ..Acc::default()
        };
        cost_cell(&finalize_row("g".to_string(), acc, &TtftMap::new()))
    }

    // Resolvable subscription value: labelled as the approximation it is,
    // beside rather than inside the real figure.
    assert_eq!(cell(None, true, Some(12.34)), "n/a (sub ~$12.34)");
    assert_eq!(cell(Some(5.0), true, Some(12.34)), "$5.00 (+sub ~$12.34)");

    // Unresolvable, and the two subscription-free shapes: byte-identical to
    // what they rendered before the equivalent existed.
    assert_eq!(cell(None, true, None), "n/a (subscription)");
    assert_eq!(cell(Some(5.0), true, None), "$5.00+sub");
    assert_eq!(cell(Some(5.0), false, None), "$5.00");
    assert_eq!(cell(None, false, None), "n/a");
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
    assert_eq!(overflowed, RowCost::Subscription(None));
    assert_eq!(sane, RowCost::Subscription(Some(EQUIV_COMPLETE_USD)));
}

#[test]
fn a_non_finite_equivalent_total_degrades_to_absent() {
    // The aggregate-level defense, asserted on the accumulator directly: no
    // configurable rate can drive the CLI's summed equivalent past the finite
    // range (each per-row figure is divided by a million), so the state is
    // reachable only from a future pricing bug -- which is exactly what this
    // filter must absorb rather than render as "inf".
    fn subscription_acc(equivalent_usd: f64) -> Acc {
        Acc {
            any_subscription: true,
            any_equivalent: true,
            equivalent_usd,
            ..Acc::default()
        }
    }

    let row = finalize_row(
        "g".to_string(),
        subscription_acc(f64::INFINITY),
        &TtftMap::new(),
    );
    let control = finalize_row("g".to_string(), subscription_acc(12.34), &TtftMap::new());

    assert_eq!(row.equivalent_total_usd, None);
    assert_eq!(cost_cell(&row), "n/a (subscription)");
    assert_eq!(control.equivalent_total_usd, Some(12.34));
}
