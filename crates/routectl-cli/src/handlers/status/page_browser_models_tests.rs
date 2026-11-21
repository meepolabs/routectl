// The Models view in a real headless Chrome: lanes in the panel's order
// with one cause line over the lanes one attention item names, lanes no
// request reached in seven days collapsed to one line (and nothing collapsed
// while that read is not live), a lane row kept across refreshes, raw state
// with the attention list down, transit wording, and a nickname link that
// lands on Traffic scoped to it. `include!`d into a child module of
// `page::tests` beside `browser`; the scenarios live in
// page_browser_models_driver.js.

use serde_json::Value;

use crate::handlers::status::panel_fixtures::FIXTURE_NOW_MS;
use crate::handlers::status::ui_case_fixtures::{CaseSource, UiCase};

const MODELS_DRIVER: &str = include_str!("page_browser_models_driver.js");

const MODELS_BUDGET_MS: u64 = 40_000;

const HOP_LANES: [&str; 3] = [
    "passthrough#claude-codex-sol",
    "passthrough#claude-codex-terra",
    "passthrough#claude-opus-5-5",
];

fn populated(name: &'static str, snippets: &'static [&'static str]) -> Value {
    browser::run_scenario(&browser::Scenario {
        name,
        hash: "models",
        snippets,
        budget_ms: MODELS_BUDGET_MS,
    })
    .result
}

fn hop_case(snippets: &'static [&'static str]) -> Value {
    browser::run_case_scenario(
        &browser::Scenario {
            name: "models-rows",
            hash: "models",
            snippets,
            budget_ms: MODELS_BUDGET_MS,
        },
        browser::CaseServed {
            source: CaseSource::open(UiCase::PassthroughHop, FIXTURE_NOW_MS, "127.0.0.1:8787"),
            now_ms: Some(FIXTURE_NOW_MS),
        },
    )
    .result
}

fn drawn(result: &Value) -> &Vec<Value> {
    result["rows"].as_array().expect("rows drawn")
}

/// The page's `idKey` of `raw`.
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

/// The page's row id of `lane` in table `table`.
fn lane_id(table: &str, lane: &str) -> String {
    format!("{table}-{}", id_key(lane))
}

fn ids_in<'a>(result: &'a Value, table: &str) -> Vec<&'a str> {
    drawn(result)
        .iter()
        .filter(|r| r["table"] == table)
        .map(|r| r["id"].as_str().expect("an id"))
        .collect()
}

fn hop_item_key() -> String {
    let agg = CaseSource::open(UiCase::PassthroughHop, FIXTURE_NOW_MS, "127.0.0.1:8787")
        .aggregate(FIXTURE_NOW_MS);
    agg["panels"]["attention"]["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["kind"] == "provider_failing")
        .and_then(|item| item["key"].as_str())
        .expect("the hop case lists one provider fold")
        .to_owned()
}

/// The passthrough hop: one cause line, carrying the attention item's own
/// words, sits over the three hop lanes, which come first; the busy lane
/// follows as its own row; the pooled nickname nothing reached in seven
/// days collapses with both its seat lanes into one line.
///
/// Mutation check: give every row its own group in modelsCauseGroups -> red
/// (three cause lines).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_one_cause_line_over_the_hop_and_unused_lanes_collapse() {
    let key = hop_item_key();

    let result = hop_case(&[MODELS_DRIVER]);

    let main: Vec<&Value> = drawn(&result)
        .iter()
        .filter(|r| r["table"] == "models-lanes")
        .collect();
    let causes: Vec<&&Value> = main.iter().filter(|r| r["kind"] == "cause").collect();
    assert_eq!(causes.len(), 1, "{result:#}");
    let cause = causes[0];
    assert_eq!(cause["id"], format!("models-cause-{}", id_key(&key)));
    let text = cause["text"].as_str().expect("cause words");
    assert!(
        text.starts_with("Circuit open on 3 routes, last outcome server error (5xx)"),
        "{text}"
    );
    assert_eq!(cause["count"], "3 lanes");
    let mut expected: Vec<String> = HOP_LANES
        .iter()
        .map(|lane| lane_id("models-lanes", lane))
        .collect();
    expected.insert(0, cause["id"].as_str().expect("id").to_owned());
    expected.push(lane_id("models-lanes", "busy#claude-sonnet-4-5"));
    assert_eq!(ids_in(&result, "models-lanes"), expected, "{result:#}");
    assert_eq!(
        result["unused"], "2 lanes with no request in the last 7 days",
        "{result:#}"
    );
    assert_eq!(
        ids_in(&result, "models-unused-lanes"),
        [
            lane_id("models-unused-lanes", "seat-a#claude-sonnet-4-5"),
            lane_id("models-unused-lanes", "seat-b#claude-sonnet-4-5"),
        ]
    );
    assert_eq!(result["error_cards"], 0, "{result:#}");
}

