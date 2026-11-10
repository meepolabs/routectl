// Usage cost from the price tables: per-field precedence, the persisted-kind
// fallback, known-zero and unknown prices, and the subscription arm.
// `include!`d into `usage_tests.rs`, so these compile into THAT module and its
// helpers stay in scope; all imports live in the host file -- do not add `use`
// lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// Every seeded row carries 1M input + 1M output tokens and nothing else, so a
// row's cost is `input_rate + output_rate` in dollars. The refreshed table
// prices `claude-sonnet-4-6` under `anthropic` at $3.00 in / $15.00 out
// ($18.00); the shipped table at $1.00 / $2.00 ($3.00), so the two layers are
// told apart by the figure alone.

/// The refreshed-table figure for one seeded row.
const REFRESHED_USD: f64 = 18.00;

/// The shipped-table figure for one seeded row.
const SHIPPED_USD: f64 = 3.00;

/// A real first-party id: the anthropic fallback selects the `anthropic` table
/// provider by kind, so the upstream is matched exactly inside it.
const TABLE_UPSTREAM: &str = "claude-sonnet-4-6";

/// An upstream no table and no config prices.
const UNLISTED_UPSTREAM: &str = "unlisted-model";

/// The served `[providers]` entry. No `[registry]` row names it.
const TABLE_PROVIDER: &str = "anth";

/// The served `[models]` nickname the seeded rows record.
const TABLE_MODEL: &str = "sonnet";

fn price_table(rows: &serde_json::Value) -> PriceTable {
    let doc = serde_json::json!({
        "schema_version": 1,
        "generated_at": "2026-11-01",
        "providers": [{ "id": "anthropic", "models": rows }],
    });
    routectl_router::prices::parse_and_validate(&serde_json::to_vec(&doc).unwrap())
        .expect("fixture table parses")
}

fn refreshed_rows() -> serde_json::Value {
    serde_json::json!([
        { "id": TABLE_UPSTREAM, "input": 3.0, "output": 15.0 },
        { "id": "free-model", "input": 0.0, "output": 0.0, "cache_read": 0.0 },
    ])
}

/// A shipped table lives for the process in production; a leaked fixture
/// gives the test the same `'static` shape.
fn shipped(rows: &serde_json::Value) -> &'static PriceTable {
    Box::leak(Box::new(price_table(rows)))
}

fn tables(
    refreshed: Option<serde_json::Value>,
    shipped_rows: Option<serde_json::Value>,
) -> PriceSnapshot {
    PriceSnapshot::from_tables(
        refreshed.map(|rows| std::sync::Arc::new(price_table(&rows))),
        shipped_rows.map(|rows| shipped(&rows)),
    )
}

/// Both layers populated, each with a distinguishable figure.
fn both_tables() -> PriceSnapshot {
    tables(Some(refreshed_rows()), Some(shipped_rows()))
}

/// An API-key `anthropic-api` provider with a `[models]` entry and no
/// `[registry]` pricing at all.
fn table_config(api_key_ref: &str) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        TABLE_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(api_key_ref),
    );
    config.models.insert(
        TABLE_MODEL.to_string(),
        routectl_router::ModelEntry::new(TABLE_PROVIDER, TABLE_UPSTREAM),
    );
    config
}

/// Insert one 1M-in / 1M-out row served by `provider` on `upstream`.
fn insert_table_row(
    db: &UsageDb,
    request_id: &str,
    provider: &str,
    upstream: &str,
    kind: Option<&str>,
    cache_read: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, cache_read) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', ?2, ?3, ?4, ?5, \
             0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000, ?6)",
            rusqlite::params![
                request_id,
                TABLE_MODEL,
                provider,
                upstream,
                kind,
                cache_read
            ],
        )
        .expect("insert table-priced row");
}

/// The verdict for the single aggregate row of `db`.
fn table_verdict(db: &UsageDb, config: &Config, prices: &PriceSnapshot) -> RowCost {
    let rows = aggregate(db, 0, i64::MAX).expect("aggregate");
    assert_eq!(rows.len(), 1, "the fixture folds to one fine row");
    cost_for_row(config, prices, &rows[0])
}

fn seeded(upstream: &str, kind: Option<&str>) -> (TempDir, UsageDb) {
    let (dir, _path, db) = temp_db();
    insert_table_row(&db, "r", TABLE_PROVIDER, upstream, kind, None);
    (dir, db)
}

/// `[registry]` pricing for the table upstream at $1.00 / $1.00 ($2.00).
fn registry_row() -> RegistryEntry {
    RegistryEntry {
        pricing: Some(PricingConfig {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(1.0),
            ..Default::default()
        }),
        provider: None,
    }
}

/// `[models.sonnet.pricing]` fixing the output price at $5.00 only.
fn model_output_price(config: &mut Config) {
    config.models.get_mut(TABLE_MODEL).unwrap().pricing = Some(PricingConfig {
        output_per_mtok: Some(5.0),
        ..Default::default()
    });
}

