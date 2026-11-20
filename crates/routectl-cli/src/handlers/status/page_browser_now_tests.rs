// The Now view in a real headless Chrome. Needs attention: each item's
// cause, raw token and fix drawn as text, the fix a focusable code element
// whose selection survives refreshes, a recovered item grey and outside the
// counts, every cause once, target links to their view, and an attention
// failure confined to its own section. Accounts at a glance: every
// subscription account under its pool, a seat with no reading saying so,
// and meters that turn amber then red with a word. `include!`d into a child
// module of `page::tests` beside `browser`; the scenarios live in
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

/// A doctor failure's remediation is drawn as plain text under the item,
/// with no fix code element: it is an instruction in words, not a line to
/// copy. An account item beside it keeps its fix as a code element
/// (positive control).
///
/// Mutation check: draw the remediation through attentionFix in
/// attentionItem -> red (the remediation is a fix code element and no
/// remediation text is drawn).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_doctor_failure_shows_its_remediation_as_text() {
    let items = fixture_items();
    let result = now_scenario("now-attention-list");
    let rows = drawn_rows(&result);
    let (doctor, row) = items
        .iter()
        .zip(rows)
        .find(|(item, _)| item["kind"] == "doctor_failure")
        .expect("the fixture lists a doctor failure");

    let remediation = str_of(&doctor["remediation"], "the served remediation");
    assert!(doctor["fix"].is_null(), "{doctor:#}");
    assert_eq!(row["remediation"], remediation, "{row:#}");
    assert!(row["fix"].is_null(), "no fix code: {row:#}");
    assert_eq!(row["code_in_remediation"], 0, "{row:#}");
    let (fixed, fixed_row) = items
        .iter()
        .zip(rows)
        .find(|(item, _)| item["fix"].is_string())
        .expect("positive control: an item with a fix");
    assert_eq!(fixed_row["fix"], fixed["fix"], "{fixed_row:#}");
    assert!(fixed_row["remediation"].is_null(), "{fixed_row:#}");
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

// ---- accounts at a glance ---------------------------------------------------

fn fixture_accounts() -> Vec<Value> {
    crate::handlers::status::panel_fixtures::aggregate_json()["panels"]["accounts"]["data"]
        ["accounts"]
        .as_array()
        .expect("the accounts fixture lists accounts")
        .clone()
}

/// The pools the glance should draw from `accounts`, derived the way the
/// page states it: subscription accounts only, named pools by name with
/// their members in panel order, then each pool-less account alone.
fn expected_pools(accounts: &[Value]) -> Vec<(String, Vec<String>)> {
    let mut named: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut solo = Vec::new();
    for row in accounts.iter().filter(|r| r["subscription"] == true) {
        let account = str_of(&row["account"], "an account").to_string();
        let pools = row["pools"].as_array().expect("a pool list");
        if pools.is_empty() {
            solo.push(("no pool".to_string(), vec![account]));
            continue;
        }
        for pool in pools {
            let name = format!("pool {}", str_of(pool, "a pool"));
            named.entry(name).or_default().push(account.clone());
        }
    }
    named.into_iter().chain(solo).collect()
}

fn drawn_pools(result: &Value) -> Vec<(String, Vec<String>)> {
    result["pools"]
        .as_array()
        .expect("pools observed")
        .iter()
        .map(|pool| {
            let seats = pool["seats"]
                .as_array()
                .expect("seats observed")
                .iter()
                .map(|seat| str_of(&seat["account"], "a seat account").to_string())
                .collect();
            (str_of(&pool["name"], "a pool name").to_string(), seats)
        })
        .collect()
}

fn drawn_seat<'a>(result: &'a Value, account: &str) -> &'a Value {
    result["pools"]
        .as_array()
        .expect("pools observed")
        .iter()
        .flat_map(|pool| pool["seats"].as_array().expect("seats observed"))
        .find(|seat| seat["account"] == account)
        .unwrap_or_else(|| panic!("no seat for {account}: {result:#}"))
}

