// The verdict strip and the favicon in a real headless Chrome: the verdict
// reads routing state from health alone, says it is checking while health is
// not current, and the favicon follows it; the request count beside the span
// is shown only when it covers that span. `include!`d into a child module of
// `page::tests`; the scenarios live in page_browser_verdict_driver.js.

use serde_json::Value;

const VERDICT_DRIVER: &str = include_str!("page_browser_verdict_driver.js");

const VERDICT_BUDGET_MS: u64 = 60_000;

/// The fill of each favicon, as the driver reads it from the icon's href.
const ICON_OK: &str = "4caf50";
const ICON_WARN: &str = "f0a020";
const ICON_STALE: &str = "9aa5b5";

const CHECKING: &str = "Checking routing state";
const HEALTHY: &str = "All routes healthy";

fn verdict_scenario(name: &'static str, hash: &'static str, preface: Option<String>) -> Value {
    let snippets: &'static [&'static str] = match preface {
        Some(set) => {
            Box::leak(vec![&*Box::leak(set.into_boxed_str()), VERDICT_DRIVER].into_boxed_slice())
        }
        None => Box::leak(vec![VERDICT_DRIVER].into_boxed_slice()),
    };
    browser::run_scenario(&browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: VERDICT_BUDGET_MS,
    })
    .result
}

/// One source broken in one way: the path its answer comes on, the panel
/// inside the aggregate (None for its own route), the view it shows on and
/// the error card that proves it broke.
struct SourceRow {
    name: &'static str,
    path: &'static str,
    panel: Option<&'static str>,
    mode: &'static str,
    view: &'static str,
    card: &'static str,
}

fn source_rows() -> Vec<SourceRow> {
    let row = |name, path, panel, mode, view, card| SourceRow {
        name,
        path,
        panel,
        mode,
        view,
        card,
    };
    let doctor_card = "#body-doctor [data-source=\"doctor\"] .unavailable-msg";
    let usage_card = "#body-accounts [data-source=\"usage\"] .unavailable-msg";
    let query_card = "#body-traffic [data-source=\"query\"] .unavailable-msg";
    vec![
        row(
            "doctor unavailable",
            "/status",
            Some("doctor"),
            "unavailable",
            "doctor",
            doctor_card,
        ),
        row(
            "doctor invalid",
            "/status",
            Some("doctor"),
            "invalid",
            "doctor",
            doctor_card,
        ),
        row(
            "usage unavailable",
            "/status",
            Some("usage"),
            "unavailable",
            "accounts",
            usage_card,
        ),
        row(
            "usage invalid",
            "/status",
            Some("usage"),
            "invalid",
            "accounts",
            usage_card,
        ),
        row(
            "query unavailable",
            "/status/query",
            None,
            "unavailable",
            "traffic",
            query_card,
        ),
        row(
            "query invalid",
            "/status/query",
            None,
            "invalid",
            "traffic",
            query_card,
        ),
    ]
}

/// A doctor, usage or QUERY answer that is refused or contradicts itself
/// costs only its own sections: the verdict text and the favicon stay as the
/// healthy routing state left them. Positive control per row: the source's
/// error card was absent before the break and present after it, so the row
/// really broke the source the strip ignores.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_failing_non_routing_source_leaves_the_verdict_and_favicon() {
    for row in source_rows() {
        let set = format!(
            "window.routectlVerdictRow = {};",
            serde_json::json!({
                "path": row.path,
                "panel": row.panel,
                "mode": row.mode,
                "view": row.view,
                "card": row.card,
            })
        );

        let seen = verdict_scenario("verdict-source-fails", row.view, Some(set));

        let name = row.name;
        assert_eq!(seen["control_before"], false, "`{name}`: {seen:#}");
        assert_eq!(seen["control_after"], true, "`{name}`: {seen:#}");
        assert_eq!(seen["before"]["text"], HEALTHY, "`{name}`: {seen:#}");
        assert_eq!(seen["before"]["favicon"], ICON_OK, "`{name}`: {seen:#}");
        assert_eq!(seen["after"]["text"], HEALTHY, "`{name}`: {seen:#}");
        assert_eq!(seen["after"]["kind"], "ok", "`{name}`: {seen:#}");
        assert_eq!(seen["after"]["favicon"], ICON_OK, "`{name}`: {seen:#}");
    }
}

/// Until the first health payload lands the strip is neutral and the icon
/// is the stale one; the payload then moves both.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_verdict_is_checking_before_the_first_health_payload() {
    let out = verdict_scenario("verdict-before-first-health", "now", None);

    let held = &out["held"];
    assert_eq!(held["text"], CHECKING, "{out:#}");
    assert_eq!(held["kind"], "idle", "{out:#}");
    assert_eq!(held["favicon"], ICON_STALE, "{out:#}");
    assert_ne!(
        out["answered"]["text"], CHECKING,
        "positive control: the health payload moves the verdict: {out:#}"
    );
}

/// A pool with no eligible seat is a bad verdict naming the pool, with the
/// warning icon.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_empty_pool_gives_a_bad_verdict_naming_it() {
    let strip = verdict_scenario("verdict-empty-pool", "now", None);

    assert_eq!(
        strip["text"], "No usable seat in seats - see Accounts",
        "{strip:#}"
    );
    assert_eq!(strip["kind"], "bad", "{strip:#}");
    assert_eq!(strip["favicon"], ICON_WARN, "{strip:#}");
}

/// On Traffic at range=month, with the QUERY for the month unanswered, the
/// stats carry the span and no count: today's usage totals do not cover a
/// month. Positive control: once the QUERY answers, its count appears.
///
/// Mutation check: show the today usage count whatever the span -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_request_count_is_shown_only_for_the_span_it_covers() {
    let out = verdict_scenario("verdict-stats-unanswered", "traffic?range=month", None);

    assert_eq!(out["unanswered"]["stats"], "this month", "{out:#}");
    let answered = out["answered"]["stats"].as_str().expect("stats text");
    assert!(answered.ends_with(" req this month"), "{out:#}");
}

/// After a good round, a network failure leaves every source stale with its
/// data kept. Health that is not current says nothing about routing now, so
/// the strip reads the checking arm (never "Status overloaded", never
/// healthy) and the icon goes stale with it. Positive control: the Accounts
/// badge (health is its primary source) reads `stale`, so health kept its
/// data rather than going dead.
///
/// Mutation check: read a stale health or usage source as overloaded in
/// verdictState -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_network_failure_after_a_good_round_reads_checking() {
    let out = verdict_scenario("verdict-network-failure", "now", None);

    assert_eq!(out["good"]["text"], HEALTHY, "{out:#}");
    assert_eq!(out["good"]["favicon"], ICON_OK, "{out:#}");
    assert_eq!(out["health_badge"], "stale", "{out:#}");
    let failed = &out["failed"];
    assert_eq!(failed["text"], CHECKING, "{out:#}");
    assert_eq!(failed["kind"], "idle", "{out:#}");
    assert_eq!(failed["favicon"], ICON_STALE, "{out:#}");
}

/// The verdict's source-problem rollup is gone from the script.
#[test]
fn the_verdict_carries_no_source_problem_rollup() {
    let code = strip_js_comments(&script());

    assert!(code.contains("function verdictState("), "positive control");
    assert!(!code.contains("sourceProblems"), "the rollup is back");
}
