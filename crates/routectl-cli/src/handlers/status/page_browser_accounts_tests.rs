// The Accounts view in a real headless Chrome: one row per configured
// account under each pool that dispatches on it, the login state with the
// daemon's cause and a focusable fix (or a correction in plain words),
// every window labelled from its length with room, reset, age, source and
// stale or kept mark, each seat's placement rank and reason, the traffic
// over the selected range, the poll state, and the two background drivers. `include!`d into a child module of
// `page::tests`; the scenarios live in page_browser_accounts_driver.js.

use serde_json::Value;

const ACCOUNTS_DRIVER: &str = include_str!("page_browser_accounts_driver.js");

const ACCOUNTS_BUDGET_MS: u64 = 30_000;

fn accounts_scenario(name: &'static str) -> Value {
    accounts_scenario_at(name, "accounts").result
}

fn accounts_scenario_at(name: &'static str, hash: &'static str) -> browser::DriverReport {
    browser::run_scenario(&browser::Scenario {
        name,
        hash,
        snippets: &[ACCOUNTS_DRIVER],
        budget_ms: ACCOUNTS_BUDGET_MS,
    })
}

fn text<'a>(value: &'a Value, what: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{what} is text: {value:#}"))
}

/// The fixture's served row for `account`.
fn fixture_row(account: &str) -> Value {
    let panel = crate::handlers::status::accounts::fixture::populated_accounts();
    panel["data"]["accounts"]
        .as_array()
        .expect("the fixture lists accounts")
        .iter()
        .find(|row| row["account"] == account)
        .unwrap_or_else(|| panic!("the fixture has no `{account}` row"))
        .clone()
}

/// The fixture's rows grouped as the page groups them: each pool sorted,
/// with its usable count from the health fixture, then the rows in no pool.
/// A seat in two pools is a row in each; the names read
/// "provider_entry (login_label)".
///
/// Mutation check: name a row by `row.provider_entry` alone in acctHead ->
/// red (the labeled seat reads `claude-seat`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_lists_every_credential_by_pool() {
    let out = accounts_scenario("accounts-rows");

    assert_eq!(out["error_cards"], 0, "{out:#}");
    assert_eq!(
        out["pools"],
        serde_json::json!(["backup", "seats", "Not in a pool"]),
        "{out:#}"
    );
    assert_eq!(
        out["pool_hints"],
        serde_json::json!(["1 of 2 usable", "1 of 3 usable"]),
        "{out:#}"
    );
    assert_eq!(
        out["names"],
        serde_json::json!([
            "claude-seat (a)",
            "claude-seat (a)",
            "codex",
            "anthropic",
            "bed",
            "compat"
        ]),
        "{out:#}"
    );
    assert_eq!(out["seat_in_both_pools"], true, "{out:#}");
}

/// A logged-out seat says so in plain words with the daemon's cause, and its
/// fix is the daemon's command in a focusable code element; a parked login
/// shows the daemon's park cause, and an unbuilt cloud entry says it has no
/// router state rather than nothing.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_row_shows_the_login_cause_and_a_focusable_fix() {
    let seat = fixture_row("anthropic#a");
    let codex = fixture_row("codex");

    let out = accounts_scenario("accounts-rows");

    assert_eq!(out["seat_login"], "unusable", "{out:#}");
    let cause = text(&out["seat_cause"], "the seat cause");
    assert!(
        cause.starts_with(text(&seat["credential"]["cause"], "the served cause")),
        "{out:#}"
    );
    assert!(cause.contains(" - since "), "{out:#}");
    assert_eq!(out["seat_fix"]["tag"], "code", "{out:#}");
    assert_eq!(out["seat_fix"]["tab_index"], 0, "{out:#}");
    assert_eq!(
        out["seat_fix"]["text"], seat["credential"]["fix"],
        "{out:#}"
    );
    assert_eq!(out["codex_login"], "parked", "{out:#}");
    assert_eq!(out["codex_cause"], codex["credential"]["cause"], "{out:#}");
    assert_eq!(out["api_key_login"], "usable", "{out:#}");
    assert_eq!(
        out["cloud_cause"], "its provider did not build, so the router has no state for it",
        "{out:#}"
    );
}

