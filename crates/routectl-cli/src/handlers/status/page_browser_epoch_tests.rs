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

/// Traffic to Accounts with Traffic's model QUERY still out: Accounts asks
/// for its own seat grouping and draws it, every traffic line it ever shows
/// is either loading or the seat answer's own count, and the late Traffic
/// answer, delivered after the page aborted it, changes nothing on screen.
///
/// Mutation check: list `QUERY_SOURCE` in place of `QUERY_SEATS_SOURCE` in
/// the Accounts entry of VIEWS -> red (Accounts asks for and reads a model
/// payload).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_switching_traffic_to_accounts_never_paints_a_non_seat_payload() {
    let report = browser::run_scenario(&epoch_scenario("epoch-traffic-to-accounts", 30_000));
    let result = &report.result;

    let queries = result["queries"].as_array().expect("query log");
    assert_eq!(queries.len(), 2, "{queries:#?}");
    let (traffic, accounts) = (&queries[0], &queries[1]);
    assert_eq!(traffic["view"], "tab-traffic", "{traffic:#}");
    assert_eq!(traffic["body"]["group_by"], "model", "{traffic:#}");
    assert_eq!(accounts["view"], "tab-accounts", "{accounts:#}");
    assert_eq!(accounts["body"]["group_by"], "seat", "{accounts:#}");
    assert!(
        delivered_at(traffic) > delivered_at(accounts),
        "the Traffic answer landed after the Accounts one"
    );
    assert_eq!(
        traffic["aborted"], true,
        "the page aborted the Traffic QUERY"
    );

    let seat_count = accounts["seat_requests"]["anthropic#a"]
        .as_u64()
        .unwrap_or_else(|| panic!("the seat answer counted the seat: {accounts:#}"));
    let seat_line = format!("{seat_count} req");
    let seen = result["seen"].as_array().expect("seen lines");
    assert!(
        seen.iter().any(|t| *t == seat_line.as_str()),
        "positive control: the seat answer was drawn: {seen:?}"
    );
    for text in seen {
        assert!(
            *text == "loading" || *text == seat_line.as_str(),
            "Accounts showed a line from another payload: {text} (seen {seen:?})"
        );
    }
    let stats = result["before"]["stats"].as_str().expect("stats text");
    assert!(
        stats.starts_with(&stats_for(answered_requests(accounts))),
        "the strip counts the seat answer: {stats:?}"
    );
    assert_ne!(
        answered_requests(traffic),
        answered_requests(accounts),
        "positive control: the Traffic answer differs, so a repaint would show"
    );
    assert_eq!(
        result["after"], result["before"],
        "the late Traffic answer repainted Accounts"
    );
}

/// The Models entry of VIEWS as the page registers it.
const MODELS_ENTRY: &str = "      id: 'models',
      label: 'Models',
      sources: ['models', 'attention', QUERY_MODELS_SOURCE],
      span: 'current state',
      build: buildModels";

/// A windowed view led by a GET source that also lists the per-seat QUERY,
/// standing in for Models so its own minimal section isolates the host.
const SEAT_PROBE_ENTRY: &str = "      id: 'models',
      label: 'Models',
      sources: ['health', QUERY_SEATS_SOURCE],
      span: null,
      build: buildModels";

/// The Models view as the page composes it.
const MODELS_VIEW: &str = "  function buildModels() {
    return viewSection('models', modelsSection, 'Model lanes');
  }";

/// The probe composes its GET section and one line per seat group, each
/// label beside its request count.
const SEAT_PROBE_VIEW: &str = "  function buildModels() {
    var stack = tabStack();
    stack.appendChild(viewSection('health', function (rec) {
      return sectionHead('Health', rec.data.targets.length + ' targets');
    }, 'Health'));
    stack.appendChild(viewSection(QUERY_SEATS_SOURCE, function (rec) {
      var line = document.createElement('p');
      line.id = 'seat-probe';
      line.textContent = rec.data.groups.map(function (g) {
        return g.label + '=' + g.metrics.requests;
      }).join(' ');
      return line;
    }, 'Seats'));
    return stack;
  }";

fn with_seat_probe_view(page: String) -> String {
    for (expected, what) in [(MODELS_ENTRY, "registers"), (MODELS_VIEW, "composes")] {
        assert_eq!(
            page.matches(expected).count(),
            1,
            "the page {what} the Models view as this test expects"
        );
    }
    page.replace(MODELS_ENTRY, SEAT_PROBE_ENTRY)
        .replace(MODELS_VIEW, SEAT_PROBE_VIEW)
}

fn seat_probe_scenario(name: &'static str, budget_ms: u64) -> browser::Scenario {
    browser::Scenario {
        name,
        hash: "models",
        snippets: &[EPOCH_DRIVER],
        budget_ms,
    }
}

fn run_on_seat_probe(name: &'static str, budget_ms: u64) -> Value {
    browser::run_scenario_on_patched_page(
        &seat_probe_scenario(name, budget_ms),
        with_seat_probe_view,
    )
    .result
}

