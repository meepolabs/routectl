// The Accounts view in a real headless Chrome: one row per configured
// account under each pool that dispatches on it, the login state with the
// daemon's cause and a focusable fix, every window labelled from its length
// with room, reset, age, source and stale mark, the poll state, and the two
// background drivers. `include!`d into a child module of `page::tests`; the
// scenarios live in page_browser_accounts_driver.js.

use serde_json::Value;

const ACCOUNTS_DRIVER: &str = include_str!("page_browser_accounts_driver.js");

const ACCOUNTS_BUDGET_MS: u64 = 30_000;

fn accounts_scenario(name: &'static str) -> Value {
    browser::run_scenario(&browser::Scenario {
        name,
        hash: "accounts",
        snippets: &[ACCOUNTS_DRIVER],
        budget_ms: ACCOUNTS_BUDGET_MS,
    })
    .result
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
        seat_poll.starts_with("usage poll: last timeout 1 min ago - failing since 2 min ago"),
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
/// a stale one shows its age, the stale mark and no room, dimmed rather than
/// recolored.
///
/// Mutation check: drop the stale mark in acctWindowMeta -> red.
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
    assert_ne!(stale["opacity"], "1", "{out:#}");
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