/// Every subscription account in the served panel is drawn under each pool
/// it dispatches in, named "provider_entry (login_label)", with its login
/// state as one word (the cause is the attention item's); Codex, which
/// carries no traffic in the served ledger, is there too. No API-key or
/// cloud account is drawn (positive control: the panel served some). The
/// section links to the Accounts view.
///
/// Mutation check: drop the `row.subscription !== true` filter in
/// glancePools -> red (API-key accounts appear in a pool of their own).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_every_subscription_account_appears_under_its_pool() {
    let accounts = fixture_accounts();
    let result = now_scenario("now-glance-as-served");
    let expected = expected_pools(&accounts);

    assert_eq!(drawn_pools(&result), expected, "{result:#}");
    assert!(
        expected
            .iter()
            .any(|(_, seats)| seats.iter().any(|s| s == "codex")),
        "positive control: the trafficless Codex login is expected"
    );
    let not_subscription: Vec<&str> = accounts
        .iter()
        .filter(|r| r["subscription"] == false)
        .map(|r| str_of(&r["account"], "an account"))
        .collect();
    assert!(
        !not_subscription.is_empty(),
        "positive control: the panel serves API-key and cloud accounts"
    );
    let drawn: Vec<String> = drawn_pools(&result)
        .into_iter()
        .flat_map(|(_, seats)| seats)
        .collect();
    for account in not_subscription {
        assert!(
            !drawn.iter().any(|a| a == account),
            "{account} is not a subscription"
        );
    }
    let seat = drawn_seat(&result, "anthropic#a");
    let served = accounts
        .iter()
        .find(|r| r["account"] == "anthropic#a")
        .expect("the seat row");
    assert_eq!(seat["name"], "claude-seat (a)", "{seat:#}");
    assert_eq!(served["credential"]["state"], "unusable", "{served:#}");
    assert_eq!(seat["login_text"], "unusable", "{seat:#}");
    assert!(
        str_of(&seat["login"], "a login class").contains("glance-login--unusable"),
        "{seat:#}"
    );
    let codex = drawn_seat(&result, "codex");
    assert_eq!(codex["name"], "codex", "an unlabeled login: {codex:#}");
    assert_eq!(codex["no_reading"], "reports no usage windows", "{codex:#}");
    assert_eq!(codex["bars"], 0, "{codex:#}");
    assert_eq!(result["link_href"], "#accounts", "{result:#}");
}

/// A seat the meter has never read says "no reading" and draws no meter at
/// all, in a pool of its own when it is in no pool. Positive control: the
/// read seat beside it draws one bar per window.
///
/// Mutation check: return `glanceMeter({ used: 0, length_secs: 0 }, nowMs)`
/// in place of the no-reading line in glanceReading -> red (a bar appears
/// and the text is gone).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_seat_with_no_reading_says_so_never_a_zero_meter() {
    let result = now_scenario("now-glance-levels");
    let codex = drawn_seat(&result, "codex");
    let seat = drawn_seat(&result, "anthropic#a");

    assert_eq!(codex["no_reading"], "no reading", "{codex:#}");
    assert_eq!(codex["bars"], 0, "{codex:#}");
    assert_eq!(codex["meters"], Value::Array(Vec::new()), "{codex:#}");
    assert!(
        drawn_pools(&result).contains(&("no pool".to_string(), vec!["codex".to_string()])),
        "Codex in no pool is a pool of one: {result:#}"
    );
    assert_eq!(seat["bars"], 2, "positive control: {seat:#}");
    assert_eq!(seat["no_reading"], Value::Null, "{seat:#}");
}

/// A window whose used fraction arrives null draws no meter at all, never a
/// 0% one: the glance shows the invalid-payload card in its own section, and
/// the attention list and the traffic tiles beside it still render (the
/// positive control that the page drew Now from the same answer).
///
/// Mutation check: read `used` through num0 again in glanceMeter, without
/// the shape check -> red (a `0%` meter is drawn and no card).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_glance_window_with_no_used_fraction_is_an_invalid_payload() {
    let result = now_scenario("now-glance-window-unread");

    assert_eq!(result["meters"], Value::Array(Vec::new()), "{result:#}");
    assert!(
        str_of(&result["card_text"], "the card").contains("invalid_payload"),
        "{result:#}"
    );
    assert!(
        result["attention_rows"].as_u64().unwrap_or(0) > 0,
        "the attention list renders: {result:#}"
    );
    assert!(
        result["kpis"].as_u64().unwrap_or(0) > 0,
        "the tiles render: {result:#}"
    );
}

