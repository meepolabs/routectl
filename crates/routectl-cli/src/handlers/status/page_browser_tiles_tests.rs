// The Now KPI tiles in a real headless Chrome: a bucket with no reported
// value draws as a gap, a reported 0 draws on the baseline; the cost tile
// keeps spend and the API-price equivalent apart and qualifies a partial
// cost; an empty ledger names the base URL.
// `include!`d into a child module of `page::tests` beside `browser`; the
// scenarios live in page_browser_tiles_driver.js, which serves a synthetic
// six-bucket series one bucket width apart.

use serde_json::Value;

const TILES_DRIVER: &str = include_str!("page_browser_tiles_driver.js");

const TILES_BUDGET_MS: u64 = 60_000;

/// The sparkline viewBox: 120 wide, 24 tall, the lowest value drawn 2 above
/// the bottom edge.
const SPARK_WIDTH: f64 = 120.0;
const SPARK_BASELINE_Y: f64 = 22.0;

/// Six buckets across the full width put bucket `i` at `i * 24`.
const BUCKET_STEP_X: f64 = SPARK_WIDTH / 5.0;

fn run(name: &'static str) -> Vec<Value> {
    let scenario = browser::Scenario {
        name,
        hash: "now",
        snippets: &[TILES_DRIVER],
        budget_ms: TILES_BUDGET_MS,
    };
    browser::run_scenario(&scenario)
        .result
        .as_array()
        .expect("one observation per round")
        .clone()
}

/// The polylines of one series as `[[x, y], ...]` runs.
fn runs(spark: &Value, series: &str) -> Vec<Vec<(f64, f64)>> {
    spark[series]
        .as_array()
        .unwrap_or_else(|| panic!("series {series} in {spark:#}"))
        .iter()
        .map(|run| {
            run.as_array()
                .expect("a polyline's points")
                .iter()
                .map(|p| (p[0].as_f64().expect("x"), p[1].as_f64().expect("y")))
                .collect()
        })
        .collect()
}

fn xs(run: &[(f64, f64)]) -> Vec<f64> {
    run.iter().map(|p| p.0).collect()
}

fn bucket_xs(buckets: &[usize]) -> Vec<f64> {
    buckets.iter().map(|&i| i as f64 * BUCKET_STEP_X).collect()
}

