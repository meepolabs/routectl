// Baked-catalog auto-fill: a model the operator has NOT priced in `[registry]`
// still gets a dollar figure from the catalog's own rates. `include!`d into
// `usage_tests.rs`, so these compile into THAT module and its helpers stay in
// scope; all imports live in the host file -- do not add `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// The catalog bakes `anthropic-api` `claude-sonnet-4-6` at $3.00 per million
// input tokens and $15.00 per million output tokens. Every seeded row below
// carries 1M input + 1M output tokens, so:
//
//   input   1_000_000 * 3.00  / 1e6 =  $3.00
//   output  1_000_000 * 15.00 / 1e6 = $15.00
//                                     ------
//                                     $18.00
//
// The figure is identical to what the `[registry]`-priced fixtures elsewhere in
// this module assert, which is the point: the fill changes WHERE the rates come
// from, never how a priced row is costed.

/// The catalog-priced total for one seeded row, per the derivation above.
const CATALOG_FILL_USD: f64 = 18.00;

/// The upstream id the baked `anthropic-api` cell matches. Deliberately a real
/// model id: the fill resolves through the catalog's glob matcher, so a
/// placeholder id would match no cell and the test would pass for the wrong
/// reason.
const CATALOG_UPSTREAM: &str = "claude-sonnet-4-6";

/// A provider name with NO `[registry]` row anywhere in the fixture config, so
/// every lookup against it falls through to the catalog layer.
const UNPRICED_PROVIDER: &str = "unpriced";

/// Config carrying one API-key `anthropic-api` provider and NO `[registry]`
/// pricing at all -- the out-of-the-box state the fill exists for.
fn catalog_fill_config() -> Config {
    let mut config = Config::default();
    config.providers.insert(
        UNPRICED_PROVIDER.to_string(),
        ProviderEntry::anthropic_api("env://UNPRICED_KEY"),
    );
    config
}

/// Insert a row with an explicit persisted `provider_kind` under the
/// catalog-matching upstream, carrying 1M input + 1M output tokens.
fn insert_catalog_row(db: &UsageDb, request_id: &str, ts_start: i64, kind: Option<&str>) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', ?3, ?4, ?5, \
             0, 'ok', 10, 0, 0, 1, 0, 1000000, 1000000)",
            rusqlite::params![
                ts_start,
                request_id,
                UNPRICED_PROVIDER,
                CATALOG_UPSTREAM,
                kind,
            ],
        )
        .expect("insert catalog-priced row");
}

fn catalog_fill_report(db: &UsageDb, config: &Config) -> WindowReport {
    build_window_report(
        db,
        config,
        &no_overlay(),
        "t".into(),
        window_bounds(WindowFlag::All, fixed_now()),
        Some(GroupDim::Provider),
        false,
    )
    .expect("window report")
}

#[test]
fn an_unpriced_registry_model_is_costed_from_the_baked_catalog() {
    // Arrange: no [registry] row at all; the row's persisted kind matches a
    // baked cell that prices both dimensions.
    let (_dir, _path, db) = temp_db();
    insert_catalog_row(&db, "catalog-priced", 1_000, Some("anthropic-api"));

    // Act
    let report = catalog_fill_report(&db, &catalog_fill_config());
    let row = find(&report, UNPRICED_PROVIDER);

    // Assert
    assert_eq!(row.priced_total_usd, Some(CATALOG_FILL_USD));
    assert!(!row.any_unpriced, "the baked rates fully price this row");
    assert!(!row.any_subscription);
}

#[test]
fn an_operator_registry_row_overrides_the_baked_catalog_rates() {
    // Arrange: the SAME ledger, plus a [registry] row at deliberately
    // different rates -- $1 in / $2 out per Mtok = $3.00 for these tokens.
    let (_dir, _path, db) = temp_db();
    insert_catalog_row(&db, "catalog-priced", 1_000, Some("anthropic-api"));
    let mut config = catalog_fill_config();
    config.registry.insert(
        CATALOG_UPSTREAM.to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                ..Default::default()
            }),
            provider: None,
        },
    );

    // Act
    let report = catalog_fill_report(&db, &config);
    let row = find(&report, UNPRICED_PROVIDER);

    // Assert: the operator's rates, and specifically NOT the baked figure.
    assert_eq!(row.priced_total_usd, Some(3.00));
    assert_ne!(
        row.priced_total_usd,
        Some(CATALOG_FILL_USD),
        "an operator [registry] row must win over the baked catalog"
    );
}

#[test]
fn a_row_with_no_persisted_kind_fills_from_no_catalog_cell() {
    // The catalog is keyed on (provider_kind, upstream). A row that never
    // recorded its kind identifies no cell, so it must fail closed to unpriced
    // rather than borrow rates from the kind the provider carries NOW.
    let (_dir, _path, db) = temp_db();
    insert_catalog_row(&db, "kindless", 1_000, None);

    let report = catalog_fill_report(&db, &catalog_fill_config());
    let row = find(&report, UNPRICED_PROVIDER);

    assert!(row.any_unpriced);
    assert_eq!(
        row.priced_total_usd, None,
        "a kindless row must not be priced from current config's kind"
    );
}

#[test]
fn a_subscription_provider_is_never_filled_from_the_catalog() {
    // The subscription check runs FIRST: a managed-OAuth provider is billed by
    // seat, so the catalog fill must not turn its rows into a dollar figure.
    let (_dir, _path, db) = temp_db();
    let mut config = catalog_fill_config();
    config.providers.insert(
        UNPRICED_PROVIDER.to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    insert_catalog_row(&db, "sub-row", 1_000, Some("anthropic-api"));

    let report = catalog_fill_report(&db, &config);
    let row = find(&report, UNPRICED_PROVIDER);

    assert!(row.any_subscription);
    assert_eq!(
        row.priced_total_usd, None,
        "a subscription row carries no per-token dollar cost"
    );
}

#[test]
fn a_mixed_window_reports_the_priced_subtotal_only() {
    // Three rows in ONE display group: catalog-priced, subscription, and
    // kindless-unpriced. The group must surface all three markers and a
    // subtotal covering the priced row alone -- the `partial` shape the query
    // vocabulary names, unchanged by the fill.
    let (_dir, _path, db) = temp_db();
    let mut config = catalog_fill_config();
    config.providers.insert(
        "sub".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );

    insert_catalog_row(&db, "priced", 1_000, Some("anthropic-api"));
    insert_catalog_row(&db, "kindless", 2_000, None);
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens) \
             VALUES (3000, 3000, 'sub-row', 'openai', 'req-model', 'shared', 'm', \
             'sub', ?1, 'anthropic-api', 0, 'ok', 10, 0, 0, 1, 0, 500, 500)",
            rusqlite::params![CATALOG_UPSTREAM],
        )
        .expect("insert subscription row");

    // Act: group by ALIAS so all three rows land in one display group.
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
    let total = find(&report, "total");

    // Assert: the priced subtotal only, with the other two states flagged.
    assert_eq!(total.priced_total_usd, Some(CATALOG_FILL_USD));
    assert!(total.any_subscription);
    assert!(total.any_unpriced);
}
