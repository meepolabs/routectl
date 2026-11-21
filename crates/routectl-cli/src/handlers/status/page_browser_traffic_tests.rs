// The Traffic table in headless Chrome: a dense range fits one screen, a
// sorted table with an opened row keeps both across refreshes, a range change
// is pending before it relabels and keeps the section's height, an opened
// alias row adds up to its group, a missing price shows the server's key, and
// the first load barely shifts. `include!`d into a child module of
// `page::tests` beside `browser`; every browser test is `#[ignore]` and runs
// with `cargo test -p routectl-cli page_browser -- --ignored`.

use serde_json::{Value, json};

const TRAFFIC_DRIVER: &str = include_str!("page_browser_traffic_driver.js");

const TRAFFIC_BUDGET_MS: u64 = 40_000;

/// A window whose page viewport is 1440 x 900: headless Chrome keeps 87 px
/// of the window's height for its own frame (the tests assert the viewport).
const SCREEN: &str = "--window-size=1440,987";

/// A scenario that first sets each `window.<name>` of `vars` to its value.
fn traffic_scenario(
    name: &'static str,
    hash: &'static str,
    vars: &[(&str, Value)],
) -> browser::Scenario {
    let set: String = vars
        .iter()
        .map(|(var, value)| format!("window.{var} = {value};\n"))
        .collect();
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(set.into_boxed_str()), TRAFFIC_DRIVER].into_boxed_slice());
    browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: TRAFFIC_BUDGET_MS,
    }
}

/// The route's own answer to `body` over a ledger seeded with `seeds`,
/// priced and named against `config`, serialized as the page receives it.
fn served_answer(
    config: routectl_router::Config,
    seeds: &[crate::handlers::status::query::fixture::Seed<'_>],
    body: &Value,
) -> Value {
    use crate::handlers::status::panel_fixtures::FIXTURE_NOW_MS;
    use crate::handlers::status::query::fixture::{query_answer, seed_ledger};
    let dir = tempfile::tempdir().expect("a temp dir");
    let db_path = dir.path().join("usage.db");
    seed_ledger(&db_path, seeds, FIXTURE_NOW_MS);
    let panel = query_answer(
        &db_path,
        config,
        body.to_string().as_bytes(),
        FIXTURE_NOW_MS,
    )
    .expect("the route takes the body");
    assert!(panel.unavailable.is_none(), "the fixture read succeeds");
    serde_json::to_value(panel).expect("the query panel serializes")
}

/// Fourteen models, each served `n + 1` times by the API-key provider.
fn dense_answer() -> Value {
    let names: Vec<String> = (1..=14).map(|n| format!("model-{n:02}")).collect();
    let ids: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            (0..=name[6..].parse::<usize>().unwrap())
                .map(|i| format!("{name}-{i}"))
                .collect()
        })
        .collect();
    let seeds: Vec<crate::handlers::status::query::fixture::Seed<'_>> = names
        .iter()
        .zip(&ids)
        .flat_map(|(name, ids)| {
            ids.iter().map(move |id| {
                (
                    id.as_str(),
                    1,
                    "fast",
                    Some(name.as_str()),
                    Some("anthropic"),
                    None,
                    "ok",
                    None,
                )
            })
        })
        .collect();
    served_answer(
        crate::handlers::status::panel_fixtures::fixture_config(),
        &seeds,
        &json!({"window": "today", "group_by": "model"}),
    )
}

/// The shared fixture config plus `top`, a chain whose second step is the
/// alias `mid`, whose expansion also names `top`'s third step.
fn nested_alias_config() -> routectl_router::Config {
    let mut config = crate::handlers::status::panel_fixtures::fixture_config();
    let chain = |steps: &[&str]| {
        routectl_router::AliasValue::Chain(steps.iter().map(|s| (*s).to_owned()).collect())
    };
    config
        .aliases
        .insert("top".into(), chain(&["sonnet", "mid", "glm"]));
    config
        .aliases
        .insert("mid".into(), chain(&["seat-sonnet", "glm"]));
    config
}

/// `top` served on each step, through `mid`, off its chain, and not at all.
const NESTED_SEEDS: [crate::handlers::status::query::fixture::Seed<'static>; 8] = [
    (
        "top-1",
        1,
        "top",
        Some("sonnet"),
        Some("anthropic"),
        None,
        "ok",
        None,
    ),
    (
        "top-2",
        1,
        "top",
        Some("sonnet"),
        Some("anthropic"),
        None,
        "ok",
        None,
    ),
    (
        "top-3",
        2,
        "top",
        Some("seat-sonnet"),
        Some("claude-seat"),
        Some("anthropic#a"),
        "ok",
        None,
    ),
    (
        "top-4",
        2,
        "top",
        Some("glm"),
        Some("compat"),
        None,
        "ok",
        None,
    ),
    (
        "top-5",
        2,
        "top",
        Some("glm"),
        Some("compat"),
        None,
        "ok",
        None,
    ),
    (
        "top-6",
        3,
        "top",
        Some("codex-mini"),
        Some("codex"),
        None,
        "ok",
        None,
    ),
    (
        "top-7",
        3,
        "top",
        None,
        None,
        None,
        "client_disconnect",
        None,
    ),
    (
        "top-8",
        3,
        "top",
        None,
        None,
        None,
        "client_disconnect",
        None,
    ),
];