/// A login that is not usable reads red when no login in its pool is usable
/// (the fixture seat: logged out, its pool siblings parked or absent), and
/// amber when it is in no pool (the unreadable compat key); a usable login
/// reads green.
///
/// Mutation check: return `blocked` for every unusable login in loginKind
/// -> red (the seat is amber).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_an_unusable_login_is_red_only_when_its_pool_has_none() {
    let out = accounts_scenario("accounts-rows");

    let class = |key: &str| {
        text(&out[key], key)
            .split(' ')
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert!(
        class("seat_login_class").contains(&"acct-login--broken".to_owned()),
        "{out:#}"
    );
    assert!(
        class("seat_row_class").contains(&"acct-row--broken".to_owned()),
        "{out:#}"
    );
    assert!(
        class("compat_login_class").contains(&"acct-login--blocked".to_owned()),
        "{out:#}"
    );
    assert!(
        class("api_key_login_class").contains(&"acct-login--usable".to_owned()),
        "{out:#}"
    );
}

/// Ctrl+A pressed on the logged-out seat's focused fix command selects the
/// served command alone, and a click selects it whole; on the focused pane
/// the page leaves the key alone (control).
///
/// Mutation check: drop the keydown listener in initActions -> red (the
/// selection is empty).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_ctrl_a_on_a_focused_fix_selects_only_the_command() {
    let seat = fixture_row("anthropic#a");

    let out = accounts_scenario("accounts-fix-select-all");

    assert_eq!(out["fix_text"], seat["credential"]["fix"], "{out:#}");
    assert_eq!(out["fix"]["focused"], true, "{out:#}");
    assert_eq!(out["fix"]["prevented"], true, "{out:#}");
    assert_eq!(
        out["fix"]["selection"], seat["credential"]["fix"],
        "{out:#}"
    );
    assert_eq!(out["user_select"], "all", "{out:#}");
    assert_eq!(out["pane"]["focused"], true, "{out:#}");
    assert_eq!(out["pane"]["prevented"], false, "{out:#}");
    assert_eq!(out["pane"]["selection"], "", "{out:#}");
}

/// An unusable API-key row, which no single command readmits, shows the
/// daemon's correction as plain text and no fix code element; the
/// logged-out seat beside it keeps its login command as fix code and draws
/// no remediation (positive control).
///
/// Mutation check: draw `cred.remediation` through acctFix in acctRow ->
/// red (the key row has a fix code element and no remediation text).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_a_correction_is_plain_text_not_a_fix_command() {
    let compat = fixture_row("#compat");
    let seat = fixture_row("anthropic#a");

    let out = accounts_scenario("accounts-rows");

    assert_eq!(compat["credential"]["state"], "unusable", "{compat:#}");
    assert!(compat["credential"]["fix"].is_null(), "{compat:#}");
    let remediation = text(
        &compat["credential"]["remediation"],
        "the served remediation",
    );
    assert_eq!(out["compat_remediation"], remediation, "{out:#}");
    assert_eq!(out["compat_fix_codes"], 0, "{out:#}");
    assert_eq!(out["compat_codes"], 0, "{out:#}");
    assert!(seat["credential"]["remediation"].is_null(), "{seat:#}");
    assert_eq!(out["seat_fix"]["tag"], "code", "{out:#}");
    assert_eq!(
        out["seat_fix"]["text"], seat["credential"]["fix"],
        "{out:#}"
    );
    assert!(out["seat_remediation"].is_null(), "{out:#}");
}

