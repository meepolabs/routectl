// The verdict strip and the favicon in a real headless Chrome: the strip
// renders the daemon's attention verdict as text with a link to the view it
// names, keeps the last known verdict with its time through an outage, and
// the favicon follows it; the request count beside the span is shown only
// when it covers that span. `include!`d into a child module of `page::tests`;
// the scenarios live in page_browser_verdict_driver.js.

use serde_json::{Value, json};

const VERDICT_DRIVER: &str = include_str!("page_browser_verdict_driver.js");

const VERDICT_BUDGET_MS: u64 = 60_000;

/// Room for the failed rounds' backoff steps and the recovery round.
const OUTAGE_BUDGET_MS: u64 = 150_000;

/// The fill of each favicon, as the driver reads it from the icon's href.
const ICON_OK: &str = "4caf50";
const ICON_WARN: &str = "f0a020";
const ICON_STALE: &str = "9aa5b5";
const ICON_BAD: &str = "d32f2f";

const CHECKING: &str = "Checking routing state";
const UNREACHABLE: &str = "the daemon cannot be reached";
const NOT_CURRENT: &str = "verdict not current";

fn verdict_scenario(name: &'static str, hash: &'static str, row: Option<Value>) -> Value {
    verdict_scenario_for(name, hash, row, VERDICT_BUDGET_MS)
}

fn verdict_scenario_for(
    name: &'static str,
    hash: &'static str,
    row: Option<Value>,
    budget_ms: u64,
) -> Value {
    let snippets: &'static [&'static str] = match row {
        Some(row) => {
            let set = format!("window.routectlVerdictRow = {row};");
            Box::leak(vec![&*Box::leak(set.into_boxed_str()), VERDICT_DRIVER].into_boxed_slice())
        }
        None => Box::leak(vec![VERDICT_DRIVER].into_boxed_slice()),
    };
    browser::run_scenario(&browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms,
    })
    .result
}

fn verdict(severity: &str, text: &str, target: Option<&str>) -> Value {
    json!({ "severity": severity, "text": text, "target": target })
}

fn broken() -> Value {
    verdict(
        "broken",
        "seats: Not logged in, for 2 min, and 1 more",
        Some("accounts"),
    )
}

fn healthy() -> Value {
    verdict("ok", "All routes healthy", None)
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
    let accounts_card = "#body-accounts [data-source=\"accounts\"] .unavailable-msg";
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
            "accounts unavailable",
            "/status",
            Some("accounts"),
            "unavailable",
            "accounts",
            accounts_card,
        ),
        row(
            "accounts invalid",
            "/status",
            Some("accounts"),
            "invalid",
            "accounts",
            accounts_card,
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

/// A doctor, accounts or QUERY answer that is refused or contradicts itself
/// costs only its own sections: the verdict text and the favicon stay as the
/// attention verdict left them. Positive control per row: the source's error
/// card was absent before the break and present after it, so the row really
/// broke a source the strip does not read.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_failing_non_attention_source_leaves_the_verdict_and_favicon() {
    for row in source_rows() {
        let set = json!({
            "path": row.path,
            "panel": row.panel,
            "mode": row.mode,
            "view": row.view,
            "card": row.card,
            "verdict": healthy(),
        });

        let seen = verdict_scenario("verdict-source-fails", row.view, Some(set));

        let name = row.name;
        assert_eq!(seen["control_before"], false, "`{name}`: {seen:#}");
        assert_eq!(seen["control_after"], true, "`{name}`: {seen:#}");
        assert_eq!(
            seen["before"]["text"], "All routes healthy",
            "`{name}`: {seen:#}"
        );
        assert_eq!(seen["before"]["favicon"], ICON_OK, "`{name}`: {seen:#}");
        assert_eq!(
            seen["after"]["text"], "All routes healthy",
            "`{name}`: {seen:#}"
        );
        assert_eq!(seen["after"]["kind"], "ok", "`{name}`: {seen:#}");
        assert_eq!(seen["after"]["dimmed"], false, "`{name}`: {seen:#}");
        assert_eq!(seen["after"]["favicon"], ICON_OK, "`{name}`: {seen:#}");
    }
}