#[test]
fn each_price_source_wins_in_its_precedence_order() {
    struct Row {
        name: &'static str,
        configure: fn(&mut Config),
        prices: fn() -> PriceSnapshot,
        expected: RowCost,
    }
    let rows = [
        Row {
            name: "shipped only",
            configure: |_| {},
            prices: || tables(None, Some(shipped_rows())),
            expected: RowCost::Priced(SHIPPED_USD),
        },
        Row {
            name: "refreshed over shipped",
            configure: |_| {},
            prices: both_tables,
            expected: RowCost::Priced(REFRESHED_USD),
        },
        Row {
            name: "registry over both tables, whole",
            configure: |c| {
                c.registry
                    .insert(TABLE_UPSTREAM.to_string(), registry_row());
            },
            prices: both_tables,
            expected: RowCost::Priced(2.00),
        },
        Row {
            // $3.00 input from the refreshed table, $5.00 output from the model.
            name: "model-config field over the refreshed field",
            configure: model_output_price,
            prices: both_tables,
            expected: RowCost::Priced(8.00),
        },
        Row {
            name: "no table and no config",
            configure: |_| {},
            prices: no_prices,
            expected: RowCost::Unpriced,
        },
    ];
    for row in rows {
        let (_dir, db) = seeded(TABLE_UPSTREAM, Some("anthropic-api"));
        let mut config = table_config("env://ANTH_KEY");
        (row.configure)(&mut config);

        let verdict = table_verdict(&db, &config, &(row.prices)());

        assert_eq!(verdict, row.expected, "{}", row.name);
    }
}

/// The shipped layer's fixture: the table upstream at $1.00 / $2.00.
fn shipped_rows() -> serde_json::Value {
    serde_json::json!([{ "id": TABLE_UPSTREAM, "input": 1.0, "output": 2.0 }])
}

#[test]
fn a_model_no_table_prices_is_unpriced_never_zero() {
    // Arrange: both tables loaded, but neither lists the upstream. The
    // positive control is the listed upstream under the same tables.
    let (_dir, unlisted) = seeded(UNLISTED_UPSTREAM, Some("anthropic-api"));
    let (_dir2, listed) = seeded(TABLE_UPSTREAM, Some("anthropic-api"));
    let config = table_config("env://ANTH_KEY");

    // Act
    let miss = table_verdict(&unlisted, &config, &both_tables());
    let hit = table_verdict(&listed, &config, &both_tables());

    // Assert
    assert_eq!(miss, RowCost::Unpriced);
    assert_ne!(miss, RowCost::Priced(0.0));
    assert_eq!(hit, RowCost::Priced(REFRESHED_USD));
}

#[test]
fn a_subscription_row_on_an_unpriced_model_carries_no_equivalent() {
    let (_dir, unlisted) = seeded(UNLISTED_UPSTREAM, Some("anthropic-api"));
    let (_dir2, listed) = seeded(TABLE_UPSTREAM, Some("anthropic-api"));
    let config = table_config("oauth://anthropic");

    let miss = table_verdict(&unlisted, &config, &both_tables());
    let hit = table_verdict(&listed, &config, &both_tables());

    // Seat-billed either way; only the priced model carries an equivalent.
    assert_eq!(miss, RowCost::Subscription(None));
    assert_eq!(hit, RowCost::Subscription(Some(REFRESHED_USD)));
}

#[test]
fn a_known_zero_price_keeps_the_equivalent_complete() {
    // `free-model` is listed with input, output and cache read at a known 0.
    // A row that used all three dimensions is complete: its equivalent is a
    // measured $0.00, not absent. The control drops the cache-read price, so
    // the same row has an unpriced used dimension and reads absent.
    let config = table_config("oauth://anthropic");
    let (_dir, _path, db) = temp_db();
    insert_table_row(
        &db,
        "r",
        TABLE_PROVIDER,
        "free-model",
        Some("anthropic-api"),
        Some(500),
    );
    let without_cache_read = tables(
        Some(serde_json::json!([{ "id": "free-model", "input": 0.0, "output": 0.0 }])),
        None,
    );

    let complete = table_verdict(&db, &config, &tables(Some(refreshed_rows()), None));
    let incomplete = table_verdict(&db, &config, &without_cache_read);

    assert_eq!(complete, RowCost::Subscription(Some(0.0)));
    assert_eq!(incomplete, RowCost::Subscription(None));
}

/// `[models.sonnet.pricing]` setting only `input` and `output`, so a
/// cache-read price stays unset.
fn model_pricing(config: &mut Config, input: Option<f64>, output: Option<f64>) {
    config.models.get_mut(TABLE_MODEL).unwrap().pricing = Some(PricingConfig {
        input_per_mtok: input,
        output_per_mtok: output,
        ..Default::default()
    });
}

