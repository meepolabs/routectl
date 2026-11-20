// The review cases on Now and Accounts in a real headless Chrome: each case
// served as its builders make it, and the signature a reviewer looks for
// read back from the drawn page. `include!`d into a child module of
// `page::tests`; the scenarios live in page_browser_cases_driver.js. The
// review server a reviewer browses a case on lives here too, as an ignored
// test whose name keeps it out of the browser test filter.

use serde_json::{Value, json};

use crate::handlers::status::panel_fixtures::FIXTURE_NOW_MS;
use crate::handlers::status::ui_case_fixtures::{CaseSource, UiCase};

const CASES_DRIVER: &str = include_str!("page_browser_cases_driver.js");

const CASE_BUDGET_MS: u64 = 40_000;

/// Room for the failed rounds' backoff steps.
const OUTAGE_BUDGET_MS: u64 = 150_000;

const LISTEN: &str = "127.0.0.1:8787";

/// The variable naming the case the review server serves.
const REVIEW_CASE_ENV: &str = "ROUTECTL_UI_CASE";

fn served(case: UiCase) -> browser::CaseServed {
    browser::CaseServed {
        source: CaseSource::open(case, FIXTURE_NOW_MS, LISTEN),
        now_ms: Some(FIXTURE_NOW_MS),
    }
}

fn case_scenario(
    case: UiCase,
    name: &'static str,
    hash: &'static str,
    snippets: &'static [&'static str],
    budget_ms: u64,
) -> Value {
    browser::run_case_scenario(
        &browser::Scenario {
            name,
            hash,
            snippets,
            budget_ms,
        },
        served(case),
    )
    .result
}

fn on_now(case: UiCase) -> Value {
    let snippets: &'static [&'static str] = if case == UiCase::Empty {
        &["window.routectlCaseEmpty = true;", CASES_DRIVER]
    } else {
        &[CASES_DRIVER]
    };
    case_scenario(case, "case-now", "now", snippets, CASE_BUDGET_MS)
}

fn on_accounts(case: UiCase) -> Value {
    case_scenario(
        case,
        "case-accounts",
        "accounts",
        &[CASES_DRIVER],
        CASE_BUDGET_MS,
    )
}

/// The page's `idKey` of `raw`, so a row id can be named from its key.
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

fn rows(out: &Value) -> &Vec<Value> {
    out["rows"].as_array().expect("rows drawn")
}