fn nested_alias_answer() -> Value {
    served_answer(
        nested_alias_config(),
        &NESTED_SEEDS,
        &json!({"window": "today", "group_by": "alias"}),
    )
}

fn group<'a>(answer: &'a Value, label: &str) -> &'a Value {
    answer["data"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["label"] == label)
        .unwrap_or_else(|| panic!("a `{label}` group in {answer:#}"))
}

fn detail_run(hash: &'static str, by: &str, answer: Value, label: &str) -> Value {
    browser::run_scenario(&traffic_scenario(
        "traffic-detail",
        hash,
        &[
            ("routectlTrafficAnswers", json!({ by: answer })),
            ("routectlTrafficRow", json!(label)),
            ("routectlTrafficRowKey", json!(format!("k{label}"))),
        ],
    ))
    .result
}

/// The lines of one detail block (`chain` or `prices`) of a `traffic-detail`
/// run.
fn block_lines<'a>(out: &'a Value, block: &str) -> &'a Vec<Value> {
    out["detail"][block]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("a `{block}` block in {out:#}"))
}

/// Fourteen groups at 1440 x 900 are fourteen rows of one table, every one
/// inside the first screen, under the eight columns in order.
///
/// Mutation check: give `#traffic-groups tbody tr` a 4rem height -> red
/// (rows fall below the fold).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_fourteen_groups_fit_one_screen() {
    let answer = dense_answer();
    assert_eq!(answer["data"]["groups"].as_array().map(Vec::len), Some(14));

    let out = browser::run_scenario_with(
        &traffic_scenario(
            "traffic-dense",
            "traffic",
            &[("routectlTrafficAnswers", json!({ "model": answer }))],
        ),
        &[SCREEN],
    )
    .result;

    assert_eq!(out["inner_width"], 1440, "{out:#}");
    assert_eq!(out["inner_height"], 900, "{out:#}");
    assert_eq!(out["rows"], 14, "{out:#}");
    assert_eq!(out["below_fold"], json!([]), "{out:#}");
    assert_eq!(
        out["headers"],
        json!([
            "Details",
            "group",
            "requests",
            "error rate",
            "TTFT p50",
            "latency p50",
            "cost",
            "At API prices",
            "cache hit"
        ]),
        "{out:#}"
    );
}

/// The table sorted by requests with the `sonnet` row open: across two
/// refreshes whose figures change, the row, its detail and the sort header
/// are the same nodes, the detail stays open, and the order and the address
/// keep the sort.
///
/// Mutation check: make `rowKey` return a fresh key on every call -> red (the
/// opened row's node is replaced).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_sort_and_an_open_row_survive_refreshes() {
    let report = browser::run_scenario(&traffic_scenario("traffic-kept", "traffic", &[]));

    let result = &report.result;
    let mut obs = vec![&result["baseline"]];
    obs.extend(result["rounds"].as_array().expect("rounds"));
    assert_eq!(obs.len(), 3, "{result:#}");
    for o in &obs {
        for flag in [
            "same_row",
            "same_detail",
            "detail_shown",
            "same_sort_button",
        ] {
            assert_eq!(o[flag], true, "`{flag}`: {o:#}");
        }
        assert_eq!(o["expanded"], "true", "{o:#}");
        assert_eq!(o["aria_sort"], "ascending", "{o:#}");
        assert_eq!(
            o["hash"], "#traffic?sort=traffic-groups.requests.asc",
            "{o:#}"
        );
        let requests: Vec<u64> = o["requests"]
            .as_array()
            .expect("requests")
            .iter()
            .map(|n| n.as_u64().expect("a count"))
            .collect();
        assert!(
            requests.windows(2).all(|w| w[0] <= w[1]),
            "the rows are not ascending: {o:#}"
        );
    }
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "positive control: the refresh changed the row's figures: {result:#}"
        );
    }
}

