// Panel states in a real headless Chrome: a failed refresh keeps what a
// source rendered, a first failure shows the error card, an invalid payload
// costs only the sections bound to its source, a changed selection is
// pending until its answer applies, and missing data never reads as a zero.
// `include!`d into a child module of `page::tests`; the scenarios live in
// page_browser_states_driver.js.

use serde_json::Value;

const STATES_DRIVER: &str = include_str!("page_browser_states_driver.js");

fn states_scenario(name: &'static str, hash: &'static str, budget_ms: u64) -> Value {
    browser::run_scenario(&browser::Scenario {
        name,
        hash,
        snippets: &[STATES_DRIVER],
        budget_ms,
    })
    .result
}

fn text<'a>(value: &'a Value, what: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{what} is text: {value:#}"))
}

/// After a good round, an unreachable daemon leaves Config's nodes and
/// figures in place, marked stale; the next good round updates them again.
///
/// Mutation check: clear the data of a dead source in markSourceTransport ->
/// red (the section is replaced by an error card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_failed_refresh_keeps_the_rendered_section_marked_stale() {
    let result = states_scenario("states-failure-after-good-round", "config", 60_000);
    let (good, failed, recovered) = (&result["good"], &result["failed"], &result["recovered"]);

    assert_eq!(good["section_stale"], false, "{good:#}");
    assert_eq!(failed["same_toggle"], true, "{failed:#}");
    assert_eq!(failed["same_figure_node"], true, "{failed:#}");
    assert_eq!(failed["connected"], true, "{failed:#}");
    assert_eq!(failed["error_cards"], 0, "{failed:#}");
    assert_eq!(failed["figure"], good["figure"], "the kept figure moved");
    assert_eq!(failed["section_stale"], true, "{failed:#}");
    assert!(
        text(&failed["status"], "the status line").starts_with("stale: last success at "),
        "{failed:#}"
    );
    assert_eq!(recovered["section_stale"], false, "{recovered:#}");
    assert_eq!(recovered["same_toggle"], true, "{recovered:#}");
    assert_ne!(
        recovered["figure"], failed["figure"],
        "positive control: a good round after the failure updates the kept figure"
    );
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_failure_on_the_first_round_shows_the_error_card() {
    let result = states_scenario("states-failure-on-first-round", "config", 20_000);

    assert_eq!(
        result["card"], "unavailable: transport failure",
        "{result:#}"
    );
    assert_eq!(result["error_cards"], 1, "{result:#}");
    assert!(
        text(&result["status"], "the status line").starts_with("no current data: "),
        "{result:#}"
    );
}

/// An invalid health payload puts an error card in every section the host
/// binds to health, in every view that composes one, and in no other
/// section; the aggregate keeps its 5 s cadence through it.
///
/// Mutation check: return `healthTargetSection(SOURCES.health)` from
/// buildModels instead of wrapping it in viewSection -> red (the Models
/// error card sits outside any host section).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_invalid_health_payload_errors_only_its_own_sections() {
    let result = states_scenario("states-invalid-health", "accounts", 60_000);

    for view in ["accounts", "models", "now", "traffic"] {
        let seen = &result["views"][view];
        let sections = seen["sections"].as_array().expect("host sections");
        assert!(
            !sections.is_empty(),
            "`{view}` has no host section: {seen:#}"
        );
        let mut carded = 0;
        for section in sections {
            let source = text(&section["source"], "a section source");
            let cards = section["error_cards"].as_u64().expect("a card count");
            if source == "health" {
                assert_eq!(cards, 1, "`{view}` health section: {seen:#}");
            } else {
                assert_eq!(cards, 0, "`{view}` {source} section errored: {seen:#}");
                assert!(
                    section["text_length"].as_u64().unwrap_or(0) > 0,
                    "`{view}` {source} section rendered nothing: {seen:#}"
                );
            }
            carded += cards;
        }
        assert_eq!(
            seen["error_cards"], carded,
            "`{view}` has an error card outside the host sections: {seen:#}"
        );
    }
    let health_sections = |view: &str| {
        result["views"][view]["sections"]
            .as_array()
            .expect("host sections")
            .iter()
            .filter(|s| s["source"] == "health")
            .count()
    };
    assert_eq!(
        health_sections("accounts"),
        0,
        "Accounts reads health inside"
    );
    assert_eq!(health_sections("models"), 1);
    assert_eq!(health_sections("now"), 0);

    let before = result["rounds_before"].as_u64().expect("rounds before");
    let after = result["rounds_after"].as_u64().expect("rounds after");
    assert!(
        after >= before + 2,
        "the aggregate backed off: {before} -> {after} rounds in 11 s"
    );
    assert!(
        text(&result["poll"], "the poll label").starts_with("live - "),
        "{result:#}"
    );
}