/// The priced arm is complete-or-absent unless a whole `[registry]` row
/// prices it: a row that read from cache with no cache-read price is
/// unpriced, never charged $0 for those reads, whether the rates came from a
/// table, from `[models.X.pricing]`, or from both. The same rows with no
/// cache reads are priced. A `[registry]` row that sets no cache-read price
/// keeps counting the reads at $0, as written.
#[test]
fn a_row_with_a_used_unpriced_dimension_is_unpriced_unless_registry_prices_it() {
    struct Row {
        name: &'static str,
        configure: fn(&mut Config),
        prices: fn() -> PriceSnapshot,
        cache_read: Option<i64>,
        expected: RowCost,
    }
    let shipped_only = || tables(None, Some(shipped_rows()));
    let rows = [
        Row {
            name: "shipped row, cache reads, no cache-read price",
            configure: |_| {},
            prices: shipped_only,
            cache_read: Some(500),
            expected: RowCost::Unpriced,
        },
        Row {
            name: "shipped row, no cache reads",
            configure: |_| {},
            prices: shipped_only,
            cache_read: None,
            expected: RowCost::Priced(SHIPPED_USD),
        },
        Row {
            name: "model-config only, cache reads, no cache-read price",
            configure: |c| model_pricing(c, Some(4.0), Some(6.0)),
            prices: no_prices,
            cache_read: Some(500),
            expected: RowCost::Unpriced,
        },
        Row {
            name: "model-config only, no cache reads",
            configure: |c| model_pricing(c, Some(4.0), Some(6.0)),
            prices: no_prices,
            cache_read: None,
            expected: RowCost::Priced(10.0),
        },
        Row {
            name: "model-config input plus shipped output, cache reads",
            configure: |c| model_pricing(c, Some(4.0), None),
            prices: shipped_only,
            cache_read: Some(500),
            expected: RowCost::Unpriced,
        },
        Row {
            name: "model-config input plus shipped output, no cache reads",
            configure: |c| model_pricing(c, Some(4.0), None),
            prices: shipped_only,
            cache_read: None,
            expected: RowCost::Priced(6.0),
        },
        Row {
            name: "registry row, cache reads, no cache-read price",
            configure: |c| {
                c.registry
                    .insert(TABLE_UPSTREAM.to_string(), registry_row());
            },
            prices: shipped_only,
            cache_read: Some(500),
            expected: RowCost::Priced(2.0),
        },
    ];

    for row in rows {
        let mut config = table_config("env://ANTH_KEY");
        (row.configure)(&mut config);
        let (_dir, _path, db) = temp_db();
        insert_table_row(
            &db,
            "r",
            TABLE_PROVIDER,
            TABLE_UPSTREAM,
            Some("anthropic-api"),
            row.cache_read,
        );

        // Act
        let verdict = table_verdict(&db, &config, &(row.prices)());

        // Assert
        assert_eq!(verdict, row.expected, "{}", row.name);
    }
}

#[test]
fn a_removed_provider_prices_a_first_party_upstream_by_its_persisted_kind() {
    // The row's provider entry is gone from config, so only the kind recorded
    // with the row can select the `anthropic` table provider. A kindless row
    // selects nothing and fails closed.
    let config = Config::default();
    let (_dir, kinded) = seeded(TABLE_UPSTREAM, Some("anthropic-api"));
    let (_dir2, kindless) = seeded(TABLE_UPSTREAM, None);

    let with_kind = table_verdict(&kinded, &config, &both_tables());
    let without_kind = table_verdict(&kindless, &config, &both_tables());

    assert_eq!(with_kind, RowCost::Priced(REFRESHED_USD));
    assert_eq!(without_kind, RowCost::Unpriced);
}

#[test]
fn a_mixed_window_reports_the_priced_subtotal_only() {
    // Three rows in ONE display group: table-priced, subscription, and an
    // unpriced upstream. The group surfaces all three markers and a subtotal
    // covering the priced row alone.
    let (_dir, _path, db) = temp_db();
    let mut config = table_config("env://ANTH_KEY");
    config.providers.insert(
        "sub".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    insert_table_row(
        &db,
        "priced",
        TABLE_PROVIDER,
        TABLE_UPSTREAM,
        Some("anthropic-api"),
        None,
    );
    insert_table_row(
        &db,
        "unlisted",
        TABLE_PROVIDER,
        UNLISTED_UPSTREAM,
        Some("anthropic-api"),
        None,
    );
    insert_table_row(
        &db,
        "sub-row",
        "sub",
        TABLE_UPSTREAM,
        Some("anthropic-api"),
        None,
    );

    let report = build_window_report(
        &db,
        &config,
        &both_tables(),
        "t".into(),
        window_bounds(WindowFlag::All, fixed_now()),
        Some(GroupDim::Alias),
        false,
    )
    .expect("window report");
    let total = find(&report, "total");

    assert_eq!(total.priced_total_usd, Some(REFRESHED_USD));
    assert_eq!(total.equivalent_total_usd, Some(REFRESHED_USD));
    assert!(total.any_subscription);
    assert!(total.any_unpriced);
}