/// An API-key and a cloud row say the provider reports no usage windows; a
/// subscription login whose reading carried none says that instead, so the
/// API-key words are not the fallback for every empty row.
///
/// Mutation check: drop the `subscription` test in acctNoWindowText -> red
/// (the Codex row reads as an API-key row).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_api_key_row_reports_no_usage_windows() {
    let out = accounts_scenario("accounts-rows");

    let words = "this provider reports no usage windows";
    assert_eq!(out["api_key_nowin"], words, "{out:#}");
    assert_eq!(out["cloud_nowin"], words, "{out:#}");
    assert_eq!(
        out["codex_nowin"], "its last reading carried no usage windows",
        "positive control: a subscription row with no windows reads differently: {out:#}"
    );
}

/// The poll line: a polled seat names its last result and its failing run,
/// `off` reads "usage poll off", and a token this build does not know is
/// shown as sent, all in the same neutral color as a row the poll skips.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_poll_state_reads_neutrally_for_every_token() {
    let out = accounts_scenario("accounts-rows");

    let seat_poll = text(&out["seat_poll"], "the seat poll line");
    assert!(
        seat_poll.starts_with(
            "usage poll: last attempt failed (timed out) 1 min ago - failing since 2 min ago"
        ),
        "{out:#}"
    );
    assert_eq!(out["compat_poll"], "usage poll off", "{out:#}");
    assert_eq!(out["cloud_poll"], "usage poll: not_yet_named", "{out:#}");
    let colors = out["poll_colors"].as_array().expect("poll colors");
    assert_eq!(colors.len(), 3, "{out:#}");
    assert!(
        colors.iter().all(|c| *c == colors[2]),
        "an unknown or off poll token is colored apart from a skipped row: {out:#}"
    );
}

/// The last poll result reads in words with the raw token in the line's
/// title; a result token this build does not know is shown as sent, also
/// with its title.
///
/// Mutation check: print `poll.last_result` as sent in acctPollText -> red
/// (the seat line reads "last timeout").
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_poll_result_reads_in_words_with_the_token_kept() {
    let out = accounts_scenario("accounts-poll-result");
    let (seat, codex) = (&out["seat"], &out["codex"]);

    assert!(
        text(&seat["text"], "the seat line")
            .starts_with("usage poll: last attempt failed (timed out) 1 min ago"),
        "{out:#}"
    );
    assert_eq!(seat["title"], "last result: timeout", "{out:#}");
    assert!(
        text(&codex["text"], "the Codex line")
            .starts_with("usage poll: last not_yet_named 1 min ago"),
        "{out:#}"
    );
    assert_eq!(codex["title"], "last result: not_yet_named", "{out:#}");
}

/// An Accounts row and its meter are the same DOM nodes across two changed
/// rounds, and the meter's figure shows each round's value.
///
/// Mutation check: replace the Idiomorph call in morphPanel with
/// `parent.replaceChildren(node)` -> red (a new row node every round).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_row_is_the_same_node_across_a_refresh() {
    let out = accounts_scenario("accounts-row-kept");
    let rounds = out["rounds"].as_array().expect("rounds observed");
    assert_eq!(rounds.len(), 2, "{out:#}");
    let obs: Vec<&Value> = std::iter::once(&out["baseline"]).chain(rounds).collect();

    for (i, o) in obs.iter().enumerate() {
        assert_eq!(o["same_node"], true, "observation {i}: {o:#}");
        assert_eq!(o["same_meter"], true, "observation {i}: {o:#}");
        assert_eq!(o["connected"], true, "observation {i}: {o:#}");
    }
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "positive control: the round repainted the figure: {obs:#?}"
        );
    }
}

/// A Codex window of 18000 s is labelled `5 h`, never "weekly"; each window
/// is named from its own length.
///
/// Mutation check: label every window `weekly` in acctWindowLabel -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_codex_five_hour_window_is_labelled_from_its_length() {
    let out = accounts_scenario("accounts-windows");

    assert_eq!(out["five_hour"]["label"], "5 h", "{out:#}");
    assert_eq!(out["week"]["label"], "7 days", "{out:#}");
    assert_eq!(out["month"]["label"], "30 days", "{out:#}");
    assert!(
        !text(&out["codex_text"], "the Codex row").contains("weekly"),
        "{out:#}"
    );
}