fn assert_pending_without_figures(at: &Value, label: &str) {
    assert_eq!(at["busy"], "true", "{label}: {at:#}");
    assert_eq!(at["pending_class"], true, "{label}: {at:#}");
    assert_eq!(at["week_pressed"], "true", "{label}: {at:#}");
    assert_eq!(
        at["figures"],
        Value::Null,
        "{label}: figures on screen under the new range: {at:#}"
    );
}

fn assert_settled(at: &Value, label: &str) {
    assert_eq!(at["busy"], Value::Null, "{label}: {at:#}");
    assert_eq!(at["pending_class"], false, "{label}: {at:#}");
}

/// A range click with the answer held: the Traffic pane is busy and shows no
/// figures under the new label until the answer applies, then shows the new
/// answer's figures.
///
/// Mutation check: drop the markPending call in applyViewState -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_range_change_is_pending_until_its_answer_applies() {
    let result = states_scenario("states-range-pending-held", "traffic", 30_000);
    let before = &result["before"];

    assert_settled(before, "before the click");
    assert!(before["figures"].is_string(), "{before:#}");
    assert_pending_without_figures(&result["held_early"], "just after the click");
    assert_pending_without_figures(&result["held_late"], "just before the answer");
    let applied = &result["applied"];
    assert_settled(applied, "after the answer");
    assert!(
        applied["figures"].is_string(),
        "positive control: the answer's figures appear once it applies: {applied:#}"
    );
    assert_ne!(applied["figures"], before["figures"], "{result:#}");
}

/// A pending selection whose answer fails is not left busy: it settles into
/// the error state.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_failed_answer_ends_the_pending_state() {
    let result = states_scenario("states-range-pending-fails", "traffic", 30_000);

    assert_pending_without_figures(&result["held_early"], "just after the click");
    let applied = &result["applied"];
    assert_settled(applied, "after the failure");
    assert_eq!(applied["figures"], Value::Null, "{applied:#}");
    assert!(
        applied["error_cards"].as_u64().unwrap_or(0) >= 1,
        "the failure shows its error card: {applied:#}"
    );
}

/// A superseded selection's answer, delivered while the newer one is still
/// held, neither ends the pending state nor puts figures on screen; the newer
/// selection's answer ends it.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_superseded_range_ends_pending_on_the_newer_answer() {
    let result = states_scenario("states-range-pending-superseded", "traffic", 30_000);

    assert_pending_without_figures(&result["held_early"], "just after the click");
    let superseded = &result["after_superseded"];
    assert_eq!(superseded["busy"], "true", "{superseded:#}");
    assert_eq!(superseded["figures"], Value::Null, "{superseded:#}");
    assert_eq!(result["month_pressed"], "true", "{result:#}");
    let applied = &result["applied"];
    assert_settled(applied, "after the newer answer");
    assert!(applied["figures"].is_string(), "{applied:#}");
}

