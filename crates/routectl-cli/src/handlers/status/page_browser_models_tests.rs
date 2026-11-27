// The Models view in a real headless Chrome: lanes in the panel's order
// with one cause line over the lanes one attention item names, lanes no
// request reached in seven days collapsed to one line (nothing collapsed
// before that read answers, and the collapse kept when a refresh of it
// fails), a lane row kept across refreshes, raw state with the attention
// list down, transit wording, and a nickname link that lands on Traffic
// scoped to it. `include!`d into a child module of `page::tests` beside
// `browser`; the scenarios live in page_browser_models_driver.js.

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
    hop_case_named("models-rows", snippets)
}

fn hop_case_named(name: &'static str, snippets: &'static [&'static str]) -> Value {
    browser::run_case_scenario(
        &browser::Scenario {
            name,
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

fn lane_row<'a>(result: &'a Value, table: &str, lane: &str) -> &'a Value {
    let id = lane_id(table, lane);
    drawn(result)
        .iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap_or_else(|| panic!("no `{lane}` row in {table}: {result:#}"))
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

/// Each lane's row toggle is named by what the row shows, its nickname and
/// provider cells, never by the `#`-joined lane key its id is built from.
///
/// Mutation check: drop the `rowName` argument from modelsRow's xrow call
/// -> red (the toggle is named by the lane key).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_row_toggles_are_named_by_nickname_and_provider() {
    let result = hop_case(&[MODELS_DRIVER]);

    for lane in HOP_LANES {
        let row = lane_row(&result, "models-lanes", lane);
        let names: Vec<&str> = row["names"]
            .as_array()
            .expect("nickname links")
            .iter()
            .map(|n| n["text"].as_str().expect("a nickname is text"))
            .collect();
        let provider = row["provider"].as_str().expect("provider cell");
        assert!(!names.is_empty() && !provider.is_empty(), "{row:#}");
        let toggle = row["toggle_name"].as_str().expect("toggle name");

        assert_eq!(
            toggle,
            format!("Details for {} on {provider}", names.join(", ")),
            "{row:#}"
        );
        assert!(!toggle.contains(lane), "`{lane}` named: {row:#}");
        assert!(!toggle.contains('#'), "{row:#}");
    }
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

/// A failed refresh of the seven-day per-model read keeps the fold: the
/// collapsed line stays the same open node, and an unused lane expanded
/// inside it stays the same open row, after the first failure and after the
/// next round.
///
/// Mutation check: return null from modelsUsedNicknames unless the read is
/// live -> red (the failure unfolds every lane and replaces the row).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_failed_lanes_refresh_keeps_the_collapsed_lanes() {
    let result = hop_case_named("models-stale-fold", &[MODELS_DRIVER]);

    let before = &result["before"];
    assert_eq!(
        before["unused"], "2 lanes with no request in the last 7 days",
        "{result:#}"
    );
    for (at, obs) in [
        ("before", before),
        ("failed_once", &result["failed_once"]),
        ("next_round", &result["next_round"]),
    ] {
        for flag in [
            "same_fold",
            "fold_open",
            "same_node",
            "connected",
            "in_fold",
            "open",
        ] {
            assert_eq!(obs[flag], true, "`{flag}` broke at {at}: {result:#}");
        }
    }
    assert_eq!(before["failed"], 0, "{result:#}");
    assert!(
        result["failed_once"]["failed"].as_u64() >= Some(1),
        "the lanes QUERY failed: {result:#}"
    );
    assert_eq!(
        result["next_round"]["unused"], before["unused"],
        "{result:#}"
    );
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
    assert!(state_of("compat").starts_with("circuit open"), "{result:#}");
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
/// The expansion repeats transit only where it has counts: the two measured
/// lanes add a line, the unmeasured one does not.
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
    assert_eq!(details.len(), 2, "{result:#}");
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

/// A nickname's link, followed from the keyboard on the default (Today)
/// range, lands on Traffic for the last seven days with the scope and model
/// applied: the address carries all three, the strip names both, the QUERY
/// Traffic sends carries them, and focus sits on the strip's heading, where
/// the next refresh leaves it.
///
/// Mutation checks: drop `state.model = String(nickname);` from
/// modelsNicknameLinks -> red (the body carries no model); drop the
/// `landFollowedLink(next);` call in syncFromLocation -> red (focus is not on
/// the heading).
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

    let address = "#traffic?range=last7d&scope=anthropic&model=sonnet";
    assert_eq!(result["href"], address);
    assert_eq!(result["hash"], address);
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
                    && b["window"] == "last7d"
                    && b["provider"] == "anthropic"
                    && b["model"] == "sonnet"
                    && b.get("bucket").is_none_or(Value::is_null)
            })
    });
    assert!(
        scoped,
        "Traffic sent the scoped body: {:#?}",
        report.requests
    );
    assert!(
        result["focused_before"]
            .as_str()
            .is_some_and(|id| id != "usage-scope-title"),
        "positive control: focus starts on the link, not the heading: {result:#}"
    );
    assert_eq!(result["focused"], "usage-scope-title", "{result:#}");
    assert_eq!(
        result["focused_text"], "Scoped to model sonnet on provider anthropic",
        "{result:#}"
    );
    assert_eq!(
        result["focused_after_refresh"], "usage-scope-title",
        "{result:#}"
    );
    assert_eq!(result["same_heading"], true, "{result:#}");
}