/// A meter is calm, then amber with "nearly used" past 90%, then red with
/// "at limit"; the number and room always show beside the color.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_meter_names_its_level_beside_its_color() {
    let out = accounts_scenario("accounts-windows");
    let (calm, near, full) = (&out["five_hour"], &out["week"], &out["month"]);

    assert_eq!(calm["used"], "50% used", "{out:#}");
    assert_eq!(calm["room"], "50% left", "{out:#}");
    assert_eq!(calm["level"], Value::Null, "{out:#}");
    assert_eq!(near["used"], "95% used", "{out:#}");
    assert_eq!(near["level"], "nearly used", "{out:#}");
    assert_eq!(full["used"], "100% used", "{out:#}");
    assert_eq!(full["level"], "at limit", "{out:#}");
    assert_ne!(calm["fill"], near["fill"], "{out:#}");
    assert_ne!(near["fill"], full["fill"], "{out:#}");
    assert_ne!(calm["fill"], full["fill"], "{out:#}");
}

/// A fresh window resets "in" a relative time and names its age and source;
/// a stale one shows its age, the stale mark and no room, its text muted at
/// full opacity rather than recolored or faded.
///
/// Mutation checks: drop the stale mark in acctWindowMeta -> red; restore
/// `.acct-win--stale { opacity: 0.6; }` -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_stale_window_shows_its_age_and_mark() {
    let out = accounts_scenario("accounts-windows");
    let (fresh, stale) = (&out["polled"], &out["stale"]);

    assert_eq!(
        fresh["meta"], "resets in 1 h - read 1 min ago - from the usage poll",
        "{out:#}"
    );
    assert_eq!(fresh["stale_mark"], Value::Null, "{out:#}");
    assert_eq!(fresh["opacity"], "1", "{out:#}");
    assert_eq!(stale["label"], "7 days - opus", "{out:#}");
    assert!(
        text(&stale["meta"], "the stale meta")
            .starts_with("reset time not reported - read 3 h ago - from a response"),
        "{out:#}"
    );
    assert_eq!(stale["stale_mark"], "stale", "{out:#}");
    assert_eq!(stale["room"], "room unknown", "{out:#}");
    assert_eq!(stale["opacity"], "1", "stale text is not dimmed: {out:#}");
    assert_eq!(
        stale["used_color"], stale["label_color"],
        "a stale figure takes the muted label color: {out:#}"
    );
    assert_ne!(
        fresh["used_color"], fresh["label_color"],
        "control: a fresh figure is brighter than its label: {out:#}"
    );
    assert_eq!(stale["level"], Value::Null, "{out:#}");
}

/// A stopped driver reads "stopped" in amber while a running one reads
/// "running" in another color.
///
/// Mutation check: drop the `acct-driver--stopped` class in acctDriverCard
/// -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_stopped_driver_reads_amber() {
    let out = accounts_scenario("accounts-drivers");
    let (running, stopped) = (&out["refresh"], &out["poll"]);

    assert_eq!(running["state"], "running", "{out:#}");
    assert_eq!(running["stopped_class"], false, "{out:#}");
    assert_eq!(stopped["state"], "stopped", "{out:#}");
    assert_eq!(stopped["stopped_class"], true, "{out:#}");
    assert_ne!(running["state_color"], stopped["state_color"], "{out:#}");
    assert_ne!(running["border"], stopped["border"], "{out:#}");
}

/// "<n> req" for an answer's request count, as a traffic line shows it.
fn req_text(answer: &Value) -> String {
    format!(
        "{} req",
        answer
            .as_u64()
            .unwrap_or_else(|| panic!("a request count: {answer}"))
    )
}