/// A range click with the answer held: from the click on, until the answer
/// lands, the pane is busy and the section marked pending, it holds no table
/// under the old figures and keeps at least the height it had; then the new
/// range's table is drawn under the new label.
///
/// Mutation check: drop the minHeight line in reserveTrafficHeight -> red
/// (the section collapses while pending).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_range_change_is_pending_before_it_relabels() {
    let out = browser::run_scenario_with(
        &traffic_scenario("traffic-range-pending", "traffic", &[]),
        &[SCREEN],
    )
    .result;

    let before = &out["before"];
    assert_eq!(before["table"], true, "{out:#}");
    assert!(
        before["hint"]
            .as_str()
            .is_some_and(|h| h.ends_with("today")),
        "{out:#}"
    );
    let height = before["height"].as_f64().expect("a height");
    let samples = out["samples"].as_array().expect("samples");
    assert!(
        !samples.is_empty(),
        "positive control: the click repainted: {out:#}"
    );
    let (settled, pending) = samples.split_last().expect("a settled repaint");
    assert!(!pending.is_empty(), "no pending repaint was seen: {out:#}");
    for at in std::iter::once(&out["at_click"]).chain(pending) {
        assert_eq!(at["busy"], "true", "{at:#} in {out:#}");
        assert_eq!(at["section_pending"], true, "{at:#}");
        assert_eq!(
            at["table"], false,
            "old figures under the new range: {at:#}"
        );
        assert!(
            at["height"].as_f64().expect("a height") >= height - 1.0,
            "the pending section collapsed from {height}: {at:#}"
        );
    }
    assert_eq!(settled["busy"], Value::Null, "{out:#}");
    assert!(
        settled["hint"]
            .as_str()
            .is_some_and(|h| h.ends_with("this week")),
        "the first repaint that is not pending carries the new label: {out:#}"
    );
    let after = &out["after"];
    assert_eq!(after["busy"], Value::Null, "{out:#}");
    assert_eq!(after["section_pending"], false, "{out:#}");
    assert!(
        after["hint"]
            .as_str()
            .is_some_and(|h| h.ends_with("this week")),
        "positive control: the new range's table is labelled: {out:#}"
    );
}

/// An opened alias row draws its chain as a block of its own, outside the
/// label:value grid: one line per chain step in chain order, top to bottom,
/// labelled "Step <n>", a step that is another alias reading "via alias
/// mid", then the off-chain and never-dispatched lines. The step counts plus
/// the two remainders equal the row's requests, all read off the screen.
///
/// Mutation check: drop the `via alias ` prefix in aliasChainBlock -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_alias_chain_reads_in_order_and_adds_up() {
    let answer = nested_alias_answer();
    let top = group(&answer, "top").clone();
    let steps = top["steps"].as_array().expect("server steps").clone();
    assert_eq!(steps.len(), 3, "{top:#}");
    assert_eq!(steps[1]["kind"], "alias", "{top:#}");
    assert!(
        steps[1]["requests"].as_i64().unwrap_or(0) > 0
            && top["off_chain"].as_i64().unwrap_or(0) > 0
            && top["not_dispatched"].as_i64().unwrap_or(0) > 0,
        "positive control: the nested step and both remainders carry requests: {top:#}"
    );

    let out = detail_run("traffic?group=alias", "alias", answer, "top");

    let chain = &out["detail"]["chain"];
    assert_eq!(chain["head"], "Alias chain", "{out:#}");
    assert_eq!(chain["in_grid"], false, "{out:#}");
    let lines = block_lines(&out, "chain");
    let labels: Vec<&str> = lines
        .iter()
        .map(|l| l["label"].as_str().expect("a line label"))
        .collect();
    assert_eq!(
        labels,
        ["Step 1", "Step 2", "Step 3", "off chain", "not dispatched"],
        "{out:#}"
    );
    let names: Vec<&str> = lines[..3]
        .iter()
        .map(|l| l["name"].as_str().expect("a step name"))
        .collect();
    assert_eq!(names, ["sonnet", "via alias mid", "glm"], "{out:#}");
    let tops: Vec<f64> = lines
        .iter()
        .map(|l| l["top"].as_f64().expect("a line top"))
        .collect();
    assert!(
        tops.windows(2).all(|w| w[1] > w[0]),
        "each line sits below the one before it: {out:#}"
    );
    for (shown, served) in lines.iter().zip(&steps) {
        assert_eq!(shown["req"], served["requests"], "{out:#}");
    }
    assert_eq!(lines[3]["req"], top["off_chain"], "{out:#}");
    assert_eq!(lines[4]["req"], top["not_dispatched"], "{out:#}");
    let on_screen: i64 = lines
        .iter()
        .map(|l| l["req"].as_i64().expect("a count"))
        .sum();
    assert_eq!(on_screen, out["requests"].as_i64().unwrap(), "{out:#}");
    assert!(
        out["detail"]["pairs"]
            .as_array()
            .expect("pairs")
            .iter()
            .all(|p| !p["term"].as_str().unwrap_or("").starts_with("step")),
        "no chain step is left in the label:value grid: {out:#}"
    );
}

/// A share above zero that rounds to no whole percent reads "<1", in a chain
/// line and in the cache-write statement; a share that rounds to one or more
/// keeps its number.
///
/// Mutation check: make shareText return `String(Math.round(pct))` -> red
/// (both read "0").
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_a_share_that_rounds_to_nothing_reads_under_one_percent() {
    let mut answer = nested_alias_answer();
    let groups = answer["data"]["groups"].as_array_mut().expect("groups");
    let top = groups
        .iter_mut()
        .find(|g| g["label"] == "top")
        .expect("a `top` group");
    top["metrics"]["requests"] = json!(1000);
    for (step, n) in top["steps"]
        .as_array_mut()
        .expect("steps")
        .iter_mut()
        .zip([992, 3, 2])
    {
        step["requests"] = json!(n);
    }
    top["off_chain"] = json!(1);
    top["not_dispatched"] = json!(2);
    top["metrics"]["cache_write_present"] = json!(1);
    top["metrics"]["cache_write_tokens"] = json!(100_000);
    top["metrics"]["cache_write_unknown_tokens"] = json!(300);
    answer["data"]["totals"]["requests"] = json!(1000);

    let out = detail_run("traffic?group=alias", "alias", answer, "top");

    let shares: Vec<&str> = block_lines(&out, "chain")
        .iter()
        .map(|l| l["share"].as_str().expect("a share"))
        .collect();
    assert_eq!(shares, ["99%", "<1%", "<1%", "<1%", "<1%"], "{out:#}");
    let write = out["detail"]["pairs"]
        .as_array()
        .expect("pairs")
        .iter()
        .find(|p| p["term"] == "cache write")
        .expect("a cache write pair");
    assert!(
        write["text"]
            .as_str()
            .is_some_and(|t| t.ends_with("300 (<1%) of unknown lifetime")),
        "{out:#}"
    );
}

