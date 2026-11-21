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

fn entries<'a>(out: &'a Value, term: &str) -> Vec<&'a Value> {
    out["detail"]
        .as_array()
        .expect("the detail pairs")
        .iter()
        .filter(|d| d["term"].as_str().is_some_and(|t| t.starts_with(term)))
        .collect()
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

/// An opened alias row lists each chain step as the server split it: a
/// step that is another alias reads "via alias mid", and the step counts plus
/// the off-chain and never-dispatched counts equal the row's requests, all
/// read off the screen.
///
/// Mutation check: drop the `via alias ` prefix in stepShare -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_alias_steps_read_via_alias_and_add_up() {
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

    let shown_steps = entries(&out, "step ");
    let names: Vec<&str> = shown_steps
        .iter()
        .map(|s| s["stepname"].as_str().expect("a step name"))
        .collect();
    assert_eq!(names, ["sonnet", "via alias mid", "glm"], "{out:#}");
    for (shown, served) in shown_steps.iter().zip(&steps) {
        assert_eq!(shown["req"], served["requests"], "{out:#}");
    }
    let remainder = |term: &str| entries(&out, term)[0]["req"].as_i64().expect("a count");
    assert_eq!(remainder("off chain"), top["off_chain"].as_i64().unwrap());
    assert_eq!(
        remainder("not dispatched"),
        top["not_dispatched"].as_i64().unwrap()
    );
    let on_screen: i64 = shown_steps
        .iter()
        .map(|s| s["req"].as_i64().expect("a count"))
        .sum::<i64>()
        + remainder("off chain")
        + remainder("not dispatched");
    assert_eq!(on_screen, out["requests"].as_i64().unwrap(), "{out:#}");
}

/// An opened row lists one line per missing price, and each configuration
/// key on screen is the server's `config_key` text exactly, in the server's
/// order.
///
/// Mutation check: draw `'models.' + p.model` in place of `p.config_key` ->
/// red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_traffic_missing_prices_show_the_server_config_key() {
    let answer = nested_alias_answer();
    let served: Vec<String> = group(&answer, "top")["metrics"]["missing_prices"]
        .as_array()
        .expect("missing prices")
        .iter()
        .filter_map(|p| p["config_key"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !served.is_empty(),
        "positive control: the fixture leaves a keyed price missing: {answer:#}"
    );

    let out = detail_run("traffic?group=alias", "alias", answer, "top");

    let shown: Vec<String> = entries(&out, "no price")
        .iter()
        .flat_map(|e| e["keys"].as_array().expect("keys").clone())
        .map(|k| k.as_str().expect("key text").to_owned())
        .collect();
    assert_eq!(shown, served, "{out:#}");
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