/// A window above 90% used reads amber with "nearly used"; one at its limit
/// reads red with "at limit"; one under it is the calm meter color with no
/// word. A stale reading keeps its level color, is dimmed, and says
/// "stale" beside its age. Each meter shows its percentage and is labelled
/// from its length.
///
/// Mutation checks: drop the `.glance-meter--warn` fill rule -> red (the
/// 95% fill is the calm color); use `>=` 0.9 as the near-full line -> red
/// (the stale 90% window turns amber).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_glance_meters_turn_amber_then_red_with_a_word() {
    let levels = now_scenario("now-glance-levels");
    let served = now_scenario("now-glance-as-served");
    let palette = &levels["palette"];
    let raised = drawn_seat(&levels, "anthropic#a")["meters"]
        .as_array()
        .expect("meters")
        .clone();
    let calm = drawn_seat(&served, "anthropic#a")["meters"]
        .as_array()
        .expect("meters")
        .clone();

    assert_ne!(palette["meter"], palette["meter_warn"], "{palette:#}");
    assert_ne!(palette["meter_warn"], palette["meter_full"], "{palette:#}");
    assert_eq!(raised[0]["pct"], "95%", "{raised:#?}");
    assert_eq!(raised[0]["word"], "nearly used", "{raised:#?}");
    assert_eq!(raised[0]["fill"], palette["meter_warn"], "{raised:#?}");
    assert_eq!(raised[1]["pct"], "100%", "{raised:#?}");
    assert_eq!(raised[1]["word"], "at limit", "{raised:#?}");
    assert_eq!(raised[1]["fill"], palette["meter_full"], "{raised:#?}");
    assert_eq!(calm[0]["label"], "5 h", "{calm:#?}");
    assert_eq!(calm[0]["pct"], "42%", "{calm:#?}");
    assert_eq!(calm[0]["word"], Value::Null, "{calm:#?}");
    assert_eq!(calm[0]["fill"], served["palette"]["meter"], "{calm:#?}");
    assert_eq!(calm[0]["opacity"], "1", "{calm:#?}");
    let stale = &calm[1];
    assert_eq!(stale["label"], "7 d opus", "{stale:#}");
    assert_eq!(stale["pct"], "90%", "{stale:#}");
    assert_eq!(stale["fill"], served["palette"]["meter"], "{stale:#}");
    assert!(
        str_of(&stale["age"], "an age").starts_with("stale, "),
        "{stale:#}"
    );
    assert!(
        str_of(&stale["opacity"], "an opacity")
            .parse::<f64>()
            .is_ok_and(|o| o < 1.0),
        "a stale meter is dimmed: {stale:#}"
    );
}

/// A refresh that moves a window's use moves the same bar to the new value,
/// with its width animated; the figure beside it follows.
///
/// Mutation check: drop the width transition from `.glance-bar-fill` -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_now_a_glance_meter_moves_on_change() {
    let result = now_scenario("now-glance-meter-moves");
    let obs: Vec<&Value> = std::iter::once(&result["baseline"])
        .chain(result["rounds"].as_array().expect("rounds observed"))
        .collect();

    assert_eq!(obs.len(), 3, "{result:#}");
    for (i, o) in obs.iter().enumerate() {
        assert_eq!(o["same_node"], true, "observation {i}: {o:#}");
        assert_eq!(o["connected"], true, "observation {i}: {o:#}");
        assert!(
            str_of(&o["transition"], "a transition").contains("width"),
            "{o:#}"
        );
    }
    for pair in obs.windows(2) {
        assert_ne!(pair[0]["pct"], pair[1]["pct"], "{obs:#?}");
        assert_ne!(pair[0]["figure"], pair[1]["figure"], "{obs:#?}");
    }
}

/// The seat dialog and its modal plumbing are gone from the page, and the
/// Now part reads no quota from the usage panel.
///
/// Mutation check: put `roots.push(el('modal-host'));` back in initActions
/// -> red.
#[test]
fn the_seat_dialog_and_modal_plumbing_are_gone() {
    let code = strip_js_comments(&script());
    let now_part = strip_js_comments(include_str!("dash_60_tab_overview.js"));
    let markup = include_str!("dashboard.html");

    assert!(
        code.contains("accountsGlanceSection") && code.contains("viewSection("),
        "positive control: the scan reads the assembled script"
    );
    for retired in [
        "modal-host",
        "offerOverlay",
        "paintOverlay",
        "seatModal",
        "SeatModal",
        "seatIndex",
        "groupQuotaByProvider",
        "'seat-open'",
        "'seat-close'",
        "'seat-backdrop'",
    ] {
        assert!(!code.contains(retired), "`{retired}` remains in the script");
    }
    assert!(!markup.contains("modal-host"), "the markup keeps the host");
    assert!(
        now_part.contains("rec.data") && !now_part.contains("quota"),
        "the Now part reads no quota"
    );
    assert!(
        !views::view_entry("now")
            .sources
            .iter()
            .any(|s| s == "usage"),
        "Now does not read the usage panel"
    );
}

/// Now composes the glance from the accounts panel, between the attention
/// list and the traffic.
#[test]
fn now_draws_the_glance_from_the_accounts_panel() {
    let body = function_body("buildNow");

    assert_eq!(
        now_sections(&body)
            .iter()
            .filter(|s| *s == "viewSection")
            .count(),
        3,
        "attention, the glance, then traffic: {body}"
    );
    let attention = body
        .find("viewSection('attention'")
        .expect("the attention section");
    let glance = body
        .find("viewSection('accounts', accountsGlanceSection")
        .expect("the glance section reads the accounts panel");
    let traffic = body
        .find("viewSection(QUERY_SERIES_SOURCE")
        .expect("the traffic section");
    assert!(attention < glance && glance < traffic, "{body}");
}