/// The range a nickname link carries: Today, which Models cannot show and
/// which is empty on most mornings, becomes the seven days the fold reads;
/// any other range the operator picked is kept as it is.
///
/// Mutation check: drop the `today` check in modelsNicknameLinks so every
/// range becomes last7d -> red (the Week and All rows).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_nickname_link_keeps_the_range_except_today() {
    for (hash, range) in [
        ("models", "last7d"),
        ("models?range=week", "week"),
        ("models?range=last24h", "last24h"),
        ("models?range=all", "all"),
    ] {
        let result = browser::run_scenario(&browser::Scenario {
            name: "models-link-hrefs",
            hash,
            snippets: &[MODELS_DRIVER],
            budget_ms: MODELS_BUDGET_MS,
        })
        .result;

        let hrefs: Vec<&str> = result["hrefs"]
            .as_array()
            .expect("hrefs")
            .iter()
            .map(|h| h.as_str().expect("an href"))
            .collect();
        assert!(!hrefs.is_empty(), "`{hash}`: {result:#}");
        for href in hrefs {
            assert!(
                href.starts_with(&format!("#traffic?range={range}&scope=")),
                "`{hash}`: {href}"
            );
        }
    }
}

/// A lane's own duration and the cause line over it read one instant the
/// same way: with the attention list up, the hop lanes under the cause line
/// carry no duration of their own (the line says it); with the list down
/// they carry one, and it is the exact words the attention item uses for the
/// same `since_ms` (the daemon's own formatter, read from its payload).
///
/// Mutation checks: format the lane duration with humanDuration -> red
/// (`5m` against `5 min`); drop the cause-item check in modelsSinceText ->
/// red (the lanes under the line repeat the duration).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_lane_and_its_cause_line_say_one_duration() {
    let live = hop_case(&[MODELS_DRIVER]);
    let down = hop_case(&[
        "window.routectlModels = { attentionDown: true };",
        MODELS_DRIVER,
    ]);

    let cause = drawn(&live)
        .iter()
        .find(|r| r["kind"] == "cause")
        .and_then(|r| r["text"].as_str())
        .expect("the hop cause line");
    let (_, span) = cause
        .rsplit_once(", for ")
        .unwrap_or_else(|| panic!("the cause says how long: {cause}"));
    for lane in HOP_LANES {
        let state = lane_row(&live, "models-lanes", lane)["state"]
            .as_str()
            .expect("state text");
        assert_eq!(state, "circuit open", "{lane} under the cause line");
        let state = lane_row(&down, "models-lanes", lane)["state"]
            .as_str()
            .expect("state text");
        assert_eq!(state, format!("circuit openfor {span}"), "{lane}");
    }
}

/// The lane duration across its arms, each rounded down: seconds read as
/// under a minute, then whole minutes, whole hours, one day, whole days.
///
/// Mutation check: round minutes up (Math.ceil) in spanText -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_a_lane_duration_reads_each_span_in_whole_units() {
    let spans: [(u64, &str); 6] = [
        (30_000, "under 1 min"),
        (60_000, "1 min"),
        (419_000, "6 min"),
        (7_259_000, "2 h"),
        (129_600_000, "1 day"),
        (262_800_000, "3 days"),
    ];
    let list: Vec<String> = spans.iter().map(|(ms, _)| ms.to_string()).collect();
    let set = format!(
        "window.routectlModels = {{ attentionDown: true, spanLanes: [{}] }};",
        list.join(", ")
    );
    let snippets: &'static [&'static str] =
        Box::leak(vec![&*Box::leak(set.into_boxed_str()), MODELS_DRIVER].into_boxed_slice());

    let result = populated("models-rows", snippets);

    for (i, (_, words)) in spans.iter().enumerate() {
        let provider = format!("span-{i}");
        let state = drawn(&result)
            .iter()
            .find(|r| r["provider"] == provider.as_str())
            .and_then(|r| r["state"].as_str())
            .unwrap_or_else(|| panic!("a `{provider}` lane: {result:#}"));
        assert!(
            state.ends_with(&format!("for {words}")),
            "{provider}: `{state}` should end in `for {words}`"
        );
    }
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