/// Each row's traffic comes from the seat group whose account is exactly
/// the row's key: a group for a login not in config (`anthropic#retired`)
/// and one whose key differs from a configured account only in case
/// (`CODEX`) create no row and land on none, and the same seat listed under
/// two pools shows one count under each.
///
/// Mutation check: key the join on `g.account.toLowerCase()` in
/// acctSeatTraffic -> red (the Codex row shows the `CODEX` group's count).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_traffic_joins_seat_groups_on_the_exact_account_key() {
    let out = accounts_scenario("accounts-traffic");

    let answered: Vec<&str> = out["answered_accounts"]
        .as_array()
        .expect("the answer's accounts")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for decoy in ["anthropic#retired", "CODEX"] {
        assert!(
            answered.contains(&decoy),
            "positive control: the answer carried `{decoy}`: {out:#}"
        );
    }
    let rows = out["row_accounts"].as_array().expect("row accounts");
    assert_eq!(rows.len(), 6, "{out:#}");
    for decoy in ["anthropic#retired", "CODEX"] {
        assert!(
            !rows.iter().any(|r| r == decoy),
            "`{decoy}` became a row: {out:#}"
        );
    }
    assert_eq!(out["traffic_lines"], 6, "one traffic line per row: {out:#}");
    let seat_req = req_text(&out["answered_seat_requests"]);
    assert_eq!(out["seat"]["req"], seat_req.as_str(), "{out:#}");
    assert_eq!(out["seat_in_backup"]["req"], seat_req.as_str(), "{out:#}");
    assert_eq!(out["codex"]["req"], Value::Null, "{out:#}");
    assert_eq!(out["codex"]["note"], "no traffic today", "{out:#}");
    assert_ne!(
        req_text(&out["answered_decoy_requests"]),
        seat_req,
        "positive control: the decoy's count is its own: {out:#}"
    );
}

/// A configured account with no seat group reads as having no traffic in
/// the range, in words, never as 0, whatever its credential kind.
///
/// Mutation check: make acctNoTrafficText answer `0 req` -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_an_account_with_no_seat_group_shows_no_traffic_not_zero() {
    let out = accounts_scenario("accounts-traffic");

    let codex = text(&out["codex"]["text"], "the Codex traffic line");
    assert_eq!(codex, "trafficno traffic today", "{out:#}");
    assert!(!codex.contains('0'), "{out:#}");
    assert_eq!(out["cloud"]["note"], "no traffic today", "{out:#}");
    assert!(
        out["seat"]["req"].is_string(),
        "positive control: a row with a group shows its count: {out:#}"
    );
}

/// The seat answer the endpoint serves carries a group for the API-key
/// entry's own account, and its row shows that group's requests and cost;
/// an API-key row and a cloud row with no group read "no traffic", and no
/// row says its traffic is not recorded.
///
/// Mutation check: restore the non-subscription branch of acctNoTrafficText
/// -> red (the idle rows read "not recorded per account ...").
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_an_api_key_row_shows_its_own_traffic() {
    let out = accounts_scenario("accounts-traffic-entry");

    let served: Vec<&str> = out["served_accounts"]
        .as_array()
        .expect("the served accounts")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        served.contains(&"#anthropic") && served.contains(&"#compat"),
        "positive control: the endpoint fixture serves the API-key groups: {out:#}"
    );
    assert_eq!(
        out["api_key"]["req"],
        req_text(&out["answered_api_key_requests"]).as_str(),
        "{out:#}"
    );
    assert!(out["api_key"]["cost"].is_string(), "{out:#}");
    assert_eq!(out["compat"]["note"], "no traffic today", "{out:#}");
    assert_eq!(out["cloud"]["note"], "no traffic today", "{out:#}");
    assert!(
        !text(&out["page_text"], "the Accounts body").contains("not recorded per account"),
        "{out:#}"
    );
}

