// Reasoning-token pricing keys off the kind PERSISTED with each row, not the
// kind the provider carries in config NOW. `include!`d into `usage_tests.rs`,
// so these compile into THAT module and its helpers stay in scope; all imports
// live in the host file -- do not add `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// Derived BY HAND from the rate table and token counts below, NOT from a
// program run. Both surfaces that price a row assemble their SQL from the same
// shared column macros, so an A-vs-B comparison between them cannot show that
// either is right -- only that they agree.
//
// Rates on upstream `up-shift`: input $3.00/Mtok, output $15.00/Mtok. No cache
// dimension is priced and no row reports one.
//
// Both seeded rows carry IDENTICAL tokens -- 1M input, 1M output, 2M reasoning
// -- and differ ONLY in persisted `provider_kind`, so any difference in their
// cost is attributable to the kind alone:
//
//   gemini era (reasoning DISJOINT, billed at the output rate)
//     input      1_000_000 * 3.00 / 1e6 =  $3.00
//     output     1_000_000 * 15.00 / 1e6 = $15.00
//     reasoning  2_000_000 * 15.00 / 1e6 = $30.00
//                                          ------
//                                          $48.00
//
//   openai-compat era (reasoning SUBSUMED in the output count, not re-charged)
//     input      1_000_000 * 3.00 / 1e6 =  $3.00
//     output     1_000_000 * 15.00 / 1e6 = $15.00
//     reasoning                            $ 0.00
//                                          ------
//                                          $18.00
//
//   era-correct window total             = $66.00
//
// The two figures a CURRENT-CONFIG lookup produces instead, each of which
// prices BOTH rows by one kind, are asserted against explicitly below:
//
//   both-as-gemini        2 * $48.00 = $96.00   (overcounts the non-gemini era)
//   both-as-openai-compat 2 * $18.00 = $36.00   (undercounts the gemini era)

/// Era-correct window total, per the derivation above.
const EXPECTED_ERA_CORRECT_USD: f64 = 66.00;

/// What pricing every row as gemini yields -- the historical OVERCOUNT.
const BOTH_ERAS_AS_GEMINI_USD: f64 = 96.00;

/// What pricing every row as a subsumed-reasoning kind yields -- the
/// historical UNDERCOUNT.
const BOTH_ERAS_AS_SUBSUMED_USD: f64 = 36.00;

/// The provider name both eras were served under. The defect this pins needs
/// ONE name spanning two kinds, which is exactly what re-kinding an entry in
/// place produces.
const SHIFT_PROVIDER: &str = "shift";

const SHIFT_UPSTREAM: &str = "up-shift";

/// Insert a row with an explicit persisted `provider_kind` and reasoning-token
/// count under the shared `(model, provider, upstream, alias)` key. `kind` is
/// `Option` so the unknown-kind path is reachable with a genuine SQL NULL, and
/// `reasoning` is `Option` to separate a reported zero from an absent count.
fn insert_era_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    kind: Option<&str>,
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, input_tokens, output_tokens, reasoning_tokens) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', ?3, ?4, ?5, \
             0, 'ok', 10, 0, 0, 1, 0, ?6, ?7, ?8)",
            rusqlite::params![
                ts_start,
                request_id,
                SHIFT_PROVIDER,
                SHIFT_UPSTREAM,
                kind,
                input,
                output,
                reasoning,
            ],
        )
        .expect("insert era row");
}

/// Seed the two-era ledger the oracle above is computed from: identical tokens,
/// different persisted kinds, one provider name.
fn seed_two_eras(db: &UsageDb) {
    insert_era_row(
        db,
        "gemini-era",
        1_000,
        Some("gemini"),
        Some(1_000_000),
        Some(1_000_000),
        Some(2_000_000),
    );
    insert_era_row(
        db,
        "subsumed-era",
        2_000,
        Some("openai-compat"),
        Some(1_000_000),
        Some(1_000_000),
        Some(2_000_000),
    );
}

/// A config whose `shift` provider carries `kind`, priced on `up-shift`. The
/// kind is a parameter precisely because the assertions must hold no matter
/// which one the operator has configured at report time.
fn era_config(shift_entry: ProviderEntry) -> Config {
    let mut config = Config::default();
    config
        .providers
        .insert(SHIFT_PROVIDER.to_string(), shift_entry);
    config.registry.insert(
        SHIFT_UPSTREAM.to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(15.0),
                ..Default::default()
            }),
            provider: None,
        },
    );
    config
}

/// Config with `shift` configured as gemini today.
fn config_shift_now_gemini() -> Config {
    era_config(ProviderEntry::gemini("env://SHIFT_KEY"))
}

/// Config with `shift` configured as a subsumed-reasoning kind today.
fn config_shift_now_subsumed() -> Config {
    era_config(ProviderEntry::openai_compat(
        "https://example.invalid",
        "env://SHIFT_KEY",
    ))
}

