// The Doctor view in a real headless Chrome: the daemon's needs-attention
// list drawn as Now draws it under its own id prefix, its section head
// carrying the one number that is the tab badge's, the checks' all-clear
// shrunk to a line while anything needs attention, the reachability line in
// plain words, finding remediations whose backtick-quoted
// commands are copyable code with the backticks left out, and a
// model-problem number equal to the Models view's cause lines beside a lane
// number equal to the Models tab badge. `include!`d into a
// child module of `page::tests` beside `browser`; the scenarios live in
// page_browser_doctor_driver.js.

use serde_json::{Value, json};

use crate::commands::doctor::SeatFix;
use crate::handlers::status::panel_fixtures::{FIXTURE_NOW_MS, fixture_config};
use crate::handlers::status::ui_case_fixtures::{CaseSource, UiCase};

const DOCTOR_DRIVER: &str = include_str!("page_browser_doctor_driver.js");

const DOCTOR_BUDGET_MS: u64 = 40_000;

fn doctor_scenario(name: &'static str, hash: &'static str, row: Option<Value>) -> Value {
    let snippets: &'static [&'static str] = match row {
        Some(row) => {
            let set = format!("window.routectlDoctorRow = {row};");
            Box::leak(vec![&*Box::leak(set.into_boxed_str()), DOCTOR_DRIVER].into_boxed_slice())
        }
        None => Box::leak(vec![DOCTOR_DRIVER].into_boxed_slice()),
    };
    browser::run_scenario(&browser::Scenario {
        name,
        hash,
        snippets,
        budget_ms: DOCTOR_BUDGET_MS,
    })
    .result
}

fn keys_of(drawn: &Value, prefix: &str) -> Vec<String> {
    drawn
        .as_array()
        .expect("items drawn")
        .iter()
        .map(|item| {
            assert_eq!(item["prefix"], prefix, "{item:#}");
            item["key"].as_str().expect("a key").to_string()
        })
        .collect()
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

/// From one attention payload, Now and Doctor draw the same items in the
/// same order, each under its own id prefix, so no id appears twice on the
/// page. Doctor's one count sits in its "Needs attention" section head with
/// a dot in the worst active severity, and equals its tab badge and the
/// daemon's active tally (broken + degraded); no headline card is drawn
/// above it, and the Fail/Warn/Pass headline is gone while the report's
/// findings still render (positive control). Served an empty panel, the head
/// reads 0 beside a hidden badge, as one line with no "Nothing needs
/// attention now." under it.
///
/// Mutation checks: pass 'now' as the Doctor prefix -> red (duplicate ids);
/// add `counts.recovered` to the head count -> red; pass attentionClear() as
/// the Doctor section's lead -> red (the healthy case is two lines).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_lists_now_items_under_its_own_ids_and_heads_them_with_the_badge() {
    let out = doctor_scenario("doctor-same-list", "now", None);

    let now = keys_of(&out["now"], "now");
    let doctor = keys_of(&out["doctor"], "doctor");
    let served: Vec<String> = out["served_keys"]
        .as_array()
        .expect("served keys")
        .iter()
        .map(|k| id_key(k.as_str().expect("a key")))
        .collect();
    assert!(served.len() > 1, "positive control: several items: {out:#}");
    assert_eq!(doctor, now, "{out:#}");
    assert_eq!(doctor, served, "{out:#}");
    assert_eq!(out["duplicate_ids"], json!([]), "{out:#}");
    let counts = &out["served_counts"];
    let broken = counts["broken"].as_u64().expect("broken");
    let active = broken + counts["degraded"].as_u64().expect("degraded");
    assert!(active > 0, "positive control: active items: {out:#}");
    assert!(
        counts["recovered"].as_u64().expect("recovered") > 0,
        "positive control: a recovered item the count leaves out: {out:#}"
    );
    let head = &out["active"];
    assert_eq!(head["count"], active.to_string(), "{out:#}");
    assert_eq!(head["badge"], head["count"], "{out:#}");
    assert_eq!(head["name"], format!("Needs attention {active}"), "{out:#}");
    let tone = if broken > 0 { "bad" } else { "warn" };
    assert_eq!(head["dot"], format!("docdot docdot--{tone}"), "{out:#}");
    assert_eq!(head["head_lines"], 1, "{out:#}");
    assert_eq!(out["old_headline_unit"], false, "{out:#}");
    let text = out["doctor_text"].as_str().expect("doctor text");
    assert!(!text.contains("item needs attention"), "{out:#}");
    assert!(!text.contains("items need attention"), "{out:#}");
    assert!(
        out["doctor_findings"].as_u64().expect("findings") > 0,
        "positive control: the report's findings still render: {out:#}"
    );

    let healthy = &out["healthy"];
    assert_eq!(healthy["count"], "0", "{out:#}");
    assert_eq!(
        healthy["badge"],
        Value::Null,
        "the badge hides at zero: {out:#}"
    );
    assert_eq!(healthy["name"], "Needs attention 0", "{out:#}");
    assert_eq!(healthy["dot"], "docdot docdot--ok", "{out:#}");
    assert_eq!(healthy["head_lines"], 1, "{out:#}");
    assert_eq!(healthy["clear_line"], 0, "{out:#}");
    assert_eq!(
        out["healthy_text"], "Needs attention 0",
        "one line: {out:#}"
    );
}