/// A seat answer whose groups carry no account identity (a payload of
/// another grouping) is not read as the accounts' traffic: every line says
/// the traffic is not available, none says there was none, and the rows and
/// windows still render.
///
/// Mutation check: drop the seat-grouping check in acctSeatTraffic -> red
/// (every row reads "no traffic today").
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_a_payload_not_grouped_by_seat_is_never_read_as_traffic() {
    let out = accounts_scenario("accounts-traffic-not-seat");

    assert_eq!(out["seat"]["note"], "not available", "{out:#}");
    assert_eq!(out["codex"]["note"], "not available", "{out:#}");
    assert_eq!(out["seat"]["req"], Value::Null, "{out:#}");
    assert_eq!(
        out["rows"], 6,
        "positive control: the rows still render: {out:#}"
    );
    assert_eq!(out["error_cards"], 0, "{out:#}");
}

/// Subscription traffic shows its API-price equivalent as "~$" with "At API
/// prices" and no spend figure; a priced group shows its spend; a partial one
/// says its figure is the priced subtotal only.
///
/// Mutation check: drop the `~$` prefix in acctCostNodes -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_traffic_cost_names_an_equivalent_as_one() {
    let out = accounts_scenario("accounts-traffic");

    assert_eq!(out["seat"]["cost"], Value::Null, "{out:#}");
    assert_eq!(out["seat"]["alt"], "~$1.23At API prices", "{out:#}");
    assert_eq!(out["api_key"]["cost"], "$0.500", "{out:#}");
    assert_eq!(out["api_key"]["alt"], Value::Null, "{out:#}");
    assert_eq!(
        out["compat"]["cost"], "$0.250priced subtotal only",
        "{out:#}"
    );
}

/// A provider scope carried in from another view narrows the seat QUERY;
/// Accounts names it in the heading and the no-traffic words and offers the
/// control that lifts it, while every row still lists.
///
/// Mutation check: drop the scope strip in buildAccounts -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_a_provider_scope_is_named_and_can_be_lifted() {
    let report = accounts_scenario_at("accounts-traffic-scoped", "accounts?scope=claude-seat");
    let out = &report.result;

    let seat_query = report
        .requests
        .iter()
        .filter(|r| r["method"] == "QUERY")
        .filter_map(|r| r["body"].as_str())
        .map(|b| serde_json::from_str::<Value>(b).expect("a JSON body"))
        .find(|b| b["group_by"] == "seat")
        .unwrap_or_else(|| panic!("a seat QUERY: {:#?}", report.requests));
    assert_eq!(seat_query["provider"], "claude-seat", "{seat_query}");
    assert_eq!(
        out["head"], "Traffic today - requests and cost per account, through claude-seat only",
        "{out:#}"
    );
    assert_eq!(
        out["codex_note"], "no traffic through claude-seat today",
        "{out:#}"
    );
    assert_eq!(out["reset"], "all providers", "{out:#}");
    assert_eq!(out["rows"], 6, "{out:#}");
}