/// A view led by a GET source still issues the QUERY it lists, at seat
/// grain, and draws the seat-grouped answer.
///
/// Mutation check: viewQuerySource reading only the primary -> red (no QUERY
/// is ever issued).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_get_led_view_listing_query_seats_issues_a_seat_query() {
    let result = run_on_seat_probe("epoch-seat-probe-first-query", 30_000);

    let first = &result["queries"][0];
    assert_eq!(
        first["body"],
        serde_json::json!({"window": "today", "group_by": "seat"}),
        "{first:#}"
    );
    assert_eq!(first["view"], "tab-models");
    let probe = result["shown"]["probe"].as_str().expect("the probe line");
    for label in ["anthropic#a=", "anthropic#retired=", "(unattributed)="] {
        assert!(probe.contains(label), "`{label}` drawn: {probe:?}");
    }
    let stats = result["shown"]["stats"].as_str().expect("stats text");
    assert!(
        stats.starts_with(&stats_for(answered_requests(first))),
        "the strip counts the seat answer: {stats:?}"
    );
}

/// Today's seat QUERY is held past a move to Week; Week's answer applies and
/// the late Today answer leaves the view as it was.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_late_seat_answer_for_an_old_range_never_repaints() {
    let result = run_on_seat_probe("epoch-old-range-late", 30_000);

    let queries = result["queries"].as_array().expect("query log");
    assert_eq!(queries.len(), 2, "one QUERY per range: {queries:#?}");
    let (old, new) = (&queries[0], &queries[1]);
    assert_eq!((query_window(old), query_window(new)), ("today", "week"));
    for q in queries {
        assert_eq!(q["body"]["group_by"], "seat", "{q:#}");
    }
    assert!(
        result["before"]["probe"].is_string(),
        "positive control: the probe drew the week answer: {result:#}"
    );

    assert_late_answer_dropped(&result, old, new);
}

/// When a QUERY ended: aborted or delivered, whichever came first.
fn ended_at(entry: &Value) -> f64 {
    entry["aborted_at"]
        .as_f64()
        .or_else(|| entry["delivered_at"].as_f64())
        .unwrap_or_else(|| panic!("the QUERY ended: {entry:#}"))
}

/// Probe view, Traffic, probe view again, then a cadence round, with every
/// QUERY still out when the next move comes: each QUERY is the one the view
/// selected at its issue asks for, and none starts before the previous one
/// ended.
///
/// Mutation check: queryStatus no longer aborting the in-flight QUERY -> red
/// (two QUERYs overlap).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_only_the_active_views_query_runs_and_never_two_at_once() {
    let result = run_on_seat_probe("epoch-seat-probe-switch", 40_000);

    let queries = result["queries"].as_array().expect("query log");
    assert!(queries.len() >= 4, "{queries:#?}");
    for q in queries {
        let expected = match q["view"].as_str() {
            Some("tab-models") => "seat",
            Some("tab-traffic") => "model",
            other => panic!("a QUERY issued on {other:?}: {q:#}"),
        };
        assert_eq!(q["body"]["group_by"], expected, "{q:#}");
    }
    let views: Vec<&str> = queries[..4]
        .iter()
        .map(|q| q["view"].as_str().expect("a view"))
        .collect();
    assert_eq!(
        views,
        ["tab-models", "tab-traffic", "tab-models", "tab-models"]
    );
    for pair in queries[..3].windows(2) {
        let next_issued = pair[1]["issued_at"].as_f64().expect("issued_at");
        assert!(
            pair[0]["delivered_at"]
                .as_f64()
                .is_none_or(|at| at > next_issued),
            "positive control: the move came before the previous answer: {pair:#?}"
        );
    }
    for pair in queries.windows(2) {
        assert!(
            pair[1]["issued_at"].as_f64() >= Some(ended_at(&pair[0])),
            "a QUERY was issued while another was out: {pair:#?}"
        );
    }
}

/// A seat QUERY held past the page going terminal and then delivered changes
/// nothing: the per-seat source stays stopped.
///
/// Mutation check: invalidateEveryFetch skipping the per-seat epoch -> red
/// (the late answer redraws the probe line).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_terminal_mode_drops_a_late_seat_answer() {
    let result = run_on_seat_probe("epoch-seat-probe-terminal", 60_000);

    let held = &result["held"];
    assert_eq!(held["body"]["group_by"], "seat", "{held:#}");
    assert_eq!(
        held["aborted"], true,
        "going terminal aborted the seat QUERY"
    );
    assert!(
        delivered_at(held) > result["terminal"]["at"].as_f64().expect("terminal time"),
        "the seat answer landed after the page went terminal"
    );
    assert!(
        held["requests"].as_u64().is_some(),
        "positive control: the late answer carried figures: {held:#}"
    );
    assert_eq!(
        result["terminal"]["query"]["probe"],
        Value::Null,
        "terminal stopped the per-seat section: {result:#}"
    );
    assert_eq!(result["query"], result["terminal"]["query"]);
    assert_eq!(result["shown"], result["terminal"]["shown"]);
}