/// One null TTFT bucket splits the line into the runs either side of it and
/// fills only under those runs, and stays out of the vertical scale, while the
/// same grid with every bucket present draws one line.
///
/// Mutation checks: bucketSamples coercing each value through num0 -> red (the
/// null bucket draws as 0 and the line stays one run); drawSegments plotting
/// an absent sample -> red; sparkSvg scaling over absent samples -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_null_ttft_bucket_breaks_the_sparkline() {
    let rounds = run("tiles-ttft-null-bucket");
    let (present, with_null) = (&rounds[0], &rounds[1]);

    let whole = runs(present, "a");
    assert_eq!(whole.len(), 1, "{present:#}");
    assert_eq!(xs(&whole[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{present:#}");

    let split = runs(with_null, "a");
    assert_eq!(split.len(), 2, "{with_null:#}");
    assert_eq!(xs(&split[0]), bucket_xs(&[0, 1]), "{with_null:#}");
    assert_eq!(xs(&split[1]), bucket_xs(&[3, 4, 5]), "{with_null:#}");
    assert_eq!(with_null["polygons"], 2, "{with_null:#}");
    assert_eq!(
        split[0][0].1, SPARK_BASELINE_Y,
        "the null is outside the scale, so the lowest present value is the baseline: {with_null:#}"
    );
}

/// A bucket that reported 0 is a measurement: it sits on the baseline inside
/// one unbroken line.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_zero_ttft_bucket_sits_on_the_baseline() {
    let rounds = run("tiles-ttft-zero-bucket");
    let spark = &rounds[0];

    let line = runs(spark, "a");
    assert_eq!(line.len(), 1, "{spark:#}");
    assert_eq!(xs(&line[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{spark:#}");
    assert_eq!(line[0][2].1, SPARK_BASELINE_Y, "{spark:#}");
    assert!(
        line[0]
            .iter()
            .enumerate()
            .all(|(i, p)| i == 2 || p.1 < SPARK_BASELINE_Y),
        "only the zero bucket is on the baseline: {spark:#}"
    );
}

/// A bucket with no cache write reported draws no write point, beside a read
/// line that stays whole; with no bucket reporting a write there is no write
/// line at all.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_unreported_cache_write_buckets_draw_no_write_points() {
    let rounds = run("tiles-cache-write-absent");
    let (partial, none) = (&rounds[0], &rounds[1]);

    let read = runs(partial, "a");
    assert_eq!(read.len(), 1, "{partial:#}");
    assert_eq!(xs(&read[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{partial:#}");
    let write = runs(partial, "b");
    assert_eq!(write.len(), 2, "{partial:#}");
    assert_eq!(xs(&write[0]), bucket_xs(&[0, 1]), "{partial:#}");
    assert_eq!(xs(&write[1]), bucket_xs(&[4, 5]), "{partial:#}");

    assert_eq!(runs(none, "a").len(), 1, "{none:#}");
    assert!(runs(none, "b").is_empty(), "{none:#}");
}

/// The fixture's configured listener, as the config panel reports it.
const FIXTURE_BASE_URL: &str = "http://127.0.0.1:8787";

const PARTIAL_QUALIFIER: &str = "priced subtotal only";

fn text<'a>(obs: &'a Value, key: &str) -> Option<&'a str> {
    obs[key].as_str()
}

/// Spend and the API-price equivalent are two figures: the spend in the tile
/// value, the "~$" equivalent labelled "At API prices" with its one-line
/// explanation beside it. With no spend the tile value never carries the
/// equivalent, and with no equivalent nothing labelled "At API prices" is
/// drawn.
///
/// Mutation checks: apiPriceBlock dropped from costTile -> red (round 0 has no
/// "At API prices"); costFigure handed `cost_usd + equivalent_cost_usd` ->
/// red (round 0 value is $52.75); costFigure reading `equivalent_cost_usd`
/// when `cost_usd` is null -> red (round 1 value carries 40.25).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_cost_tile_keeps_spend_and_api_price_apart() {
    let rounds = run("tiles-cost-equivalent");
    let (both, no_spend, no_equiv) = (&rounds[0], &rounds[1], &rounds[2]);

    assert_eq!(text(both, "value"), Some("$12.50"), "{both:#}");
    assert_eq!(text(both, "alt_label"), Some("At API prices"), "{both:#}");
    assert_eq!(text(both, "alt_value"), Some("~$40.25"), "{both:#}");
    assert_eq!(
        text(both, "alt_note"),
        Some("what this traffic would cost on pay-as-you-go API pricing"),
        "{both:#}"
    );

    let spend = text(no_spend, "value").expect("a value");
    assert!(
        !spend.contains('$') && !spend.contains("40.25"),
        "{no_spend:#}"
    );
    assert_eq!(text(no_spend, "alt_value"), Some("~$40.25"), "{no_spend:#}");

    assert_eq!(text(no_equiv, "value"), Some("$12.50"), "{no_equiv:#}");
    assert!(no_equiv["alt_label"].is_null(), "{no_equiv:#}");
    assert!(no_equiv["alt_value"].is_null(), "{no_equiv:#}");
}

/// The tile's "At API prices" figure says when it leaves some subscription
/// traffic out, as the server's `equivalent_status` says: a `partial`
/// equivalent carries the `partial` mark and its title, a `complete` one
/// carries none (also when the window lists a missing price, which belongs
/// to another kind of row), and a subscription window with requests but no
/// equivalent reads the faint word `unpriced` rather than drawing nothing.
///
/// Mutation checks: the mark appended in apiPriceBlock removed -> red (round
/// 0); equivalentCoverage ignoring `equivalent_status` -> red (round 0
/// unmarked); equivalentCoverage reading a listed missing price as partial
/// -> red (round 3 marked); apiPriceBlock returning null with no equivalent
/// -> red (round 2).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_an_equivalent_that_leaves_rows_out_says_so() {
    let rounds = run("tiles-equivalent-coverage");
    let (partial, full, unpriced, complete_beside_gap) =
        (&rounds[0], &rounds[1], &rounds[2], &rounds[3]);

    assert_eq!(
        text(partial, "alt_value"),
        Some("~$617.54partial"),
        "{partial:#}"
    );
    assert_eq!(text(partial, "alt_mark"), Some("partial"), "{partial:#}");
    assert_eq!(
        text(partial, "alt_mark_title"),
        Some("priced subtotal only: leaves out usage that has no price"),
        "{partial:#}"
    );

    assert_eq!(text(full, "alt_value"), Some("~$40.25"), "{full:#}");
    assert!(full["alt_mark"].is_null(), "{full:#}");

    assert!(
        unpriced["requests"].as_u64() > Some(0),
        "the window carries requests: {unpriced:#}"
    );
    assert_eq!(
        text(unpriced, "alt_label"),
        Some("At API prices"),
        "{unpriced:#}"
    );
    assert_eq!(
        text(unpriced, "alt_value"),
        Some("unpriced"),
        "{unpriced:#}"
    );
    assert_eq!(unpriced["alt_faint"], true, "{unpriced:#}");
    assert!(unpriced["alt_mark"].is_null(), "{unpriced:#}");

    assert_eq!(
        text(complete_beside_gap, "alt_value"),
        Some("~$40.25"),
        "{complete_beside_gap:#}"
    );
    assert!(
        complete_beside_gap["alt_mark"].is_null(),
        "{complete_beside_gap:#}"
    );
}

/// A partial window qualifies its cost on the tile and on every provider card
/// whose own cost is partial (the all-providers card included, since it reads
/// the partial totals), and on no other card; the cost sparkline is
/// captioned directly above it. A priced window carries neither.
///
/// Mutation checks: the provider card qualifier branch removed -> red; the
/// tile's partial note reworded -> red; costSparkCaption not appended -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_partial_cost_is_qualified_on_the_tile_and_its_cards() {
    let rounds = run("tiles-cost-partial");
    let (partial, priced) = (&rounds[0], &rounds[1]);

    let sub = text(partial, "sub").expect("a sub line");
    assert!(sub.contains(PARTIAL_QUALIFIER), "{partial:#}");
    assert!(
        text(partial, "caption").is_some_and(|c| c.contains("each bucket priced on its own")),
        "{partial:#}"
    );
    assert_eq!(partial["caption_before_spark"], true, "{partial:#}");

    let cards = partial["cards"].as_array().expect("cards");
    assert!(
        cards.iter().all(|c| c["status"] != "unmatched"),
        "every card resolves to its group: {partial:#}"
    );
    let partial_cards: Vec<&Value> = cards.iter().filter(|c| c["status"] == "partial").collect();
    let other_cards: Vec<&Value> = cards.iter().filter(|c| c["status"] != "partial").collect();
    assert!(
        !partial_cards.is_empty(),
        "a partial card is drawn: {partial:#}"
    );
    assert!(
        other_cards.iter().any(|c| c["status"] == "priced"),
        "a priced card is drawn as the control: {partial:#}"
    );
    for card in &partial_cards {
        assert_eq!(text(card, "qual"), Some(PARTIAL_QUALIFIER), "{card:#}");
    }
    for card in &other_cards {
        assert!(card["qual"].is_null(), "{card:#}");
    }

    assert!(
        !text(priced, "sub")
            .expect("a sub line")
            .contains(PARTIAL_QUALIFIER),
        "{priced:#}"
    );
    assert!(priced["caption"].is_null(), "{priced:#}");
    assert!(
        priced["cards"]
            .as_array()
            .expect("cards")
            .iter()
            .all(|c| c["qual"].is_null()),
        "{priced:#}"
    );
}

/// With nothing ever routed (the series answer's `earliest_ms` null), Now
/// says "No requests yet" and names the configured listener as the base URL,
/// in a focusable code element.
///
/// Mutation checks: ledgerIsEmpty returning false -> red (the window's empty
/// state is drawn); clientBaseAddress reading another field -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_an_empty_ledger_names_the_base_url() {
    let obs = &browser::run_scenario(&browser::Scenario {
        name: "tiles-first-run",
        hash: "now",
        snippets: &[TILES_DRIVER],
        budget_ms: TILES_BUDGET_MS,
    })
    .result;

    assert_eq!(text(obs, "title"), Some("No requests yet"), "{obs:#}");
    assert_eq!(text(obs, "base_url"), Some(FIXTURE_BASE_URL), "{obs:#}");
    assert_eq!(obs["base_url_focusable"], true, "{obs:#}");
}

/// The control: a window with no traffic over a ledger that has some (the
/// series answer's `earliest_ms` set) keeps the window's own empty state and
/// names no base URL.
///
/// Mutation check: ledgerIsEmpty returning true -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_an_empty_window_over_a_used_ledger_is_not_first_run() {
    let obs = &browser::run_scenario(&browser::Scenario {
        name: "tiles-empty-window",
        hash: "now",
        snippets: &[TILES_DRIVER],
        budget_ms: TILES_BUDGET_MS,
    })
    .result;

    assert_eq!(
        text(obs, "title"),
        Some("No requests in this window"),
        "{obs:#}"
    );
    assert!(obs["base_url"].is_null(), "{obs:#}");
}

/// The cache-hit row table the CLI's `cache_hit_text` is pinned by.
const CACHE_HIT_ROWS: &str = include_str!("../../../tests/fixtures/cache_hit_rows.json");

/// Virtual time for one series answer per table row at the 5 s cadence.
const CACHE_HIT_BUDGET_MS: u64 = 180_000;

/// Every row of the shared table, served as the window's cache-hit share,
/// reads on the Now tile as the row's text: the tile and the CLI print one
/// rule.
///
/// Mutation checks: cacheHitTile back on `hit.toFixed(1)` -> red (99.96 reads
/// "100.0%"); cacheHitText rounding above 99 instead of flooring -> red;
/// cacheHitText keeping a decimal at exactly 100 -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_cache_hit_tile_shows_each_row_of_the_shared_table() {
    let rows: Vec<Value> = serde_json::from_str(CACHE_HIT_ROWS).expect("cache hit rows parse");
    let set_rows = format!("window.routectlCacheHitRows = {CACHE_HIT_ROWS};");
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(set_rows.into_boxed_str()), TILES_DRIVER].into_boxed_slice());

    let shown = browser::run_scenario(&browser::Scenario {
        name: "tiles-cache-hit-rows",
        hash: "now",
        snippets,
        budget_ms: CACHE_HIT_BUDGET_MS,
    })
    .result;
    let shown = shown.as_array().expect("one reading per row");

    assert_eq!(shown.len(), rows.len(), "{shown:#?}");
    for (i, (row, reading)) in rows.iter().zip(shown).enumerate() {
        assert_eq!(reading["row"], i, "served in table order: {reading:#}");
        assert_eq!(reading["text"], row["text"], "cache_hit_pct {}", row["pct"]);
    }
}