fn era_report(db: &UsageDb, config: &Config) -> WindowReport {
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
fn aggregate_partitions_one_provider_name_by_persisted_kind() {
    // Arrange: two rows identical but for their persisted kind.
    let (_dir, _path, db) = temp_db();
    seed_two_eras(&db);

    // Act
    let rows = aggregate(&db, 0, i64::MAX).expect("aggregate");

    // Assert: the read grain separates the eras, which is what lets each be
    // priced by what it was. Without the split there is no era to price.
    assert_eq!(rows.len(), 2, "each persisted kind is its own partition");
    let mut kinds: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r.key.provider_kind.as_deref())
        .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec![Some("gemini"), Some("openai-compat")]);
}

#[test]
fn mixed_era_total_matches_the_hand_computed_oracle() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    seed_two_eras(&db);

    // Act
    let report = era_report(&db, &config_shift_now_gemini());
    let row = find(&report, SHIFT_PROVIDER);

    // Assert: the era-correct sum, and explicitly NEITHER single-kind figure a
    // current-config lookup would produce.
    assert_eq!(
        row.priced_total_usd,
        Some(EXPECTED_ERA_CORRECT_USD),
        "each era must price by its own persisted kind"
    );
    assert_ne!(
        row.priced_total_usd,
        Some(BOTH_ERAS_AS_GEMINI_USD),
        "pricing the subsumed era as gemini double-charges its reasoning"
    );
    assert_ne!(
        row.priced_total_usd,
        Some(BOTH_ERAS_AS_SUBSUMED_USD),
        "pricing the gemini era as subsumed drops its disjoint reasoning"
    );
}

#[test]
fn historical_totals_do_not_move_when_the_provider_kind_is_reconfigured() {
    // Arrange: ONE ledger, read under two configs that differ ONLY in the
    // kind `shift` carries now -- the operator edit that produced the defect.
    let (_dir, _path, db) = temp_db();
    seed_two_eras(&db);

    // Act
    let as_gemini = era_report(&db, &config_shift_now_gemini());
    let as_subsumed = era_report(&db, &config_shift_now_subsumed());

    // Assert: both agree with the oracle, so re-kinding a provider cannot
    // retroactively reprice history.
    assert_eq!(
        find(&as_gemini, SHIFT_PROVIDER).priced_total_usd,
        Some(EXPECTED_ERA_CORRECT_USD),
        "config kind gemini"
    );
    assert_eq!(
        find(&as_subsumed, SHIFT_PROVIDER).priced_total_usd,
        Some(EXPECTED_ERA_CORRECT_USD),
        "config kind openai-compat"
    );
}

#[test]
fn one_display_group_is_reported_for_a_two_era_provider() {
    // The finer read grain must not leak into the report as two same-label
    // rows: the eras are priced apart and reported together.
    let (_dir, _path, db) = temp_db();
    seed_two_eras(&db);

    let report = era_report(&db, &config_shift_now_gemini());

    let labelled: Vec<&DisplayRow> = report
        .rows
        .iter()
        .filter(|r| r.label == SHIFT_PROVIDER)
        .collect();
    assert_eq!(labelled.len(), 1, "one group per provider label");
    // The single group carries BOTH eras' tokens.
    assert_eq!(labelled[0].requests, 2);
    assert_eq!(labelled[0].reasoning_tokens, 4_000_000);
}

#[test]
fn unknown_persisted_kind_with_reasoning_tokens_is_unpriced_not_guessed() {
    // Arrange: a row with a NULL persisted kind that DID report reasoning
    // tokens. Neither structure can be justified for it, so it must fail
    // closed rather than borrow an answer from current config.
    let (_dir, _path, db) = temp_db();
    insert_era_row(
        &db,
        "kindless-with-reasoning",
        1_000,
        None,
        Some(1_000_000),
        Some(1_000_000),
        Some(2_000_000),
    );

    // Act
    let report = era_report(&db, &config_shift_now_gemini());
    let row = find(&report, SHIFT_PROVIDER);

    // Assert: no dollar figure at all, and the group says so.
    assert!(
        row.any_unpriced,
        "an unknown-kind reasoning row must report as unpriced"
    );
    assert_eq!(
        row.priced_total_usd, None,
        "no cost may be asserted for a row whose reasoning structure is unknown"
    );
}

#[test]
fn unknown_persisted_kind_without_reasoning_tokens_still_prices() {
    // A kindless row that reported NO reasoning tokens is fully determined:
    // the kind cannot change what its input and output dimensions cost, so
    // failing it closed would throw away a correct figure.
    let (_dir, _path, db) = temp_db();
    insert_era_row(
        &db,
        "kindless-no-reasoning",
        1_000,
        None,
        Some(1_000_000),
        Some(1_000_000),
        None,
    );

    let report = era_report(&db, &config_shift_now_gemini());
    let row = find(&report, SHIFT_PROVIDER);

    // input $3.00 + output $15.00, reasoning absent.
    assert_eq!(row.priced_total_usd, Some(18.00));
    assert!(!row.any_unpriced);
}