/// The traffic heading names the selected range and moves with the picker,
/// which is live on Accounts with its covers text. While the new range's
/// answer is out the lines say so; every login, window and poll line reads
/// the same before, during and after, in the same nodes, and the account
/// section is never marked pending.
///
/// Mutation checks: give `accounts` the span `'current state'` in VIEWS ->
/// red (the picker is dimmed and Week never applies); name a fixed range in
/// acctTrafficHeadText -> red (the heading stays on today).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_traffic_follows_the_range_and_nothing_else_does() {
    let out = accounts_scenario("accounts-traffic-range");
    let (today, held, week) = (&out["today"], &out["held"], &out["week"]);

    let heading = "Traffic {} - requests and cost per account";
    assert_eq!(today["head"], heading.replace("{}", "today"), "{out:#}");
    assert_eq!(held["head"], heading.replace("{}", "this week"), "{out:#}");
    assert_eq!(week["head"], held["head"], "{out:#}");
    assert_eq!(today["windowless"], "false", "{out:#}");
    assert_eq!(today["covers_hidden"], false, "{out:#}");
    assert!(
        text(&today["covers"], "covers").starts_with("Today:"),
        "{out:#}"
    );
    assert!(
        text(&week["covers"], "covers").starts_with("Week:"),
        "{out:#}"
    );
    assert_eq!(week["week_pressed"], "true", "{out:#}");

    assert_eq!(
        today["seat"]["req"],
        req_text(&out["today_requests"]).as_str(),
        "{out:#}"
    );
    assert_eq!(held["seat"]["note"], "loading", "{out:#}");
    assert_eq!(
        week["seat"]["req"],
        req_text(&out["week_requests"]).as_str(),
        "{out:#}"
    );
    assert_ne!(
        today["seat"]["req"], week["seat"]["req"],
        "positive control: the week answer carried other figures: {out:#}"
    );

    for (when, seen) in [("held", held), ("week", week)] {
        assert_eq!(seen["rows"], today["rows"], "{when}: {out:#}");
        assert_eq!(seen["seat_row_kept"], true, "{when}: {out:#}");
        assert_eq!(seen["window_kept"], true, "{when}: {out:#}");
        assert_eq!(seen["pools_pending"], false, "{when}: {out:#}");
        assert_eq!(seen["pools_skeleton"], false, "{when}: {out:#}");
    }
    assert!(
        !today["rows"].as_array().expect("rows").is_empty(),
        "positive control: the rows were read: {out:#}"
    );
}

/// Whether `code` (comments stripped) reads a `quota` member, in either
/// spelling.
fn reads_quota_member(code: &str) -> bool {
    let code = compact(code);
    code.contains(".quota") || code.contains("['quota']") || code.contains("[\"quota\"]")
}

/// No page code reads the usage panel's per-seat quota any more: the
/// Accounts view reads every window from the accounts panel, and the quota
/// tile, its window map and the Health pool and driver sections are gone.
/// Positive control: the same scan flags the read the old tile made.
///
/// Mutation check: restore `rec.data.quota.forEach(...)` in any part -> red.
#[test]
fn no_page_code_reads_the_usage_quota() {
    let code = strip_js_comments(&script());

    assert!(
        code.contains("accountsPoolSection"),
        "positive control: the scan reads the assembled script"
    );
    assert!(
        reads_quota_member("rec.data.quota.forEach(f);"),
        "positive control: the scan flags a quota read"
    );
    assert!(
        !reads_quota_member(&code),
        "the page still reads a `quota` member"
    );
    for retired in [
        "QUOTA_WINDOW",
        "quotaTile",
        "healthQuotaSection",
        "healthPoolSection",
        "healthDriverSection",
    ] {
        assert!(!code.contains(retired), "`{retired}` remains in the script");
    }
}

/// Each pool and model ranking a seat reads "rank N of M in <pool> for
/// <model>" with its reason in plain words and the token in the title; an
/// unavailable seat reads "not ranked" with its reason and no number; the
/// same seat under two pool groups shows the same entries under each; a
/// reason this build has no words for is shown as sent, in the same color.
///
/// Mutation check: print `seat.rank` unguarded in acctPlaceEntry -> red
/// (the unavailable entry reads `rank null of 1`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_seat_shows_its_rank_and_reason_per_pool_and_model() {
    let out = accounts_scenario("accounts-placement");

    let seat_entries = serde_json::json!([
        "not ranked in claude-pool for claude-opus-4-7 - breaker open after repeated failures",
        "rank 1 of 2 in claude-pool for claude-sonnet-4-5 - ranked on usage pace"
    ]);
    assert_eq!(out["seat"]["entries"], seat_entries, "{out:#}");
    assert_eq!(
        out["seat"]["titles"],
        serde_json::json!(["breaker_open", "pace"]),
        "{out:#}"
    );
    assert_eq!(out["seat_in_backup"]["entries"], seat_entries, "{out:#}");
    assert_eq!(
        out["codex"]["entries"],
        serde_json::json!(["rank 2 of 2 in claude-pool for claude-sonnet-4-5 - not_yet_named"]),
        "{out:#}"
    );
    let colors = out["reason_colors"].as_array().expect("reason colors");
    assert_eq!(colors.len(), 3, "{out:#}");
    assert!(colors.iter().all(|c| *c == colors[0]), "{out:#}");
}

