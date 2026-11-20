// The Now needs-attention section in a real headless Chrome: each item's
// cause, raw token and fix drawn as text, the fix a focusable code element
// whose selection survives refreshes, a recovered item grey and outside the
// counts, every cause once, target links to their view, and an attention
// failure confined to its own section. `include!`d into a child module of
// `page::tests` beside `browser`; the scenarios live in
// page_browser_now_driver.js.

use serde_json::Value;

const NOW_DRIVER: &str = include_str!("page_browser_now_driver.js");

const NOW_BUDGET_MS: u64 = 40_000;

fn now_scenario(name: &'static str) -> Value {
    browser::run_scenario(&browser::Scenario {
        name,
        hash: "now",
        snippets: &[NOW_DRIVER],
        budget_ms: NOW_BUDGET_MS,
    })
    .result
}

fn fixture_attention() -> Value {
    crate::handlers::status::panel_fixtures::aggregate_json()["panels"]["attention"].clone()
}

fn fixture_items() -> Vec<Value> {
    fixture_attention()["data"]["items"]
        .as_array()
        .expect("the attention fixture lists items")
        .clone()
}

fn drawn_rows(result: &Value) -> &Vec<Value> {
    result["rows"].as_array().expect("rows observed")
}

fn str_of<'a>(value: &'a Value, what: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{what} is text: {value:#}"))
}

/// A selection over a fix command holds across two refreshes that each
/// served a different fix for that item (positive control: the server's fix
/// moved, a sibling cause repainted), catches up once released, and the
/// code element is a focusable `<code>` the keyboard reaches.
///
/// Mutation check: drop the markRefreshUnit call on attentionItem -> red
/// (the held text is replaced by the next round's fix).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_selected_fix_command_survives_refreshes_and_takes_focus() {
    let result = now_scenario("now-fix-selection");
    let obs: Vec<&Value> = std::iter::once(&result["baseline"])
        .chain(result["rounds"].as_array().expect("rounds observed"))
        .collect();
    assert_eq!(obs.len(), 3, "{result:#}");
    let picked = str_of(&obs[0]["picked"], "the selected fix");

    assert!(!picked.is_empty(), "the scenario selected the fix");
    for (i, o) in obs.iter().enumerate() {
        for flag in ["same_text", "same_anchor", "connected"] {
            assert_eq!(o[flag], true, "`{flag}` broke at observation {i}: {o:#}");
        }
        assert_eq!(o["shown_fix"], picked, "the held fix changed at {i}: {o:#}");
    }
    for (i, o) in obs.iter().enumerate().skip(1) {
        assert_ne!(
            o["served_fix"], o["shown_fix"],
            "positive control: round {i} served another fix: {o:#}"
        );
    }
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "positive control: a sibling item repainted: {obs:#?}"
        );
    }
    let released = &result["released"];
    assert_eq!(
        released["shown_fix"], released["served_fix"],
        "{released:#}"
    );
    let focus = &result["focus"];
    assert_eq!(focus["tag"], "code", "{focus:#}");
    assert_eq!(focus["tab_index"], 0, "{focus:#}");
    assert_eq!(focus["focused"], true, "{focus:#}");
}

/// Every fixture item is drawn once, in the panel's order, with its subject,
/// cause, raw token and fix as the daemon sent them, and a link to its view
/// that opens it. Each cause appears in the Now body exactly once.
///
/// Mutation check: append the cause to the head as well as the cause line
/// in attentionItem -> red (the cause appears twice).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_each_fixture_cause_renders_once_with_its_token_fix_and_link() {
    let items = fixture_items();
    let result = now_scenario("now-attention-list");
    let rows = drawn_rows(&result);
    let body = str_of(&result["body_text"], "the Now body text");

    assert_eq!(rows.len(), items.len(), "{result:#}");
    for (item, row) in items.iter().zip(rows) {
        let cause = str_of(&item["cause"], "a cause");
        assert_eq!(row["cause"], item["cause"], "{row:#}");
        assert_eq!(row["subject"], item["subject"], "{row:#}");
        assert_eq!(row["raw"], item["raw_token"], "{row:#}");
        assert_eq!(row["fix"], item["fix"], "{row:#}");
        assert_eq!(
            body.matches(cause).count(),
            1,
            "cause `{cause}` is not drawn exactly once"
        );
        let target = str_of(&item["target"], "a target");
        assert_eq!(row["href"], format!("#{target}"), "{row:#}");
        assert!(
            str_of(&row["link_text"], "the link text").starts_with("see "),
            "{row:#}"
        );
    }
    let after = &result["after_click"];
    assert_eq!(after["hash"], result["clicked_href"], "{result:#}");
    assert_eq!(
        after["pane_shown"], true,
        "the link opened its view: {after:#}"
    );
    assert!(
        items.iter().any(|item| item["fix"].is_null())
            && items.iter().any(|i| i["fix"].is_string()),
        "positive control: the fixture has items with and without a fix"
    );
}