/// While the seven-day per-model read has not answered, no lane collapses:
/// the hop case draws all six lanes in the main table and no unused line.
///
/// Mutation check: return an empty set instead of null from
/// modelsUsedNicknames while the read is not live -> red (every lane
/// collapses).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_nothing_collapses_before_the_lanes_query_answers() {
    let result = hop_case(&[
        "window.routectlModels = { holdLanesQuery: true };",
        MODELS_DRIVER,
    ]);

    let lanes = drawn(&result)
        .iter()
        .filter(|r| r["kind"] == "lane" && r["table"] == "models-lanes")
        .count();
    assert_eq!(lanes, 6, "{result:#}");
    assert_eq!(result["unused"], Value::Null, "{result:#}");
}

/// An expanded lane row is the same node, still open, across two refreshes
/// whose transit count moved, and the count shows each round's value.
///
/// Mutation check: key a lane row by `lane` plus its checked count in
/// modelsRow -> red (the node is replaced when the count moves).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_lane_row_is_kept_across_two_refreshes() {
    let result = populated("models-kept", &[MODELS_DRIVER]);

    let rounds = result["rounds"].as_array().expect("rounds observed");
    assert_eq!(rounds.len(), 2, "{result:#}");
    let obs: Vec<&Value> = std::iter::once(&result["baseline"]).chain(rounds).collect();
    for (i, o) in obs.iter().enumerate() {
        for flag in ["same_node", "connected", "open"] {
            assert_eq!(o[flag], true, "`{flag}` broke at {i}: {o:#}");
        }
    }
    for pair in obs.windows(2) {
        assert_ne!(pair[0]["figure"], pair[1]["figure"], "{obs:#?}");
    }
    let detail = obs[2]["detail"].as_str().expect("detail text");
    for part in [
        "capabilities",
        "most-changed fields",
        "field_00",
        "other fields",
    ] {
        assert!(detail.contains(part), "`{part}` in the detail: {detail}");
    }
}

/// With the attention list unavailable every lane still draws with its raw
/// state, each cause line says the cause is not available, and the view
/// shows no error card.
///
/// Mutation check: read `SOURCES.attention.data.items` directly in
/// modelsSection -> red (the section draws its error card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_rows_keep_their_raw_state_without_the_attention_list() {
    let result = populated(
        "models-rows",
        &[
            "window.routectlModels = { attentionDown: true };",
            MODELS_DRIVER,
        ],
    );

    let lanes: Vec<&Value> = drawn(&result)
        .iter()
        .filter(|r| r["kind"] == "lane")
        .collect();
    assert_eq!(lanes.len(), 3, "{result:#}");
    let state_of = |provider: &str| {
        lanes
            .iter()
            .find(|r| r["provider"] == provider)
            .and_then(|r| r["state"].as_str())
            .unwrap_or_else(|| panic!("a `{provider}` lane: {result:#}"))
    };
    assert!(
        state_of("claude-seat").starts_with("credential unusable"),
        "{result:#}"
    );
    assert!(state_of("compat").starts_with("breaker open"), "{result:#}");
    assert_eq!(state_of("anthropic"), "ok");
    let causes: Vec<&Value> = drawn(&result)
        .iter()
        .filter(|r| r["kind"] == "cause")
        .collect();
    assert_eq!(causes.len(), 2, "{result:#}");
    for cause in causes {
        assert!(
            cause["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("cause not available - ")),
            "{cause:#}"
        );
    }
    assert_eq!(result["error_cards"], 0, "{result:#}");
}

/// A lane whose route item cleared shows how long ago, read from the
/// attention list's recovered item; a lane no recovered item names shows
/// nothing of the kind.
///
/// Mutation check: never look up the recovered item in modelsStateCell ->
/// red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_cleared_problem_shows_its_age() {
    let result = populated(
        "models-rows",
        &[
            "window.routectlModels = { recoveredLane: { subject: 'sonnet', minutes: 7 } };",
            MODELS_DRIVER,
        ],
    );
    let control = populated("models-rows", &[MODELS_DRIVER]);

    let sonnet = |r: &Value| {
        drawn(r)
            .iter()
            .find(|row| row["provider"] == "anthropic")
            .and_then(|row| row["state"].as_str())
            .map(str::to_owned)
            .expect("the sonnet lane")
    };
    assert_eq!(sonnet(&result), "okrecovered 7 min ago", "{result:#}");
    assert_eq!(sonnet(&control), "ok", "{control:#}");
}