/// State is the first data column, after the row toggle; the rest keep
/// their order. Read from the main table's header row.
///
/// Mutation check: move `C('State')` back after `C('Account')` in
/// MODELS_COLUMNS -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_state_is_the_first_column() {
    let result = populated("models-rows", &[MODELS_DRIVER]);

    assert_eq!(
        result["headers"],
        serde_json::json!([
            "Details",
            "State",
            "Model",
            "Provider",
            "Upstream",
            "Account",
            "Refuses",
            "Changed in transit"
        ]),
        "{result:#}"
    );
}

/// The lanes sit in one card titled like the section, and the line the
/// unused lanes collapse to sits in that card under the table.
///
/// Mutation check: return the bare fragment instead of `card(...)` from
/// modelsCard -> red (no card around the table).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_the_lanes_sit_in_a_card() {
    let result = hop_case(&[MODELS_DRIVER]);

    assert_eq!(
        result["card"],
        serde_json::json!({ "title": "Model lanes", "holds_unused": true }),
        "{result:#}"
    );
}

/// A chip reads plain words for the capability and verdict tokens the panel
/// sends: a verdict in the word map reads as its words (`broken` ->
/// `refused`), and any other token reads as itself with its underscores
/// spaced. The raw tokens stay at the head of the tooltip, and a refusal
/// keeps its own mark.
///
/// Mutation checks: render `cell.capability + ' ' + cell.verdict` again in
/// modelsCapabilities -> red (`forced_tool_choice unknown`); drop the
/// underscore replacement from modelsSpacedToken -> red (the unknown token
/// keeps its underscores); drop `broken` from MODELS_VERDICT_WORDS -> red
/// (`web search: broken`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_chips_read_plain_words() {
    let result = populated(
        "models-rows",
        &[
            "window.routectlModels = { extraCapability: \
             { capability: 'next_gen_tool', verdict: 'half_known' } };",
            MODELS_DRIVER,
        ],
    );

    let chips = result["chips"].as_array().expect("chips");
    let chip = |text: &str| {
        chips
            .iter()
            .find(|c| c["text"] == text)
            .unwrap_or_else(|| panic!("a `{text}` chip: {chips:#?}"))
    };
    let known = chip("forced tool choice: unknown");
    assert!(
        known["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("forced_tool_choice - unknown, ")),
        "{known:#}"
    );
    let unknown = chip("next gen tool: half known");
    assert!(
        unknown["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("next_gen_tool - half_known, ")),
        "{unknown:#}"
    );
    let refusal = chips
        .iter()
        .find(|c| {
            c["refusal"] == true
                && c["title"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("web_search - broken"))
        })
        .unwrap_or_else(|| panic!("the web_search refusal chip: {chips:#?}"));
    assert_eq!(refusal["text"], "web search: refused", "{refusal:#}");
    for c in chips {
        let text = c["text"].as_str().expect("chip text");
        assert!(!text.contains('_'), "a raw token on a chip: {text}");
    }
}

/// A nickname link reads at regular weight with no underline at rest,
/// though its row-header cell is bold; keyboard focus underlines it and
/// draws the 2px focus ring.
///
/// Mutation checks: drop `font-weight: 400;` from `.models-link` -> red
/// (600, the cell's weight); drop `text-decoration: none;` -> red
/// (underlined at rest); drop `.models-link:focus-visible` from the
/// underline rule -> red (no underline on focus).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_nickname_links_are_light_at_rest() {
    let result = populated("models-rows", &[MODELS_DRIVER]);

    let link = &result["link"];
    assert_eq!(link["cell_weight"], "600", "positive control: {link:#}");
    assert_eq!(link["weight"], "400", "{link:#}");
    assert_eq!(link["decoration"], "none", "{link:#}");
    assert_eq!(link["focus_visible"], true, "{link:#}");
    assert_eq!(link["focus_decoration"], "underline", "{link:#}");
    assert_eq!(link["focus_outline"], "solid 2px", "{link:#}");
}