/// The recovered item renders grey with "recovered N min ago" read against
/// the panel's as_of, and the section counts are the daemon's active counts:
/// the recovered broken pool is not among them although an active broken
/// pool is.
///
/// Mutation checks: tally broken from every item in attentionSection
/// (recovered included) -> red ("2 broken"); drop the recovered cause rule
/// from dash_components.css -> red (the cause keeps the text color).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_recovered_item_is_grey_and_not_counted() {
    let counts = &fixture_attention()["data"]["counts"];
    let result = now_scenario("now-attention-list");
    let rows = drawn_rows(&result);
    let recovered: Vec<&Value> = rows
        .iter()
        .filter(|r| str_of(&r["class_name"], "a class").contains("attn-item--recovered"))
        .collect();
    let active_broken = rows
        .iter()
        .filter(|r| str_of(&r["class_name"], "a class").contains("attn-item--broken"))
        .count();

    assert_eq!(recovered.len(), 1, "{result:#}");
    let row = recovered[0];
    assert_eq!(row["severity"], "recovered", "{row:#}");
    assert_eq!(row["recovered"], "recovered 7 min ago", "{row:#}");
    assert_eq!(row["cause_color"], result["faint"], "{row:#}");
    let active = rows
        .iter()
        .find(|r| r["recovered"].is_null())
        .expect("an active row");
    assert_eq!(
        active["cause_color"], result["fg"],
        "positive control: an active cause is not grey"
    );
    assert_eq!(active_broken, 1, "positive control: one active broken item");
    assert_eq!(counts["recovered"], 1, "the fixture recovered one item");
    let expected = vec![
        format!("{} broken", counts["broken"]),
        format!("{} degraded", counts["degraded"]),
    ];
    assert_eq!(result["counts"], Value::from(expected), "{result:#}");
}

/// Daemon strings that look like markup are drawn verbatim as text.
///
/// Mutation check: parse the value as markup in attentionText (a template
/// element's parsed content appended) -> red (the cause reads "bold & ").
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_attention_strings_are_drawn_as_text() {
    let result = now_scenario("now-attention-markup");

    for field in ["cause", "subject", "raw"] {
        assert_eq!(result[field], result["markup"], "{field}: {result:#}");
    }
    assert_eq!(result["b_elements"], 0, "{result:#}");
    assert_eq!(result["img_elements"], 0, "{result:#}");
}

/// An attention panel that answers unavailable shows its error card in the
/// attention section only, titled, and the traffic tiles and provider cards
/// still render.
///
/// Mutation check: build the attention section outside viewSection (call
/// attentionSection(SOURCES.attention) directly) -> red (no titled card in
/// the attention section; the whole view becomes the last-resort card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_an_attention_failure_cards_its_section_and_keeps_the_tiles() {
    let result = now_scenario("now-attention-unavailable");

    assert_eq!(result["error_cards"], 1, "{result:#}");
    assert_eq!(result["cards_in_attention"], 1, "{result:#}");
    assert!(
        str_of(&result["card_text"], "the card").contains("attention_failed"),
        "{result:#}"
    );
    assert_eq!(result["attention_title"], "Needs attention", "{result:#}");
    assert_eq!(result["attention_rows"], 0, "{result:#}");
    assert!(
        result["kpis"].as_u64().unwrap_or(0) > 0,
        "the tiles render: {result:#}"
    );
    assert!(
        result["providers"].as_u64().unwrap_or(0) > 0,
        "the provider cards render: {result:#}"
    );
}

/// The arguments Now's builder hands to `stack.appendChild`, in order.
fn now_sections(body: &str) -> Vec<String> {
    body.split("stack.appendChild(")
        .skip(1)
        .map(|rest| {
            rest.split('(')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .collect()
}

/// Every Now section goes through viewSection; the only other node is the
/// scope reset, a control composed beside the sections.
///
/// Mutation check: append `attentionSection(SOURCES.attention)` directly in
/// buildNow -> red.
#[test]
fn every_now_section_goes_through_view_section() {
    let sections = now_sections(&function_body("buildNow"));

    assert!(
        sections.iter().filter(|s| *s == "viewSection").count() >= 2,
        "positive control: Now composes its sections: {sections:?}"
    );
    for section in &sections {
        assert!(
            section == "viewSection" || section == "scopeRecovery",
            "Now appends `{section}` outside viewSection"
        );
    }
    assert_eq!(
        now_sections("stack.appendChild(viewSection(a)); stack.appendChild(attentionSection(r));"),
        ["viewSection", "attentionSection"],
        "the scan reads each appended builder"
    );
}

/// Now registers its sources with the attention list first.
#[test]
fn now_reads_attention_accounts_the_series_query_and_config() {
    assert_eq!(
        views::view_entry("now").sources,
        ["attention", "accounts", "QUERY_SERIES_SOURCE", "config"]
    );
}