/// A figure absent from a payload reads as a dash, never a zero: the poll
/// driver's sweep count and a pool's usable count, while the rest of the
/// view keeps rendering.
///
/// Mutation check: make acctCountText answer `0` for an absent count -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_missing_data_reads_as_unavailable_never_zero() {
    let result = states_scenario("states-missing-data", "accounts", 30_000);
    let (present, missing) = (&result["present"], &result["missing"]);

    assert_eq!(present["sweeps"], "7", "positive control: {present:#}");
    assert_eq!(present["seats_pool"], "1 of 3 usable", "{present:#}");

    assert_eq!(missing["sweeps"], "-", "{missing:#}");
    assert_eq!(missing["seats_pool"], "- of 3 usable", "{missing:#}");
    assert_eq!(missing["backup_pool"], "1 of 2 usable", "{missing:#}");
    assert_eq!(
        missing["seat_window"], present["seat_window"],
        "{missing:#}"
    );
    assert_eq!(missing["error_cards"], 0, "{missing:#}");
}

/// A health answer with no pool list costs the pool headers' usable counts
/// alone: they say the count is not available, and every account row and
/// window still renders with no error card.
///
/// Mutation check: drop the try/catch in acctPoolCounts so its throw reaches
/// the section -> red (the whole section is an error card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_unavailable_pool_counts_leave_the_account_rows() {
    let result = states_scenario("states-pool-counts-unavailable", "accounts", 30_000);

    assert_eq!(
        result["seats_pool"], "usable count not available",
        "{result:#}"
    );
    assert_eq!(
        result["backup_pool"], "usable count not available",
        "{result:#}"
    );
    assert!(
        result["seat_window"].is_string(),
        "positive control: the seat row still renders its window: {result:#}"
    );
    assert_eq!(result["error_cards"], 0, "{result:#}");
}

/// A grouping changed by the address while Accounts is showing resets the
/// Traffic query too: opening Traffic with the new answer held shows a busy
/// pane and none of the old grouping's cards, then the new grouping's cards
/// once the answer applies. No hidden pane is left busy meanwhile. Now's
/// request does not carry the grouping, so returning to Now with its next
/// answer held still shows the figure it last rendered, in the same node.
///
/// Mutation check: drop the resetMovedQuerySources call in applyViewState ->
/// red (the model cards sit under the alias grouping); drop the active-view
/// arm in paintPending -> red (the hidden Traffic pane is busy); reset every
/// query source in queryInputChanged -> red (Now returns as a skeleton).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_selection_changed_on_another_view_resets_its_query() {
    let result = states_scenario("states-group-changed-on-another-view", "now", 60_000);
    let (before, on_accounts, held, applied) = (
        &result["before"],
        &result["on_accounts"],
        &result["held"],
        &result["applied"],
    );
    let labels = |at: &Value| at["labels"].as_array().expect("card labels").clone();

    assert!(!labels(before).is_empty(), "{before:#}");
    assert_eq!(
        on_accounts["busy"],
        Value::Null,
        "the hidden Traffic pane is marked busy: {on_accounts:#}"
    );
    assert_eq!(on_accounts["accounts_busy"], Value::Null, "{on_accounts:#}");
    assert_eq!(on_accounts["now_busy"], Value::Null, "{on_accounts:#}");
    assert_eq!(held["busy"], "true", "{held:#}");
    assert!(
        labels(held).is_empty(),
        "the old grouping's cards under the new one: {held:#}"
    );
    assert_eq!(held["figures"], Value::Null, "{held:#}");
    assert_settled(applied, "after the alias answer");
    assert!(
        !labels(applied).is_empty(),
        "positive control: the alias cards appear once released: {applied:#}"
    );
    assert_ne!(labels(applied), labels(before), "{result:#}");
    assert_eq!(result["pressed_group"], "Alias", "{result:#}");

    let (now_before, now_returned) = (&result["now_before"], &result["now_returned"]);
    assert!(
        now_before["figure"].is_string(),
        "positive control: Now rendered a figure: {now_before:#}"
    );
    assert_eq!(
        result["series_answers_at_return"], result["series_answers_before_return"],
        "Now's next answer was held: {result:#}"
    );
    assert_eq!(
        now_returned["figure"], now_before["figure"],
        "{now_returned:#}"
    );
    assert_eq!(now_returned["same_node"], true, "{now_returned:#}");
    assert_eq!(now_returned["connected"], true, "{now_returned:#}");
    assert_eq!(now_returned["skeleton"], false, "{now_returned:#}");
    assert_eq!(now_returned["error_cards"], 0, "{now_returned:#}");
    assert_eq!(now_returned["busy"], Value::Null, "{now_returned:#}");
}