/// A partial window, and a partial provider card, with no priced row at all
/// (the server sends `cost_usd` null) read the word `partial`, never a
/// dollar figure, and carry no "priced subtotal only" note, since there is
/// no subtotal; the sparkline caption about subtotals is absent too. The
/// priced card beside them keeps its figure.
///
/// Mutation checks: costFigure handing `cost_usd` to money() for any partial
/// set, with money(null) reading '0.00' -> red; the card qualifier keyed on
/// `cost_status` alone -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_partial_cost_with_no_priced_row_is_never_a_figure() {
    let rounds = run("tiles-cost-partial-unpriced");
    let obs = &rounds[0];

    assert_eq!(text(obs, "value"), Some("partial"), "{obs:#}");
    let sub = text(obs, "sub").expect("a sub line");
    assert!(!sub.contains(PARTIAL_QUALIFIER), "{obs:#}");
    assert!(obs["caption"].is_null(), "{obs:#}");
    let cards = obs["cards"].as_array().expect("cards");
    let partial: Vec<&Value> = cards.iter().filter(|c| c["status"] == "partial").collect();
    assert!(!partial.is_empty(), "a partial card is drawn: {obs:#}");
    for card in &partial {
        assert_eq!(text(card, "cost"), Some("partial"), "{card:#}");
        assert!(card["qual"].is_null(), "{card:#}");
    }
    assert!(
        cards
            .iter()
            .any(|c| c["status"] == "priced" && text(c, "cost") == Some("$3.46")),
        "positive control: the priced card keeps its figure: {obs:#}"
    );
}