/// An opened row groups its missing prices per model in a block of their
/// own: one line per model in the server's order, naming each missing
/// dimension, and each configuration key on screen is the server's
/// `config_key` text exactly, in the server's order.
///
/// Mutation check: draw `'models.' + p.model` in place of `p.config_key` ->
/// red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_missing_prices_group_per_model_with_the_server_keys() {
    let answer = nested_alias_answer();
    let prices = group(&answer, "top")["metrics"]["missing_prices"]
        .as_array()
        .expect("missing prices")
        .clone();
    let mut models: Vec<&str> = Vec::new();
    for p in &prices {
        let model = p["model"].as_str().expect("a model");
        if models.last() != Some(&model) {
            models.push(model);
        }
    }
    assert!(
        prices.iter().any(|p| p["config_key"].is_string()) && models.len() < prices.len(),
        "positive control: a keyed price is missing and one model misses two: {prices:#?}"
    );

    let out = detail_run("traffic?group=alias", "alias", answer, "top");

    assert_eq!(out["detail"]["prices"]["head"], "No price", "{out:#}");
    assert_eq!(out["detail"]["prices"]["in_grid"], false, "{out:#}");
    let lines = block_lines(&out, "prices");
    let shown_models: Vec<&str> = lines
        .iter()
        .map(|l| l["name"].as_str().expect("a model name"))
        .collect();
    assert_eq!(shown_models, models, "{out:#}");
    for (line, model) in lines.iter().zip(&models) {
        let own: Vec<&Value> = prices.iter().filter(|p| p["model"] == *model).collect();
        let text = line["text"].as_str().expect("line text");
        for p in &own {
            let dim = p["dimension"].as_str().expect("a dimension");
            assert!(text.contains(dim), "`{dim}` missing from {line:#}");
        }
        let served_keys: Vec<&Value> = own
            .iter()
            .map(|p| &p["config_key"])
            .filter(|k| k.is_string())
            .collect();
        let shown_keys: Vec<&Value> = line["keys"].as_array().expect("keys").iter().collect();
        assert_eq!(shown_keys, served_keys, "{line:#}");
    }
}

/// The first load of Traffic over fourteen groups shifts its layout by under
/// 0.1. Positive
/// control: the same load with a 700 px block under the section from the
/// first paint, pushed down when the table lands as the old routing
/// sections were, scores at or above the limit.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_first_load_barely_shifts() {
    let dense = dense_answer();
    let run = |plant: bool| {
        let out = browser::run_scenario_with(
            &traffic_scenario(
                "traffic-layout-shift",
                "traffic",
                &[
                    ("routectlTrafficAnswers", json!({ "model": dense })),
                    ("routectlTrafficPlant", json!(plant)),
                ],
            ),
            &[SCREEN],
        )
        .result;
        out["first_load"].as_f64().expect("a shift score")
    };

    let first = run(false);
    let planted = run(true);

    assert!(
        first < 0.1,
        "first-load layout shift {first} (planted {planted})"
    );
    assert!(
        planted >= 0.1,
        "positive control: a planted push scored {planted}"
    );
}

/// The page writes no configuration key itself: every key it shows is the
/// server's text. Positive control: the scan finds a planted one.
#[test]
fn the_page_builds_no_models_config_key() {
    let builds_key = |src: &str| {
        strip_js_comments(src).contains("'models.") || strip_js_comments(src).contains("\"models.")
    };

    assert!(!builds_key(&script()), "the page spells a `models.` key");
    assert!(builds_key("var k = 'models.' + nick;"));
}

/// The token `var ALIAS_STEP_KIND = '<token>';` declares.
fn alias_step_kind() -> String {
    let script = script();
    let needle = "var ALIAS_STEP_KIND = '";
    let start = script
        .find(needle)
        .expect("the page declares ALIAS_STEP_KIND")
        + needle.len();
    let len = script[start..].find('\'').expect("the token is closed");
    script[start..start + len].to_owned()
}

