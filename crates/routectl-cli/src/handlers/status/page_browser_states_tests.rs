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
    assert_eq!(health_sections("accounts"), 2, "pool seats and the driver");
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

/// A panel absent from the answer shows its unavailable card, and a figure
/// absent from a payload reads as a dash; neither becomes a zero.
///
/// Mutation check: make seatCountText answer `0` for an absent count -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_missing_data_reads_as_unavailable_never_zero() {
    let result = states_scenario("states-missing-data", "accounts", 30_000);
    let (present, missing) = (&result["present"], &result["missing"]);

    assert_eq!(present["sweeps"], "12", "positive control: {present:#}");
    assert_eq!(present["first_pool_eligible"], "1 of 3", "{present:#}");
    assert_eq!(present["quota_card"], Value::Null, "{present:#}");

    assert_eq!(missing["sweeps"], "-", "{missing:#}");
    assert_eq!(missing["first_pool_eligible"], "- of 3", "{missing:#}");
    assert_eq!(
        missing["quota_card"],
        "unavailable: missing_panel (this source is not answering right now)",
        "{missing:#}"
    );
    assert_eq!(missing["quota_zero_figures"], 0, "{missing:#}");
}