/// Transit reads exact counts beside the responses checked, "unmeasured"
/// for a lane nothing was checked on, and never says error, loss or rate.
///
/// Mutation check: drop the `checked <= 0` arm of modelsTransitText -> red.
/// Positive control: the transit text nodes exist, one per lane, and a
/// measured lane reads its counts.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_transit_reads_counts_or_unmeasured_and_never_a_failure_word() {
    let result = populated("models-rows", &[MODELS_DRIVER]);

    let shown: Vec<&str> = result["transit_nodes"]
        .as_array()
        .expect("transit nodes")
        .iter()
        .map(|t| t.as_str().expect("text"))
        .collect();
    let details: Vec<&str> = result["detail_transit"]
        .as_array()
        .expect("detail transit")
        .iter()
        .map(|t| t.as_str().expect("text"))
        .collect();
    assert_eq!(shown.len(), 3, "{result:#}");
    assert_eq!(details.len(), 3, "{result:#}");
    assert!(shown.contains(&"2 of 5 responses changed"), "{shown:?}");
    assert!(shown.contains(&"0 of 1 responses changed"), "{shown:?}");
    assert!(shown.contains(&"unmeasured"), "{shown:?}");
    assert!(
        details
            .iter()
            .any(|t| t.contains("2 of 5 responses changed since ")),
        "{details:?}"
    );
    let failure = |t: &str| {
        let lower = t.to_ascii_lowercase();
        ["error", "loss", "rate"].iter().any(|w| lower.contains(w))
    };
    for text in shown.iter().chain(&details) {
        assert!(!failure(text), "transit text `{text}` names a failure");
    }
    assert!(failure("an error rate"), "control: the matcher fires");
}

/// A nickname's link lands on Traffic with the scope and model applied: the
/// address carries both, the strip names both, and the QUERY Traffic sends
/// carries both.
///
/// Mutation check: drop `state.model = String(nickname);` from
/// modelsNicknameLinks -> red (the body carries no model).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_nickname_link_lands_on_traffic_scoped_to_it() {
    let report = browser::run_scenario(&browser::Scenario {
        name: "models-link",
        hash: "models",
        snippets: &[MODELS_DRIVER],
        budget_ms: MODELS_BUDGET_MS,
    });
    let result = &report.result;

    assert_eq!(result["href"], "#traffic?scope=anthropic&model=sonnet");
    assert_eq!(result["hash"], "#traffic?scope=anthropic&model=sonnet");
    assert_eq!(
        result["strip"], "Scoped to model sonnet on provider anthropic",
        "{result:#}"
    );
    let scoped = report.requests.iter().any(|r| {
        r["body"]
            .as_str()
            .and_then(|b| serde_json::from_str::<Value>(b).ok())
            .is_some_and(|b| {
                b["group_by"] == "model"
                    && b["window"] == "today"
                    && b["provider"] == "anthropic"
                    && b["model"] == "sonnet"
            })
    });
    assert!(
        scoped,
        "Traffic sent the scoped body: {:#?}",
        report.requests
    );
}

/// The seven-day per-model read describes every lane, so it never carries
/// the selection's scope: on a page scoped to one model the lanes QUERY
/// goes out without `provider` or `model`. Positive control: the address
/// scope was applied (Traffic's own body carries it).
///
/// Mutation check: mark QUERY_MODELS_SOURCE `scoped: true` -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_the_lanes_query_ignores_the_scope() {
    let report = browser::run_scenario(&browser::Scenario {
        name: "models-scope-then-lanes",
        hash: "traffic?scope=anthropic&model=sonnet",
        snippets: &[MODELS_DRIVER],
        budget_ms: MODELS_BUDGET_MS,
    });

    let bodies: Vec<Value> = report
        .requests
        .iter()
        .filter_map(|r| r["body"].as_str())
        .filter_map(|b| serde_json::from_str::<Value>(b).ok())
        .collect();
    assert!(
        bodies
            .iter()
            .any(|b| b["window"] == "today" && b["model"] == "sonnet"),
        "positive control: Traffic sent the scoped body: {bodies:#?}"
    );
    let lanes: Vec<&Value> = bodies
        .iter()
        .filter(|b| b["window"] == "last7d" && b["group_by"] == "model")
        .collect();
    assert!(!lanes.is_empty(), "the lanes QUERY went out: {bodies:#?}");
    for body in lanes {
        assert!(body.get("model").is_none(), "{body}");
        assert!(body.get("provider").is_none(), "{body}");
    }
    assert_eq!(
        report.result["models_hash"],
        "#models?scope=anthropic&model=sonnet"
    );
}