/// The step fields the page reads are the keys an alias group serializes,
/// and a non-alias group carries none of them.
#[test]
fn the_alias_step_fields_are_the_served_group_keys() {
    let answer = nested_alias_answer();
    let top = group(&answer, "top");
    let mut group_keys: Vec<String> = string_array("ALIAS_GROUP_LISTS");
    group_keys.extend(string_array("ALIAS_GROUP_COUNTS"));
    let step = &top["steps"][0];
    let mut step_keys: Vec<String> = string_array("ALIAS_STEP_TEXT");
    step_keys.extend(string_array("ALIAS_STEP_COUNTS"));

    for key in &group_keys {
        assert!(
            top.get(key).is_some(),
            "`{key}` is not on an alias group: {top:#}"
        );
    }
    let mut served_step: Vec<&String> = step.as_object().expect("a step").keys().collect();
    served_step.sort();
    let mut declared: Vec<&String> = step_keys.iter().collect();
    declared.sort();
    assert_eq!(served_step, declared);
    assert!(
        top["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .any(|s| s["kind"] == alias_step_kind().as_str()),
        "ALIAS_STEP_KIND is the token a nested step carries: {top:#}"
    );
    let model_group = &served_answer(
        crate::handlers::status::panel_fixtures::fixture_config(),
        &NESTED_SEEDS,
        &json!({"window": "today", "group_by": "model"}),
    )["data"]["groups"][0];
    for key in &group_keys {
        assert!(model_group.get(key).is_none(), "{model_group:#}");
    }
}

/// The dense answer with each named group's metrics overwritten by `set`.
fn dense_with(set: &[(&str, Value)]) -> Value {
    let mut answer = dense_answer();
    let groups = answer["data"]["groups"].as_array_mut().expect("groups");
    for (label, metrics) in set {
        let g = groups
            .iter_mut()
            .find(|g| g["label"] == *label)
            .unwrap_or_else(|| panic!("a `{label}` group"));
        for (k, v) in metrics.as_object().expect("metric fields") {
            g["metrics"][k] = v.clone();
        }
    }
    answer
}

/// The page's idKey: `k`, then each ASCII letter or digit as itself and any
/// other character as `_<hex code>_`.
fn id_key(raw: &str) -> String {
    raw.chars().fold(String::from("k"), |mut out, ch| {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push_str(&format!("_{:x}_", u32::from(ch)));
        }
        out
    })
}

fn observe(hash: &'static str, answer: Value, open: &[&str]) -> Value {
    let keys: Vec<String> = open.iter().map(|l| id_key(l)).collect();
    browser::run_scenario(&traffic_scenario(
        "traffic-observe",
        hash,
        &[
            ("routectlTrafficAnswers", json!({ "model": answer })),
            ("routectlTrafficOpen", json!(keys)),
        ],
    ))
    .result
}

fn row<'a>(out: &'a Value, label: &str) -> &'a Value {
    out["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["label"] == label)
        .unwrap_or_else(|| panic!("a `{label}` row in {out:#}"))
}

/// A cost the server could not total is never drawn as a dollar zero: a
/// partial group with no priced row (`cost_usd` null) reads the word
/// `partial`, with no `$`; a partial group with a priced subtotal carries a
/// visible `partial` mark beside its figure; a priced group carries no mark;
/// a subscription group reads its word.
///
/// Mutation checks: costFigure handing `m.cost_usd` to money() whenever the
/// status is priced or partial -> red (`$-`); that and money(null)
/// returning '0.00' -> red (`$0.00`); the mark appended by
/// trafficCostFigure removed -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_an_unknown_cost_never_reads_as_zero() {
    let answer = dense_with(&[
        (
            "model-01",
            json!({"cost_status": "partial", "cost_usd": null}),
        ),
        (
            "model-02",
            json!({"cost_status": "partial", "cost_usd": 0.25}),
        ),
        (
            "model-03",
            json!({"cost_status": "priced", "cost_usd": 1.5}),
        ),
        (
            "model-04",
            json!({"cost_status": "subscription", "cost_usd": null}),
        ),
    ]);

    let out = observe("traffic", answer, &[]);

    let unpriced = row(&out, "model-01");
    assert_eq!(unpriced["cost"], "partial", "{unpriced:#}");
    assert_eq!(unpriced["cost_mark"], Value::Null, "{unpriced:#}");
    let subtotal = row(&out, "model-02");
    assert_eq!(subtotal["cost"], "$0.250partial", "{subtotal:#}");
    assert_eq!(subtotal["cost_mark"], "partial", "{subtotal:#}");
    assert_eq!(
        subtotal["cost_mark_spoken"],
        json!({"text": "partial (priced subtotal only)", "hidden_part_hidden": true}),
        "the mark's reason reaches a keyboard and a screen reader: {subtotal:#}"
    );
    let priced = row(&out, "model-03");
    assert_eq!(priced["cost"], "$1.50", "positive control: {priced:#}");
    assert_eq!(priced["cost_mark"], Value::Null, "{priced:#}");
    assert_eq!(row(&out, "model-04")["cost"], "subscription", "{out:#}");
    for r in out["rows"].as_array().expect("rows") {
        assert_ne!(r["cost"], "$0.00", "{r:#}");
    }
}

const UNKNOWN_LIFETIME: &str = "cache write (unknown lifetime)";