#[test]
fn a_reported_zero_reasoning_count_on_an_unknown_kind_still_prices() {
    // `reasoning_tokens = 0` reported explicitly is as determined as an absent
    // count: there are no reasoning tokens to attribute either way.
    let (_dir, _path, db) = temp_db();
    insert_era_row(
        &db,
        "kindless-zero-reasoning",
        1_000,
        None,
        Some(1_000_000),
        Some(1_000_000),
        Some(0),
    );

    let report = era_report(&db, &config_shift_now_gemini());
    let row = find(&report, SHIFT_PROVIDER);

    assert_eq!(row.priced_total_usd, Some(18.00));
    assert!(!row.any_unpriced);
}

#[test]
fn ttft_samples_from_both_eras_land_in_one_display_group() {
    // TTFB rows carry the same widened key as the aggregate, so a partitioned
    // read must still attach every era's samples to the one reported group.
    let (_dir, _path, db) = temp_db();
    for (id, kind, ttfb) in [
        ("g-fast", "gemini", 10),
        ("g-slow", "gemini", 20),
        ("s-fast", "openai-compat", 30),
        ("s-slow", "openai-compat", 40),
    ] {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, provider_kind, \
                 stream, outcome, latency_ms, ttfb_ms, tool_count, msg_count, \
                 attempt_count, fallback_count) \
                 VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', ?3, ?4, ?5, \
                 1, 'ok', 500, ?6, 0, 0, 1, 0)",
                rusqlite::params![
                    1_000 + i64::from(ttfb),
                    id,
                    SHIFT_PROVIDER,
                    SHIFT_UPSTREAM,
                    kind,
                    ttfb,
                ],
            )
            .expect("insert ttfb row");
    }

    let report = build_window_report(
        &db,
        &config_shift_now_gemini(),
        &no_overlay(),
        "t".into(),
        window_bounds(WindowFlag::All, fixed_now()),
        Some(GroupDim::Provider),
        true,
    )
    .expect("window report");
    let row = find(&report, SHIFT_PROVIDER);

    // Nearest-rank over the four pooled samples [10, 20, 30, 40]:
    // p50 -> rank ceil(0.5*4) = 2 -> 20; p95 -> rank ceil(0.95*4) = 4 -> 40.
    assert_eq!(row.ttft_p50_ms, Some(20), "p50 pools both eras' samples");
    assert_eq!(row.ttft_p95_ms, Some(40), "p95 pools both eras' samples");
}

#[test]
fn an_unrecognized_non_null_kind_with_reasoning_tokens_is_unpriced_not_subsumed() {
    // Arrange: a row whose persisted kind is a token this build does not know.
    // The classifier once treated every non-NULL kind it did not recognize as
    // SUBSUMED, which silently dropped reasoning tokens from the bill -- an
    // undercharge with no signal. A kind we have never heard of may report
    // reasoning disjointly, so it must fail closed exactly like a NULL kind.
    let (_dir, _path, db) = temp_db();
    insert_era_row(
        &db,
        "future-disjoint-provider",
        1_000,
        Some("future-disjoint-provider"),
        Some(1_000_000),
        Some(1_000_000),
        Some(2_000_000),
    );

    // Act
    let report = era_report(&db, &config_shift_now_gemini());
    let row = find(&report, SHIFT_PROVIDER);

    // Assert: unpriced, and specifically NOT the $18.00 a subsumed reading
    // would have produced (3 + 15 + 0), nor the $48.00 a disjoint one would.
    assert!(
        row.any_unpriced,
        "an unrecognized kind carrying reasoning tokens must report as unpriced"
    );
    assert_eq!(
        row.priced_total_usd, None,
        "no cost may be asserted for an unrecognized reasoning structure"
    );
}

#[test]
fn every_provider_kind_token_is_classified_disjoint_or_subsumed() {
    // The two lists together must cover every `ProviderEntry::kind_str` token,
    // or a kind routectl genuinely supports would price as `Unpriced` forever
    // -- the safe direction, but still wrong. This pins the coverage so adding
    // a provider kind upstream cannot silently skip the pricing decision.
    //
    // The expected set is written out literally rather than derived, so a NEW
    // kind fails this test and forces someone to make the disjoint-or-subsumed
    // call deliberately.
    for kind in [
        "openai-compat",
        "anthropic-api",
        "bedrock",
        "openai-responses",
        "gemini",
    ] {
        assert!(
            matches!(
                reasoning_structure(Some(kind)),
                ReasoningStructure::Disjoint | ReasoningStructure::Subsumed
            ),
            "provider kind {kind} has no reasoning-structure classification; \
             add it to DISJOINT_REASONING_KIND or SUBSUMED_REASONING_KINDS"
        );
    }
}