/// A lane with no refusal and one nothing was checked on draw a faint dash
/// in Refuses and in Changed in transit, and a screen reader is handed the
/// word: Chrome's computed name reads `none` and `unmeasured`. A measured
/// lane keeps its counts as drawn. Positive control: the busy lane's cells
/// are the ones read (both absent), and its state reads `ok`.
///
/// Mutation checks: append the word as plain text instead of hidden text in
/// modelsAbsent -> red (the shown text is the word); drop the hidden word
/// -> red (the name is `-`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_an_absent_value_draws_a_dash_and_reads_its_word() {
    let result = hop_case(&[MODELS_DRIVER]);

    let busy = lane_row(&result, "models-lanes", "busy#claude-sonnet-4-5");
    assert_eq!(busy["state"], "ok", "{busy:#}");
    assert_eq!(busy["refuses"], "-", "{busy:#}");
    assert_eq!(busy["refuses_name"], "none", "{busy:#}");
    assert_eq!(busy["transit"], "-", "{busy:#}");
    assert_eq!(busy["transit_name"], "unmeasured", "{busy:#}");

    let measured = populated("models-rows", &[MODELS_DRIVER]);
    let counted = drawn(&measured)
        .iter()
        .find(|r| r["transit"] == "2 of 5 responses changed")
        .unwrap_or_else(|| panic!("a measured lane: {measured:#}"));
    assert_eq!(
        counted["transit_name"], "2 of 5 responses changed",
        "{counted:#}"
    );
}

const HOP_SCREEN: &str = "--window-size=1024,900";

fn hop_reach() -> Value {
    browser::run_case_scenario_with(
        &browser::Scenario {
            name: "models-reach",
            hash: "models",
            snippets: &[MODELS_DRIVER],
            budget_ms: MODELS_BUDGET_MS,
        },
        browser::CaseServed {
            source: CaseSource::open(UiCase::PassthroughHop, FIXTURE_NOW_MS, "127.0.0.1:8787"),
            now_ms: Some(FIXTURE_NOW_MS),
        },
        &[HOP_SCREEN],
    )
    .result
}

fn px(v: &Value, part: &str, side: &str) -> f64 {
    v[part][side]
        .as_f64()
        .unwrap_or_else(|| panic!("no {part}.{side}: {v:#}"))
}

/// At 1024 px on the passthrough hop, whose lanes refuse nothing and are
/// unmeasured, the whole table fits its card: the Changed in transit header
/// and every lane's transit cell end inside the scroller, which has nothing
/// to scroll. The Refuses column asks for no more than its header label and
/// padding (read with the table at its max-content width, so no leftover
/// card width is shared into it), and each absent value draws in the faint
/// color. Positive control: the
/// window is 1024 px and the columns read are the named ones.
///
/// Mutation checks: draw `unmeasured` as a word again in modelsTransitCell
/// -> red (seven faint dashes, not eight); make Refuses a wrapping column again
/// (`W('Refuses')`) -> red (Refuses wider than its label); drop `mag-zero`
/// from modelsAbsent -> red (a dash in the cell color).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_changed_in_transit_fits_the_card_at_1024() {
    let out = hop_reach();

    assert_eq!(out["inner_width"], 1024, "{out:#}");
    assert_eq!(out["refuses_header"], "Refuses", "{out:#}");
    assert_eq!(out["transit_header"], "Changed in transit", "{out:#}");
    let wrap_right = px(&out, "wrap", "right");
    assert!(
        px(&out, "transit", "right") <= wrap_right + 1.0,
        "Changed in transit is past the card: {out:#}"
    );
    let cells = out["transit_cells"].as_array().expect("transit cells");
    assert_eq!(cells.len(), 4, "{out:#}");
    for cell in cells {
        let right = cell["right"].as_f64().expect("a cell edge");
        assert!(
            right <= wrap_right + 1.0,
            "a transit cell is past the card: {out:#}"
        );
    }
    assert!(
        out["wrap_scroll"].as_f64() <= out["wrap_client"].as_f64().map(|c| c + 1.0),
        "the lanes scroll sideways at 1024 px: {out:#}"
    );
    let content = out["refuses_label"].as_f64().expect("label width")
        + out["refuses_padding"].as_f64().expect("padding");
    let own = out["refuses_own"].as_f64().expect("Refuses own width");
    assert!(
        own <= content + 1.0,
        "Refuses asks for more than its header: {out:#}"
    );
    let colors = out["absent_colors"].as_array().expect("absent colors");
    assert_eq!(colors.len(), 8, "{out:#}");
    for color in colors {
        assert_eq!(color, &out["faint"], "{out:#}");
    }
}