/// The "At API prices" cell never reads as the value of all of a group's
/// subscription traffic when it is not: an equivalent the server calls
/// `partial` carries the `partial` mark, whose title says what it leaves
/// out; a `complete` one carries no mark, even beside a listed missing price
/// (that gap belongs to another kind of row); subscription traffic with no
/// equivalent reads the faint word `unpriced`, as does a mix with no priced
/// row (only subscription and unpriced rows remain); a group that may hold
/// no subscription traffic keeps its `-`.
///
/// Mutation checks: the mark appended by equivalentFigure removed -> red;
/// equivalentCoverage ignoring `equivalent_status` (reading every present
/// equivalent as full) -> red (model-01 unmarked); equivalentCoverage reading
/// a listed missing price as partial -> red (model-07 marked); the
/// `unpriced` branch dropped -> red (model-03 reads `-`); the partial clause
/// of holdsSubscription dropped -> red (model-05 reads `-`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_an_equivalent_that_leaves_rows_out_says_so() {
    let gap = json!([{"model": "opus", "dimension": UNKNOWN_LIFETIME, "config_key": null}]);
    let sub = |equivalent: Value, status: &str, missing: &Value| {
        let dims: Vec<Value> = missing
            .as_array()
            .expect("a list")
            .iter()
            .map(|p| p["dimension"].clone())
            .collect();
        json!({
            "cost_status": "subscription",
            "cost_usd": null,
            "equivalent_cost_usd": equivalent,
            "equivalent_status": status,
            "missing_prices": missing,
            "missing_price_dimensions": dims,
        })
    };
    let mut beside_unpriced_row = sub(json!(12.5), "complete", &gap);
    beside_unpriced_row["cost_status"] = json!("partial");
    let answer = dense_with(&[
        ("model-01", sub(json!(268.29), "partial", &gap)),
        ("model-02", sub(json!(40.25), "complete", &json!([]))),
        ("model-03", sub(Value::Null, "partial", &gap)),
        (
            "model-04",
            json!({"cost_status": "priced", "cost_usd": 1.5, "equivalent_cost_usd": null}),
        ),
        (
            "model-05",
            json!({"cost_status": "partial", "cost_usd": null, "equivalent_cost_usd": null}),
        ),
        (
            "model-06",
            json!({"cost_status": "partial", "cost_usd": 0.25, "equivalent_cost_usd": null}),
        ),
        ("model-07", beside_unpriced_row),
    ]);

    let out = observe("traffic", answer, &[]);

    let partial = row(&out, "model-01");
    assert_eq!(partial["equivalent"], "~$268.29partial", "{partial:#}");
    assert_eq!(partial["equivalent_mark"], "partial", "{partial:#}");
    assert_eq!(
        partial["equivalent_mark_title"], EQUIVALENT_PARTIAL_TITLE,
        "{partial:#}"
    );
    assert_eq!(
        partial["equivalent_mark_spoken"],
        json!({
            "text": format!("partial ({EQUIVALENT_PARTIAL_TITLE})"),
            "hidden_part_hidden": true
        }),
        "the mark's reason reaches a keyboard and a screen reader: {partial:#}"
    );
    let full = row(&out, "model-02");
    assert_eq!(full["equivalent"], "~$40.25", "positive control: {full:#}");
    assert_eq!(full["equivalent_mark"], Value::Null, "{full:#}");
    let complete_beside_gap = row(&out, "model-07");
    assert_eq!(
        complete_beside_gap["equivalent"], "~$12.50",
        "{complete_beside_gap:#}"
    );
    assert_eq!(
        complete_beside_gap["equivalent_mark"],
        Value::Null,
        "{complete_beside_gap:#}"
    );
    let unpriced = row(&out, "model-03");
    assert_eq!(unpriced["equivalent"], "unpriced", "{unpriced:#}");
    assert_eq!(unpriced["equivalent_faint"], true, "{unpriced:#}");
    let priced = row(&out, "model-04");
    assert_eq!(priced["equivalent"], "-", "{priced:#}");
    assert_eq!(priced["equivalent_mark"], Value::Null, "{priced:#}");
    let mixed_unpriced = row(&out, "model-05");
    assert_eq!(
        mixed_unpriced["equivalent"], "unpriced",
        "{mixed_unpriced:#}"
    );
    let mixed_priced = row(&out, "model-06");
    assert_eq!(mixed_priced["equivalent"], "-", "{mixed_priced:#}");
}

/// The title of the `partial` mark on an "At API prices" figure, on every
/// view that draws one.
const EQUIVALENT_PARTIAL_TITLE: &str = "priced subtotal only: leaves out usage that has no price";