/// Until the first attention payload lands the strip reads the start-up
/// text and the icon is the grey one; the payload then replaces both.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_verdict_is_checking_before_the_first_verdict() {
    let row = json!({ "verdict": broken() });

    let out = verdict_scenario("verdict-before-first", "now", Some(row));

    let held = &out["held"];
    assert_eq!(held["text"], CHECKING, "{out:#}");
    assert_eq!(held["kind"], "idle", "{out:#}");
    assert_eq!(held["favicon"], ICON_STALE, "{out:#}");
    assert_eq!(held["link_children"], 0, "{out:#}");
    assert_eq!(out["answered"]["text"], broken()["text"], "{out:#}");
    assert_eq!(out["answered"]["favicon"], ICON_BAD, "{out:#}");
}

/// One served verdict and what the strip shows for it.
struct LiveRow {
    name: &'static str,
    verdict: Value,
    kind: &'static str,
    favicon: &'static str,
    link: Option<(&'static str, &'static str)>,
}

fn live_rows() -> Vec<LiveRow> {
    let accounts = Some(("see Accounts", "#accounts"));
    vec![
        LiveRow {
            name: "broken, to accounts",
            verdict: broken(),
            kind: "bad",
            favicon: ICON_BAD,
            link: accounts,
        },
        LiveRow {
            name: "degraded, to accounts",
            verdict: verdict(
                "degraded",
                "claude-a (seat-a): Not logged in",
                Some("accounts"),
            ),
            kind: "warn",
            favicon: ICON_WARN,
            link: accounts,
        },
        LiveRow {
            name: "degraded, to models",
            verdict: verdict("degraded", "fast: circuit open", Some("models")),
            kind: "warn",
            favicon: ICON_WARN,
            link: Some(("see Models", "#models")),
        },
        LiveRow {
            name: "ok, no target",
            verdict: healthy(),
            kind: "ok",
            favicon: ICON_OK,
            link: None,
        },
        LiveRow {
            name: "a target naming no view",
            verdict: verdict("degraded", "seats: Not logged in", Some("billing")),
            kind: "warn",
            favicon: ICON_WARN,
            link: None,
        },
        LiveRow {
            name: "markup in the text",
            verdict: verdict(
                "degraded",
                "<b>seats</b> & <img src=x>: Not logged in",
                Some("accounts"),
            ),
            kind: "warn",
            favicon: ICON_WARN,
            link: accounts,
        },
    ]
}

/// The served verdict renders as given: its text verbatim as text (a row
/// carrying markup shows the markup as characters and adds no element), its
/// severity as the strip's tone and the favicon, and its target as a link
/// labelled from the view it names. A target naming no registered view
/// renders no link; the rows with a view are the positive control.
///
/// Mutation checks: map `broken` to the amber icon -> red (broken row);
/// write the text with innerHTML -> red (markup row); link any target
/// string -> red (no-view row).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_served_verdict_renders_as_text_tone_icon_and_link() {
    for row in live_rows() {
        let name = row.name;
        let set = json!({ "verdict": row.verdict });

        let out = verdict_scenario("verdict-live", "now", Some(set));

        assert_eq!(out["text"], row.verdict["text"], "`{name}`: {out:#}");
        assert_eq!(out["marked_up"], 0, "`{name}`: {out:#}");
        assert_eq!(out["kind"], row.kind, "`{name}`: {out:#}");
        assert_eq!(out["favicon"], row.favicon, "`{name}`: {out:#}");
        assert_eq!(out["dimmed"], false, "`{name}`: {out:#}");
        assert_eq!(out["note"], Value::Null, "`{name}`: {out:#}");
        match row.link {
            Some((label, href)) => {
                assert_eq!(out["link_text"], label, "`{name}`: {out:#}");
                assert_eq!(out["link_href"], href, "`{name}`: {out:#}");
            }
            None => assert_eq!(out["link_children"], 0, "`{name}`: {out:#}"),
        }
    }
}

