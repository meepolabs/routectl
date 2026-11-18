// Late and reordered answers in a real headless Chrome. `include!`d into a
// child module of `page::tests`; the scenarios live in
// page_browser_epoch_driver.js. Each answer the page receives is logged with
// the `totals.requests` the server put in it, and the harness raises that
// figure on every QUERY answer, so the verdict's request figure says which
// answer is on screen.

use serde_json::Value;

const EPOCH_DRIVER: &str = include_str!("page_browser_epoch_driver.js");

/// The usage view by its pre-rename token, which opens the view reading the
/// series-less QUERY under either tab naming.
const QUERY_VIEW_HASH: &str = "usage";

fn epoch_scenario(name: &'static str, budget_ms: u64) -> browser::Scenario {
    browser::Scenario {
        name,
        hash: QUERY_VIEW_HASH,
        snippets: &[EPOCH_DRIVER],
        budget_ms,
    }
}

fn is_query(entry: &Value) -> bool {
    entry["method"] == "QUERY"
}

fn query_window(entry: &Value) -> &str {
    entry["body"]["window"]
        .as_str()
        .expect("a QUERY body window")
}

fn answered_requests(entry: &Value) -> u64 {
    entry["requests"]
        .as_u64()
        .unwrap_or_else(|| panic!("the answer carried totals.requests: {entry:#}"))
}

fn delivered_at(entry: &Value) -> f64 {
    entry["delivered_at"]
        .as_f64()
        .unwrap_or_else(|| panic!("the answer was delivered: {entry:#}"))
}

/// The verdict's request figure for an answer of `requests`.
fn stats_for(requests: u64) -> String {
    format!("{requests} req ")
}

/// The answer on screen before the held one was released is the newest
/// request's, and releasing the held one changed nothing.
fn assert_late_answer_dropped(result: &Value, held: &Value, current: &Value) {
    assert_ne!(
        answered_requests(held),
        answered_requests(current),
        "positive control: the held answer differs from the current one, so a repaint would show"
    );
    assert!(
        delivered_at(held) > delivered_at(current),
        "the held answer was released after the current one"
    );
    assert_eq!(
        held["aborted"], true,
        "the page aborted the superseded QUERY"
    );
    let before = result["before"]["stats"].as_str().expect("stats text");
    assert!(
        before.starts_with(&stats_for(answered_requests(current))),
        "the current answer applied first: {before:?} vs {current:#}"
    );
    assert_eq!(
        result["after"], result["before"],
        "the late answer repainted the view"
    );
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_old_range_query_released_late_never_repaints() {
    let report = browser::run_scenario(&epoch_scenario("epoch-old-range-late", 30_000));

    let queries = report.result["queries"].as_array().expect("query log");
    assert_eq!(queries.len(), 2, "one QUERY per range: {queries:#?}");
    let (old, new) = (&queries[0], &queries[1]);
    assert_eq!((query_window(old), query_window(new)), ("today", "week"));

    assert_late_answer_dropped(&report.result, old, new);
}

/// Today, Week, Today: the held first answer asks for the very selection on
/// screen, so only its age tells it apart from the current request.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_older_answer_for_the_same_selection_never_repaints() {
    let report = browser::run_scenario(&epoch_scenario("epoch-same-range-reissued-late", 30_000));

    let queries = report.result["queries"].as_array().expect("query log");
    assert_eq!(queries.len(), 3, "{queries:#?}");
    let windows: Vec<&str> = queries.iter().map(query_window).collect();
    assert_eq!(windows, ["today", "week", "today"]);
    assert_eq!(
        queries[0]["body"], queries[2]["body"],
        "the held and the current request ask for the same selection"
    );

    assert_late_answer_dropped(&report.result, &queries[0], &queries[2]);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_held_query_is_neither_aborted_nor_duplicated_across_get_rounds() {
    let report = browser::run_scenario(&epoch_scenario("epoch-held-query-cadence", 40_000));

    let log = report.result["log"].as_array().expect("request log");
    let held = log
        .iter()
        .find(|e| is_query(e))
        .expect("a QUERY was issued");
    let (issued, answered) = (
        held["issued_at"].as_f64().expect("issued_at"),
        delivered_at(held),
    );
    assert_eq!(held["aborted"], false, "the cadence aborted the held QUERY");
    let queries_while_held = log
        .iter()
        .filter(|e| is_query(e) && e["issued_at"].as_f64() < Some(answered))
        .count();
    assert_eq!(
        queries_while_held, 1,
        "a second QUERY was issued while one was held: {log:#?}"
    );
    let rounds_while_held = log
        .iter()
        .filter(|e| {
            e["path"] == "/status"
                && e["issued_at"].as_f64() > Some(issued)
                && e["delivered_at"].as_f64().is_some_and(|at| at < answered)
        })
        .count();
    assert!(
        rounds_while_held >= 2,
        "at least two GET rounds landed while the QUERY was held: {log:#?}"
    );
    let shown = report.result["shown"]["stats"]
        .as_str()
        .expect("stats text");
    assert!(
        shown.starts_with(&stats_for(answered_requests(held))),
        "the held answer, still current, applied when it landed: {shown:?}"
    );
}

/// Leaving the view drops what its in-flight QUERY still returns: here a
/// refusal of that selection, which would otherwise stop the page from ever
/// asking for it again.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_late_answer_for_a_view_left_behind_is_dropped() {
    let report = browser::run_scenario(&epoch_scenario("epoch-left-view-late-refusal", 30_000));

    let refusal = &report.result["refusal"];
    assert_eq!(refusal["status"], 400);
    assert!(
        delivered_at(refusal) < report.result["returned_at"].as_f64().expect("returned_at"),
        "the refusal landed while the view was left"
    );
    let after = report.result["queries_after_return"]
        .as_array()
        .expect("queries after return");
    assert_eq!(
        after.len(),
        1,
        "returning to the view asked for its selection once: {after:#?}"
    );
    assert_eq!(after[0]["body"], refusal["body"], "the same selection");
    let shown = report.result["shown"]["stats"]
        .as_str()
        .expect("stats text");
    assert!(
        shown.starts_with(&stats_for(answered_requests(&after[0]))),
        "the fresh answer is on screen: {shown:?}"
    );
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_late_aggregate_after_a_query_403_keeps_the_page_terminal() {
    let report = browser::run_scenario(&epoch_scenario("epoch-terminal-late-aggregate", 60_000));

    let result = &report.result;
    let at_terminal = &result["terminal"]["shown"];
    assert_eq!(at_terminal["banner_hidden"], false);
    assert!(
        at_terminal["banner_class"]
            .as_str()
            .is_some_and(|c| c.contains("banner--terminal")),
        "the 403 put up the terminal banner: {at_terminal:#}"
    );

    let late = &result["late_aggregate"];
    assert_eq!(late["status"], 200, "the late aggregate was a good answer");
    assert_eq!(
        late["aborted"], true,
        "going terminal aborted the aggregate"
    );
    assert!(
        delivered_at(late) > result["terminal"]["at"].as_f64().expect("terminal time"),
        "the aggregate landed after the page went terminal"
    );

    assert_eq!(
        &result["shown"], at_terminal,
        "the late aggregate changed the banner, verdict or a view"
    );
    assert_eq!(
        result["requests"], result["terminal"]["requests"],
        "the page issued requests after going terminal: {:#?}",
        result["log"]
    );
}