/// An opened row whose group wrote cache tokens of unknown lifetime says in
/// one plain sentence that those tokens have no price and that the dollar
/// figures leave them out, with the group's own count; a group with none
/// draws no sentence. The count reads the way the cache-write pair beside it
/// reads it, and one token reads in the singular.
///
/// Mutation checks: unpricedRemainderLine dropped from trafficRow -> red
/// (model-01 has no sentence); its `unknown <= 0` guard removed -> red
/// (model-02 draws one).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_an_unknown_lifetime_remainder_is_explained_as_unpriced() {
    let writes = |unknown: i64| {
        json!({
            "cache_write_present": 1,
            "cache_write_tokens": 2_000_000,
            "cache_write_unknown_tokens": unknown,
        })
    };
    let answer = dense_with(&[
        ("model-01", writes(1_234_567)),
        ("model-02", writes(0)),
        ("model-03", writes(1)),
    ]);

    let out = observe("traffic", answer, &["model-01", "model-02", "model-03"]);

    let details = &out["details"];
    let remainder = |label: &str| details[id_key(label)]["remainder"].clone();
    assert_eq!(
        remainder("model-01"),
        "1.2M cache-write tokens of unknown lifetime have no price; the cost and \
         At API prices figures leave them out.",
        "{out:#}"
    );
    assert_eq!(remainder("model-02"), Value::Null, "{out:#}");
    assert!(
        details[id_key("model-02")]["pairs"]
            .as_array()
            .is_some_and(|p| !p.is_empty()),
        "positive control: model-02's detail is open: {out:#}"
    );
    assert_eq!(
        remainder("model-03"),
        "1 cache-write token of unknown lifetime has no price; the cost and \
         At API prices figures leave it out.",
        "{out:#}"
    );
}

/// The remainder sentence is the one place an opened row explains writes of
/// unknown lifetime: where it is drawn, the No price block leaves that
/// dimension out (and drops the block when it was the only gap) while still
/// listing any other one, and the cache-write pair keeps its share. With no
/// sentence the dimension stays listed, with no "no configuration key" tail
/// naming it again; another unkeyed dimension keeps its tail (control).
///
/// Mutation checks: trafficMissingPrices returning `m.missing_prices` ->
/// red (model-01 draws a No price block); the dimension test dropped from
/// the unkeyed filter in missingPriceLine -> red (model-04's tail).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_the_remainder_sentence_replaces_its_no_price_line() {
    let unknown = crate::commands::usage::UNKNOWN_LIFETIME_LABEL;
    let gap = |dims: &[(&str, Option<&str>)]| -> Value {
        dims.iter()
            .map(|(dim, key)| json!({"model": "opus", "dimension": dim, "config_key": key}))
            .collect()
    };
    let writes = |unknown_tokens: i64, missing: Value| {
        json!({
            "cache_write_present": 1,
            "cache_write_tokens": 16_017,
            "cache_write_unknown_tokens": unknown_tokens,
            "missing_prices": missing,
        })
    };
    let answer = dense_with(&[
        ("model-01", writes(8_572, gap(&[(unknown, None)]))),
        (
            "model-02",
            writes(
                8_572,
                gap(&[
                    (unknown, None),
                    ("output", Some("models.opus.pricing.output")),
                ]),
            ),
        ),
        ("model-03", writes(0, gap(&[("input", None)]))),
        ("model-04", writes(0, gap(&[(unknown, None)]))),
    ]);

    let out = observe(
        "traffic",
        answer,
        &["model-01", "model-02", "model-03", "model-04"],
    );

    let detail = |label: &str| out["details"][id_key(label)].clone();
    let price_texts = |label: &str| -> Vec<String> {
        detail(label)["prices"]["lines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .map(|l| l["text"].as_str().expect("line text").to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    for label in ["model-01", "model-02"] {
        let d = detail(label);
        assert_eq!(d["remainders"], 1, "{label}: {d:#}");
        let write = d["pairs"]
            .as_array()
            .expect("pairs")
            .iter()
            .find(|p| p["term"] == "cache write")
            .expect("a cache write pair");
        assert!(
            write["text"]
                .as_str()
                .is_some_and(|t| t.ends_with("(54%) of unknown lifetime")),
            "{label}: {d:#}"
        );
    }
    assert_eq!(detail("model-01")["prices"], Value::Null, "{out:#}");
    assert_eq!(
        price_texts("model-02"),
        ["opus: output - set models.opus.pricing.output"],
        "{out:#}"
    );
    assert_eq!(detail("model-03")["remainders"], 0, "{out:#}");
    assert_eq!(
        price_texts("model-03"),
        ["opus: input - no configuration key prices input"],
        "control: another unkeyed dimension keeps its tail: {out:#}"
    );
    assert_eq!(detail("model-04")["remainders"], 0, "{out:#}");
    assert_eq!(
        price_texts("model-04"),
        [format!("opus: {unknown}")],
        "{out:#}"
    );
}