/// While the attention list has items and every check passed, the checks'
/// all-clear is one line about Doctor's own checks and no all-clear card is
/// drawn; once nothing needs attention, the full "Every check passed" card
/// is back and the line is gone. The passing checks stay behind their
/// disclosure in both states.
///
/// Mutation check: drop the doctorChecksLine return from doctorAllClear
/// (always draw the card) -> red (the active case draws the card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_shrinks_the_checks_all_clear_while_anything_needs_attention() {
    let out = doctor_scenario("doctor-checks-card", "doctor", None);

    let active = &out["active"];
    assert!(
        active["badge"].as_str().is_some_and(|b| b != "0"),
        "premise: something needs attention: {out:#}"
    );
    assert_eq!(active["findings"], 0, "premise: no failing check: {out:#}");
    assert_eq!(
        active["line"], "Doctor's own checks: none reported a problem (no upstream was dialed).",
        "{out:#}"
    );
    assert_eq!(active["cards"], json!([]), "{out:#}");
    assert_eq!(active["passes"], "1 check passed", "{out:#}");

    let healthy = &out["healthy"];
    assert_eq!(
        healthy["badge"],
        Value::Null,
        "premise: nothing active: {out:#}"
    );
    assert_eq!(healthy["line"], Value::Null, "{out:#}");
    assert_eq!(healthy["cards"], json!(["Every check passed"]), "{out:#}");
    assert_eq!(healthy["passes"], "1 check passed", "{out:#}");
}

fn reach(state_key: &str, verdict: &str) -> Value {
    json!({ "state_key": state_key, "reachability": verdict })
}

/// The reachability line says what it reports in words: routes, read from
/// real traffic, with no test request sent. The `unknown` verdict reads "not
/// used yet" while `reachable` and `degraded`, which have no other word,
/// read as themselves, in the line's fixed order with each count. A verdict
/// this build does not know is not dropped from the count: the doctor
/// section stops drawing and says it could not read the payload.
///
/// Mutation checks: drop the `unknown` entry from REACH_WORDS -> red (the
/// line reads "2 unknown"); make reachWord return '' for a token with no
/// word -> red (reachable and degraded lose their word); let validatedReport
/// accept any reachability token -> red (the unknown verdict is drawn past).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_reachability_line_reads_in_plain_words() {
    let out = doctor_scenario(
        "doctor-reach",
        "doctor",
        Some(json!({
            "reachability": [
                reach("a", "unknown"),
                reach("b", "reachable"),
                reach("c", "unknown"),
                reach("d", "degraded"),
            ],
            "next": [reach("a", "reachable"), reach("b", "rerouted")],
        })),
    );

    assert_eq!(
        out["line"],
        "Routes, from real traffic since the daemon started (Doctor sends no test \
         requests): 1 reachable, 1 degraded, 2 not used yet",
        "{out:#}"
    );
    assert_eq!(out["after_error"], true, "{out:#}");
    let after = out["after"].as_str().expect("section text");
    assert!(!after.contains("Routes, from real traffic"), "{out:#}");
}

fn warn(detail: &str, remediation: &str) -> Value {
    json!({
        "section": "config",
        "name": detail,
        "status": "Warn",
        "detail": detail,
        "remediation": remediation,
    })
}

/// The remediation rows, each taken from the doctor's own sentence
/// builders where one quotes a command: one quoted command, two quoted
/// tokens, a single unbalanced backtick, and none.
fn remediation_rows() -> Vec<Value> {
    let login = SeatFix::of(&fixture_config(), "claude-seat").remediation();
    let unknown = SeatFix::for_oauth_ref("claude-seat", "nosuch".to_string(), None).remediation();
    vec![
        warn("one quoted command", &login),
        warn("two quoted tokens", &unknown),
        warn(
            "unbalanced",
            "run `routectl doctor after editing the config",
        ),
        warn("plain", "remove the old folder"),
    ]
}

fn card<'a>(out: &'a Value, detail: &str) -> &'a Value {
    out["cards"]
        .as_array()
        .expect("cards")
        .iter()
        .find(|c| c["detail"] == detail)
        .unwrap_or_else(|| panic!("no card `{detail}`: {out:#}"))
}