/// The rank joins on the exact account key: a ranking whose seats are the
/// labeled seat's key in another case, its provider entry name and a
/// forwarding member adds no entry to any row, and a row no ranking names
/// shows no placement and no rank text at all.
///
/// Mutation check: key the join on `seat.account.toLowerCase()` in
/// acctPlacementIndex -> red (the seat gains a `claude-haiku-4-5` entry).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_rank_joins_on_the_exact_account_key() {
    let out = accounts_scenario("accounts-placement");

    let seat_entries = out["seat"]["entries"].as_array().expect("seat entries");
    assert!(
        seat_entries
            .iter()
            .all(|e| !text(e, "an entry").contains("claude-haiku-4-5")),
        "{out:#}"
    );
    assert_eq!(
        seat_entries.len(),
        2,
        "positive control: the seat's own rankings are drawn: {out:#}"
    );
    assert_eq!(out["api_key_places"], 0, "{out:#}");
    let api_key = text(&out["api_key_text"], "the API-key row");
    assert!(
        !api_key.contains("rank") && !api_key.contains("placement"),
        "{out:#}"
    );
}

/// A window a complete report omitted reads "kept from an earlier report"
/// beside its meter, in the stale mark's neutral color; a window not kept
/// carries no such label. A window both kept and stale shows both marks.
///
/// Mutation checks: drop the kept label in acctWindowMeta -> red; draw the
/// kept label only when the window is not stale -> red (`stale` and `mark`
/// do not both appear).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_kept_window_is_labelled_and_keeps_its_meter() {
    let out = accounts_scenario("accounts-placement");
    let kept = &out["kept"];

    assert_eq!(kept["mark"], "kept from an earlier report", "{out:#}");
    assert_eq!(kept["stale"], "stale", "{out:#}");
    assert_eq!(kept["meter"], true, "{out:#}");
    assert_eq!(kept["used"], "42% used", "{out:#}");
    assert_eq!(kept["mark_color"], kept["stale_color"], "{out:#}");
    assert_eq!(kept["mark_border"], kept["stale_border"], "{out:#}");
    assert_eq!(out["plain_mark"], Value::Null, "{out:#}");
    assert_eq!(
        out["plain_meter"], true,
        "positive control: the plain window is drawn: {out:#}"
    );
}

/// A panel that carries no ranking draws every row with no placement line
/// and no error. A ranking that is not a list of pools, a pool and model
/// ranking with no seats, and one naming the same account twice are each a
/// malformed payload, shown as the section's error card rather than as rows
/// with a rank line missing, drawn twice or counted against the wrong
/// total. The accounts-placement scenario is the control that a ranking of
/// distinct accounts draws.
///
/// Mutation checks, in acctPlacementIndex: return the empty index for any
/// non-object placement -> red (`malformed` draws rows); drop the empty-seats
/// test -> red (`no_seats`); drop the repeated-account test -> red
/// (`repeated`).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_accounts_absent_ranking_draws_no_rank_and_a_malformed_one_errors() {
    let out = accounts_scenario("accounts-placement-shapes");
    let absent = &out["absent"];

    assert_eq!(absent["places"], 0, "{out:#}");
    assert_eq!(absent["error_cards"], 0, "{out:#}");
    assert!(
        absent["rows"].as_u64().is_some_and(|n| n > 0),
        "positive control: the rows are drawn: {out:#}"
    );
    for case in ["malformed", "no_seats", "repeated"] {
        let seen = &out[case];
        assert_eq!(seen["rows"], 0, "{case}: {out:#}");
        assert_eq!(seen["places"], 0, "{case}: {out:#}");
        assert_eq!(seen["error_cards"], 1, "{case}: {out:#}");
    }
}