/// An error rate is plain text below 5% of a group's requests, the degraded
/// color from 5%, the broken color from 50%, and a zero is faint; each
/// boundary is a row, one request either side of it.
///
/// Mutation checks: errorRateState returning 'broken' for any rate above
/// zero -> red; `>=` turned to `>` at either threshold -> red (the 5.0% or
/// 50.0% row loses its color).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_error_rate_colors_only_past_its_thresholds() {
    let rate = |errors: i64| json!({"requests": 1000, "errors": errors});
    let cases: [(&str, i64, &str, Option<&str>); 6] = [
        ("model-01", 0, "0%", None),
        ("model-02", 1, "0.1%", None),
        ("model-03", 49, "4.9%", None),
        ("model-04", 50, "5.0%", Some("degraded")),
        ("model-05", 499, "49.9%", Some("degraded")),
        ("model-06", 500, "50.0%", Some("broken")),
    ];
    let set: Vec<(&str, Value)> = cases.iter().map(|c| (c.0, rate(c.1))).collect();

    let out = observe("traffic", dense_with(&set), &[]);

    let colors = &out["colors"];
    assert_ne!(colors["warn"], colors["neg"], "{colors:#}");
    for (label, _, text, state) in cases {
        let r = row(&out, label);
        assert_eq!(r["rate"], text, "`{label}`: {r:#}");
        let class: Vec<String> = state
            .map(|s| format!("traffic-rate--{s}"))
            .into_iter()
            .collect();
        assert_eq!(r["rate_class"], json!(class), "`{label}`: {r:#}");
        let want = match state {
            Some("degraded") => &colors["warn"],
            Some("broken") => &colors["neg"],
            _ => &r["rate_color"],
        };
        assert_eq!(&r["rate_color"], want, "`{label}`: {r:#}");
        if state.is_none() {
            assert_ne!(r["rate_color"], colors["neg"], "`{label}`: {r:#}");
            assert_ne!(r["rate_color"], colors["warn"], "`{label}`: {r:#}");
        }
    }
}

/// The table says what its "At API prices" column is, in the Now tile's own
/// line, and which requests each timing is read over, on the card and in
/// the column titles; an opened row names the populations of its tail
/// percentiles.
///
/// Mutation checks: the API-price line dropped from trafficNotes -> red; the
/// TTFT column title dropped -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_columns_say_what_they_measure() {
    let out = observe("traffic", dense_answer(), &["model-01"]);

    let notes = out["notes"].as_array().expect("notes");
    assert!(
        notes.contains(&json!(
            "At API prices: what this traffic would cost on pay-as-you-go API pricing"
        )),
        "{out:#}"
    );
    assert!(
        notes.contains(&json!(
            "Latency over every request, failures included; time to first token over streamed \
             successes"
        )),
        "{out:#}"
    );
    let title = |label: &str| {
        out["titles"]
            .as_array()
            .expect("titles")
            .iter()
            .find(|t| t[0] == label)
            .unwrap_or_else(|| panic!("a `{label}` column in {out:#}"))[1]
            .clone()
    };
    assert_eq!(
        title("At API prices"),
        "what this traffic would cost on pay-as-you-go API pricing"
    );
    assert_eq!(
        title("TTFT p50"),
        "median time to first token over streamed successes"
    );
    assert_eq!(
        title("latency p50"),
        "median latency over every request, failures included"
    );
    let terms: Vec<&str> = out["details"][id_key("model-01")]["pairs"]
        .as_array()
        .expect("pairs")
        .iter()
        .map(|p| p["term"].as_str().expect("a term"))
        .collect();
    assert_eq!(
        &terms[..2],
        [
            "TTFT p90, p99 over streamed successes",
            "latency p90, p99 over every request, failures included"
        ],
        "{out:#}"
    );
}

/// The scope strip's subline names what the scope holds: a model on a
/// provider, a provider alone, a model alone.
///
/// Mutation check: trafficScopeHint returning the provider line always ->
/// red (the model rows).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_scope_subline_names_the_scope() {
    let rows = [
        (
            "traffic?scope=anthropic&model=sonnet",
            "every figure below is this model on this provider only",
        ),
        (
            "traffic?scope=anthropic",
            "every figure below is this provider only",
        ),
        (
            "traffic?model=sonnet",
            "every figure below is this model only",
        ),
    ];
    for (hash, want) in rows {
        let out = observe(hash, dense_answer(), &[]);
        assert_eq!(out["scope_hint"], want, "`{hash}`: {out:#}");
    }
}

/// A Traffic read that fails says so in plain words, that it retries, and
/// the last good read when there was one, with the technical code kept as a
/// line of its own: on a cold load (nothing read yet, the daemon
/// unreachable) and after a good read (the ledger then unavailable).
///
/// Mutation check: viewSection called without the failed-read sentence ->
/// red (the card reads only its code).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_a_failed_read_says_what_failed_in_plain_words() {
    let run = |mode: &str| {
        browser::run_scenario(&traffic_scenario(
            "traffic-read-failed",
            "traffic",
            &[("routectlTrafficFail", json!(mode))],
        ))
        .result
    };

    let cold = run("network");
    assert_eq!(
        cold["card"]["lead"], "Couldn't read traffic from routectl; retrying automatically",
        "{cold:#}"
    );
    assert_eq!(
        cold["card"]["code"], "unavailable: transport failure",
        "{cold:#}"
    );

    let warm = run("unavailable-after-first");
    let lead = warm["card"]["lead"].as_str().expect("a lead sentence");
    assert!(
        lead.starts_with(
            "Couldn't read traffic from routectl; retrying automatically - last good read at "
        ),
        "{warm:#}"
    );
    assert!(
        warm["card"]["code"]
            .as_str()
            .is_some_and(|c| c.starts_with("unavailable: db_unavailable")),
        "{warm:#}"
    );
}