fn served_remediation(out: &Value, detail: &str) -> String {
    out["served"]
        .as_array()
        .expect("served findings")
        .iter()
        .find(|f| f["detail"] == detail)
        .and_then(|f| f["remediation"].as_str())
        .unwrap_or_else(|| panic!("no served `{detail}`: {out:#}"))
        .to_string()
}

/// A quoted command in a remediation is a focusable code element whose text
/// carries no backtick, while the payload's remediation keeps its backticks
/// and the card reads as the sentence with them dropped. A remediation with
/// one unbalanced backtick, or none, is plain text with no code element. Every
/// attention fix on the view is a focusable code element with no backtick.
///
/// Mutation check: draw every remediation as plain text in
/// remediationText -> red (the quoted rows carry no code element).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_remediations_show_quoted_commands_bare() {
    let rows = remediation_rows();
    let out = doctor_scenario(
        "doctor-remediations",
        "doctor",
        Some(json!({ "findings": rows })),
    );

    for (detail, spans) in [("one quoted command", 1), ("two quoted tokens", 2)] {
        let served = served_remediation(&out, detail);
        let shown = card(&out, detail);
        assert!(
            served.contains('`'),
            "the payload keeps its backticks: {served}"
        );
        assert_eq!(shown["text"], served.replace('`', ""), "{shown:#}");
        let quoted: Vec<&str> = served.split('`').skip(1).step_by(2).collect();
        let codes = shown["codes"].as_array().expect("codes");
        assert_eq!(codes.len(), spans, "{shown:#}");
        for (code, want) in codes.iter().zip(&quoted) {
            assert_eq!(code["text"], *want, "{shown:#}");
            assert_eq!(code["tab_index"], 0, "{shown:#}");
        }
    }
    assert!(
        card(&out, "one quoted command")["codes"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("routectl login ")),
        "{out:#}"
    );
    for detail in ["unbalanced", "plain"] {
        let shown = card(&out, detail);
        assert_eq!(shown["text"], served_remediation(&out, detail), "{shown:#}");
        assert_eq!(shown["codes"], json!([]), "{shown:#}");
    }
    let fixes = out["fixes"].as_array().expect("fixes");
    assert!(
        !fixes.is_empty(),
        "positive control: attention fixes drawn: {out:#}"
    );
    for fix in fixes {
        assert_eq!(fix["tag"], "code", "{fix:#}");
        assert_eq!(fix["tab_index"], 0, "{fix:#}");
        assert!(
            !fix["text"].as_str().expect("fix text").contains('`'),
            "{fix:#}"
        );
    }
}

/// On one aggregate fixture the Doctor tab's model-problem number is the
/// number of cause lines the Models view draws, and the lane number beside
/// it is the Models tab badge. The passthrough hop case carries three lanes
/// under one cause, so the two numbers differ there (premise asserted on the
/// served rows), and the line names both units, each in its own number.
///
/// Mutation checks: count rows with a cause_key instead of distinct cause
/// groups in doctorModelProblemsSection -> red (3 against 1); draw the cause
/// count as the lane number -> red (1 against the badge's 3).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_model_problems_equal_the_models_cause_lines_and_badge() {
    let out = browser::run_case_scenario(
        &browser::Scenario {
            name: "doctor-model-problems",
            hash: "models",
            snippets: &[DOCTOR_DRIVER],
            budget_ms: DOCTOR_BUDGET_MS,
        },
        browser::CaseServed {
            source: CaseSource::open(UiCase::PassthroughHop, FIXTURE_NOW_MS, "127.0.0.1:8787"),
            now_ms: Some(FIXTURE_NOW_MS),
        },
    )
    .result;

    let rows = out["served_rows"].as_array().expect("served models rows");
    let with_cause: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["cause_key"].as_str())
        .collect();
    let distinct: std::collections::BTreeSet<&str> = with_cause.iter().copied().collect();
    assert!(
        with_cause.len() > distinct.len(),
        "premise: more problem lanes than causes: {out:#}"
    );
    let lines = out["cause_lines"].as_u64().expect("cause lines");
    assert!(
        lines > 0,
        "positive control: Models draws a cause line: {out:#}"
    );
    assert_eq!(lines, distinct.len() as u64, "{out:#}");
    assert_eq!(out["doctor_count"], lines.to_string(), "{out:#}");
    let not_ok = rows.iter().filter(|row| row["state"] != "ok").count();
    assert!(
        not_ok > distinct.len(),
        "premise: more lanes not ok than causes: {out:#}"
    );
    assert_eq!(out["models_badge"], not_ok.to_string(), "{out:#}");
    assert_eq!(out["doctor_lanes"], out["models_badge"], "{out:#}");
    assert_eq!(
        out["doctor_line"],
        format!("{lines} model problem ({not_ok} lanes) on the Models view - see Models"),
        "{out:#}"
    );
    assert_eq!(out["link_action"], "models", "{out:#}");
}