fn row<'a>(out: &'a Value, name: &str) -> &'a Value {
    rows(out)
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` row: {out:#}"))
}

fn attention_row<'a>(out: &'a Value, key: &str) -> Option<&'a Value> {
    let id = format!("now-attention-{}", id_key(key));
    out["attention"]
        .as_array()
        .expect("attention rows")
        .iter()
        .find(|r| r["id"] == id.as_str())
}

/// An empty ledger with no subscription seat: Now says no requests yet with
/// the daemon's base URL and that the config has no subscription login, and
/// Accounts lists every configured credential, none of them in a pool.
///
/// Mutation check: return `overviewEmpty()` for an empty ledger in
/// buildOverviewLive -> red (the first-run card and base URL are gone).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_empty_shows_no_requests_yet_and_every_credential() {
    let now = on_now(UiCase::Empty);
    let accounts = on_accounts(UiCase::Empty);

    assert_eq!(now["empty_title"], "No requests yet", "{now:#}");
    assert_eq!(now["base_url"], format!("http://{LISTEN}"), "{now:#}");
    assert_eq!(
        now["glance_none"], "No subscription login in this config.",
        "{now:#}"
    );
    assert_eq!(
        now["attention_clear"], "Nothing needs attention now.",
        "{now:#}"
    );
    assert_eq!(accounts["pools"], json!(["Not in a pool"]), "{accounts:#}");
    let names: Vec<&Value> = rows(&accounts).iter().map(|r| &r["name"]).collect();
    assert_eq!(
        names,
        [&json!("anthropic"), &json!("bed"), &json!("compat")]
    );
    for row in rows(&accounts) {
        let traffic = row["traffic"].as_str().unwrap_or("");
        assert!(traffic.ends_with("no traffic today"), "{accounts:#}");
    }
    assert_eq!(accounts["error_cards"], 0, "{accounts:#}");
}

/// One seat: Now shows one pool holding one seat, and Accounts one pool of
/// one, usable, with its windows and its own traffic.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_one_seat_shows_one_pool_of_one() {
    let now = on_now(UiCase::OneSeat);
    let accounts = on_accounts(UiCase::OneSeat);

    assert_eq!(
        now["glance_pools"],
        json!([{ "name": "pool main", "seats": ["claude (main)"] }]),
        "{now:#}"
    );
    assert_eq!(accounts["pools"], json!(["main"]), "{accounts:#}");
    assert_eq!(
        accounts["pool_hints"],
        json!(["1 of 1 usable"]),
        "{accounts:#}"
    );
    assert_eq!(rows(&accounts).len(), 1, "{accounts:#}");
    let seat = row(&accounts, "claude (main)");
    assert_eq!(seat["login"], "logged in", "{accounts:#}");
    assert_eq!(seat["windows"], 2, "{accounts:#}");
    let traffic = seat["traffic"].as_str().expect("a traffic line");
    assert!(traffic.contains("2 req"), "{accounts:#}");
}

/// Five seats in one pool: five rows under it, and the idle seats each read
/// their own state: a stale reading marked stale, a seat never read saying
/// so, a window not started saying so, and no traffic today in words.
///
/// Mutation check: drop the `acct-win--stale` class in acctWindow -> red
/// (the stale seat reads fresh).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_five_seat_pool_lists_idle_seats_with_their_state() {
    let accounts = on_accounts(UiCase::FiveSeatPool);

    assert_eq!(accounts["pools"], json!(["seats"]), "{accounts:#}");
    assert_eq!(
        accounts["pool_hints"],
        json!(["5 of 5 usable"]),
        "{accounts:#}"
    );
    assert_eq!(rows(&accounts).len(), 5, "{accounts:#}");
    let busy = row(&accounts, "seat-1 (s1)");
    assert_eq!(busy["stale_windows"], 0, "control: {accounts:#}");
    assert!(busy["traffic"].as_str().unwrap_or("").contains("3 req"));
    assert_eq!(
        row(&accounts, "seat-3 (s3)")["stale_windows"],
        1,
        "{accounts:#}"
    );
    assert_eq!(
        row(&accounts, "seat-4 (s4)")["nowin"],
        "no reading yet",
        "{accounts:#}"
    );
    let unstarted = row(&accounts, "seat-5 (s5)")["resets"].to_string();
    assert!(unstarted.contains("not started"), "{accounts:#}");
    for idle in ["seat-3 (s3)", "seat-4 (s4)", "seat-5 (s5)"] {
        let traffic = row(&accounts, idle)["traffic"].as_str().unwrap_or("");
        assert!(
            traffic.contains("no traffic today"),
            "`{idle}`: {accounts:#}"
        );
    }
}

/// A logged-out seat: Accounts shows the plain cause and the login fix, and
/// Now lists the daemon's `account_unusable` item for it with the same fix.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_logged_out_seat_shows_its_cause_fix_and_item() {
    let now = on_now(UiCase::LoggedOut);
    let accounts = on_accounts(UiCase::LoggedOut);

    let fix = "routectl login anthropic --label b";
    let out = row(&accounts, "seat-b (b)");
    assert_eq!(out["login"], "unusable", "{accounts:#}");
    let cause = out["cause"].as_str().expect("a cause");
    assert!(
        cause.starts_with("Not logged in, for 40 min"),
        "{accounts:#}"
    );
    assert_eq!(out["fix"], fix, "{accounts:#}");
    assert_eq!(row(&accounts, "seat-a (a)")["fix"], Value::Null, "control");
    assert_eq!(
        accounts["pool_hints"],
        json!(["1 of 2 usable"]),
        "{accounts:#}"
    );
    let item = attention_row(&now, "account_unusable:anthropic#b")
        .unwrap_or_else(|| panic!("no account_unusable item: {now:#}"));
    assert_eq!(item["fix"], fix, "{now:#}");
    assert!(
        item["raw"]
            .as_str()
            .unwrap_or("")
            .starts_with("credential_unusable"),
        "{now:#}"
    );
}

/// A usage poll failing for 15 minutes: Now lists the daemon's
/// `usage_poll_failing` item; the one-seat case, the same seat polling
/// cleanly, lists none.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_poll_failing_lists_a_usage_poll_failing_item() {
    let now = on_now(UiCase::PollFailing);
    let clean = on_now(UiCase::OneSeat);

    let key = "usage_poll_failing:anthropic#main";
    let item = attention_row(&now, key).unwrap_or_else(|| panic!("no poll item: {now:#}"));
    assert_eq!(item["raw"], "usage_poll_failing (http)", "{now:#}");
    assert!(
        item["cause"]
            .as_str()
            .unwrap_or("")
            .starts_with("Usage poll failing"),
        "{now:#}"
    );
    assert!(attention_row(&clean, key).is_none(), "control: {clean:#}");
}

/// The outage on Now and on Accounts, over the logged-out case: through two
/// failed aggregate rounds the strip keeps the last verdict, dimmed, saying
/// the daemon cannot be reached, every badge that showed a number keeps it
/// dimmed, and the open view keeps its sections. Positive control: the
/// verdict and at least one badge were live before the outage.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_case_outage_keeps_the_last_verdict_and_dims_badges() {
    for view in ["now", "accounts"] {
        let out = case_scenario(
            UiCase::LoggedOut,
            "case-outage",
            view,
            &["window.routectlCaseFailures = 2;", CASES_DRIVER],
            OUTAGE_BUDGET_MS,
        );

        let live = &out["live"];
        assert_eq!(live["strip"]["dimmed"], false, "`{view}`: {out:#}");
        let verdict = live["strip"]["text"].as_str().expect("a verdict");
        assert!(verdict.contains("Not logged in"), "`{view}`: {out:#}");
        let shown: Vec<(&String, &Value)> = live["badges"]
            .as_object()
            .expect("badges")
            .iter()
            .filter(|(_, b)| !b.is_null())
            .collect();
        assert!(!shown.is_empty(), "`{view}` showed no badge: {out:#}");
        let failed = out["failed"].as_array().expect("failed rounds");
        assert_eq!(failed.len(), 2, "`{view}`: {out:#}");
        for (i, seen) in failed.iter().enumerate() {
            assert_eq!(seen["strip"]["text"], verdict, "`{view}` {i}: {out:#}");
            assert_eq!(seen["strip"]["dimmed"], true, "`{view}` {i}: {out:#}");
            let note = seen["strip"]["note"].as_str().unwrap_or("");
            assert!(
                note.contains("the daemon cannot be reached"),
                "`{view}` {i}"
            );
            for (name, before) in &shown {
                let during = &seen["badges"][name.as_str()];
                assert_eq!(during["text"], before["text"], "`{view}` {i} `{name}`");
                assert_eq!(during["dimmed"], true, "`{view}` {i} `{name}`: {out:#}");
            }
            assert_eq!(seen["sections"], live["sections"], "`{view}` {i}: {out:#}");
        }
    }
}

#[test]
fn the_page_id_key_is_mirrored_for_attention_rows() {
    assert_eq!(
        id_key("account_unusable:anthropic#b"),
        "kaccount_5f_unusable_3a_anthropic_23_b"
    );
    assert!(
        script().contains("'_' + s.charCodeAt(i).toString(16) + '_'"),
        "the page encodes ids as this mirror does"
    );
}

/// Serve one review case to a browser until killed: the released page and
/// the case's payloads at the wall clock, on a loopback port the OS picks.
/// Its name keeps it out of the browser test filter, so only a run naming
/// it starts it.
#[test]
#[ignore = "serves a review case until killed; run it by name with ROUTECTL_UI_CASE set"]
fn ui_review_case_server() {
    let token = std::env::var(REVIEW_CASE_ENV).unwrap_or_default();
    let tokens: Vec<&str> = UiCase::ALL.iter().map(|c| c.token()).collect();
    let case = UiCase::from_token(&token)
        .unwrap_or_else(|| panic!("set {REVIEW_CASE_ENV} to one of {tokens:?}"));
    browser::serve_review_case(case);
}