/// A query payload missing a required metric, or with a malformed series
/// bucket, puts an error card in the section that reads it; the populated
/// payload before it rendered figures and no error card.
///
/// Mutation check: make isMetricsShape return true for any object -> red
/// (every row renders zeros instead of the card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_malformed_query_payload_is_an_error_never_a_zero() {
    let rows = [
        ("states-query-missing-totals-errors", "traffic"),
        ("states-query-group-missing-a-metric", "traffic"),
        ("states-query-malformed-series-bucket", "now"),
    ];
    for (name, hash) in rows {
        let result = states_scenario(name, hash, 40_000);
        let (present, broken) = (&result["present"], &result["broken"]);

        assert_eq!(present["error_cards"], 0, "`{name}` control: {present:#}");
        assert!(
            present["figures"].as_u64().unwrap_or(0) > 0,
            "`{name}` control renders figures: {present:#}"
        );
        assert_eq!(
            broken["error_card"],
            "unavailable: invalid_payload (this section could not be rendered from the \
             payload it received)",
            "`{name}`: {broken:#}"
        );
        assert_eq!(broken["figures"], 0, "`{name}`: {broken:#}");
    }
}

/// A QUERY that keeps failing while the aggregate rounds keep landing reports
/// its own last success: the stale time on the status line does not advance
/// with the aggregate.
///
/// Mutation check: read the aggregate's lastSuccess in renderSectionStatus
/// -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_stale_query_reports_its_own_last_success() {
    let result = states_scenario("states-query-stale-time", "traffic", 60_000);
    let first = text(&result["first"], "the first stale line");

    assert!(first.starts_with("stale: last success at "), "{result:#}");
    let before = result["rounds_at_first"].as_u64().expect("rounds");
    let later = result["rounds_later"].as_u64().expect("rounds");
    assert!(
        later >= before + 2,
        "positive control: aggregate rounds landed: {result:#}"
    );
    assert_eq!(result["later"], result["first"], "{result:#}");
    assert!(
        result["figures"].is_string(),
        "the kept figures stay: {result:#}"
    );
}

/// After a good round with Now and Traffic populated, the daemon goes
/// unreachable for both the aggregate and the QUERY: the Traffic section
/// keeps its figure node, marked stale with its own status line, the
/// verdict keeps its last sentence and nothing reads 0; once the daemon answers again
/// the query is refetched and the stale mark clears.
///
/// Mutation check: drop the markQuerySourcesTransport call in runRound ->
/// red (the Traffic section is never marked stale).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_unreachable_daemon_marks_the_query_section_stale() {
    let result = states_scenario("states-outage-marks-query-stale", "now", 90_000);
    let (good, down, recovered) = (&result["good"], &result["down"], &result["recovered"]);

    assert_eq!(good["section_stale"], false, "{good:#}");
    assert!(good["figure"].is_string(), "{good:#}");
    assert_ne!(good["verdict"], "Checking routing state", "{good:#}");

    assert_eq!(down["section_stale"], true, "{down:#}");
    assert_eq!(down["same_figure_node"], true, "{down:#}");
    assert_eq!(down["connected"], true, "{down:#}");
    assert_eq!(down["figure"], good["figure"], "the kept figure moved");
    assert_eq!(down["error_cards"], 0, "{down:#}");
    assert_eq!(down["zero_figures"], good["zero_figures"], "{down:#}");
    assert_eq!(down["verdict"], good["verdict"], "{down:#}");
    assert!(
        text(&down["status"], "the status line").starts_with("stale: last success at "),
        "{down:#}"
    );

    assert_eq!(recovered["section_stale"], false, "{recovered:#}");
    assert_eq!(recovered["error_cards"], 0, "{recovered:#}");
    assert_ne!(
        recovered["figure"], down["figure"],
        "positive control: the recovered query brings fresh figures: {recovered:#}"
    );
}