/// A broken verdict is shown, then two aggregate rounds fail as an
/// unreachable daemon's do: after each the strip keeps the same sentence,
/// dimmed, with the verdict's time and that the daemon cannot be reached,
/// and the icon stays red. No sentence it took in between was the start-up
/// text. A valid payload with another verdict then replaces it.
///
/// Mutation check: delete the `lastVerdict = current` assignment in
/// renderVerdict -> red (the strip falls back to the start-up text).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_an_outage_keeps_the_last_verdict_with_its_time() {
    let after = verdict(
        "degraded",
        "claude-a (seat-a): Not logged in",
        Some("accounts"),
    );
    let row = json!({ "verdict": broken(), "failures": 2, "after": after });

    let out = verdict_scenario_for("verdict-outage", "now", Some(row), OUTAGE_BUDGET_MS);

    assert_eq!(out["live"]["text"], broken()["text"], "{out:#}");
    assert_eq!(out["live"]["favicon"], ICON_BAD, "{out:#}");
    let served = out["served_time"].as_str().expect("served time");
    let failed = out["failed"].as_array().expect("failed rounds");
    assert_eq!(failed.len(), 2, "{out:#}");
    for (i, seen) in failed.iter().enumerate() {
        assert_eq!(seen["text"], broken()["text"], "round {i}: {out:#}");
        assert_eq!(seen["kind"], "bad", "round {i}: {out:#}");
        assert_eq!(seen["dimmed"], true, "round {i}: {out:#}");
        assert_eq!(seen["favicon"], ICON_BAD, "round {i}: {out:#}");
        assert_eq!(seen["link_text"], "see Accounts", "round {i}: {out:#}");
        let note = seen["note"].as_str().expect("a note");
        assert!(note.contains(served), "round {i}: {out:#}");
        assert!(note.contains(UNREACHABLE), "round {i}: {out:#}");
    }
    let texts = out["texts"].as_array().expect("recorded texts");
    assert!(
        !texts.iter().any(|t| t == CHECKING),
        "the strip fell back to the start-up text: {out:#}"
    );
    let recovered = &out["recovered"];
    assert_eq!(recovered["text"], after["text"], "{out:#}");
    assert_eq!(recovered["dimmed"], false, "{out:#}");
    assert_eq!(recovered["note"], Value::Null, "{out:#}");
    assert_eq!(recovered["favicon"], ICON_WARN, "{out:#}");
}

/// The daemon answers but the attention panel is refused: the last verdict
/// stays, dimmed, with its time and that the verdict is not current, not
/// that the daemon cannot be reached.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_a_refused_attention_panel_keeps_the_verdict_as_not_current() {
    let row = json!({ "verdict": broken() });

    let out = verdict_scenario("verdict-panel-unavailable", "now", Some(row));

    let refused = &out["refused"];
    assert_eq!(refused["text"], broken()["text"], "{out:#}");
    assert_eq!(refused["favicon"], ICON_BAD, "{out:#}");
    let note = refused["note"].as_str().expect("a note");
    let served = out["served_time"].as_str().expect("served time");
    assert!(note.contains(served), "{out:#}");
    assert!(note.contains(NOT_CURRENT), "{out:#}");
    assert!(!note.contains(UNREACHABLE), "{out:#}");
}

/// Following the "see Accounts" link from Now opens the Accounts view and
/// its address, and the link stays while the verdict does.
///
/// Mutation check: delete the `selectTab(arg)` call in the `open-view`
/// action (its preventDefault kept) -> red (the view never opens).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_verdict_link_opens_its_view() {
    let row = json!({ "verdict": broken() });

    let out = verdict_scenario("verdict-link-opens", "now", Some(row));

    assert_eq!(out["before"]["link_href"], "#accounts", "{out:#}");
    assert_eq!(out["hash"], "#accounts", "{out:#}");
    assert_eq!(out["pane_hidden"], false, "{out:#}");
    assert_eq!(out["after"]["link_text"], "see Accounts", "{out:#}");
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

/// The page derives no verdict of its own: the health-reading verdict and
/// its helpers are gone, and the strip reads the attention verdict.
#[test]
fn the_page_derives_no_verdict_of_its_own() {
    let code = strip_js_comments(&script());

    assert!(
        code.contains("SOURCES.attention"),
        "positive control: the strip reads attention"
    );
    for gone in [
        "seatVerdict",
        "verdictInputsKnown",
        "verdictState",
        "sourceProblems",
    ] {
        assert!(!code.contains(gone), "`{gone}` is back in the script");
    }
}
