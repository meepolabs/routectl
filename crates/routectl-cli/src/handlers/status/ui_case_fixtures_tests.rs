//! The review cases are what the panels' own builders make of each case's
//! state, and each case carries the facts it exists to show.

use serde_json::Value;

use super::super::panel_fixtures::FIXTURE_NOW_MS;
use super::{CaseSource, UiCase};

const LISTEN: &str = "127.0.0.1:8787";

fn aggregate(case: UiCase) -> Value {
    CaseSource::open(case, FIXTURE_NOW_MS, LISTEN).aggregate(FIXTURE_NOW_MS)
}

fn panel<'a>(aggregate: &'a Value, name: &str) -> &'a Value {
    let env = &aggregate["panels"][name];
    assert_eq!(env["unavailable"], Value::Null, "`{name}`: {env:#}");
    &env["data"]
}

fn accounts(aggregate: &Value) -> Vec<&Value> {
    panel(aggregate, "accounts")["accounts"]
        .as_array()
        .expect("an account list")
        .iter()
        .collect()
}

fn item_kinds(aggregate: &Value) -> Vec<&str> {
    panel(aggregate, "attention")["items"]
        .as_array()
        .expect("an item list")
        .iter()
        .map(|item| item["kind"].as_str().expect("a kind"))
        .collect()
}

fn seat_query(source: &CaseSource) -> Value {
    source
        .query(br#"{"window":"today","group_by":"seat"}"#, FIXTURE_NOW_MS)
        .expect("a seat-grouped answer")
}

/// Every panel of every case is available, and each case's token names it.
#[test]
fn every_case_serves_every_panel_available() {
    for case in UiCase::ALL {
        let agg = aggregate(case);

        for name in ["health", "config", "doctor", "accounts", "attention"] {
            panel(&agg, name);
        }
        assert_eq!(UiCase::from_token(case.token()), Some(case));
    }
    assert_eq!(UiCase::from_token("no-such-case"), None);
}

/// The empty case: no request anywhere, every configured credential listed,
/// none of them a subscription, and nothing needs attention.
#[test]
fn the_empty_case_has_no_traffic_and_no_subscription() {
    let source = CaseSource::open(UiCase::Empty, FIXTURE_NOW_MS, LISTEN);
    let agg = source.aggregate(FIXTURE_NOW_MS);

    let rows = accounts(&agg);
    let names: Vec<&str> = rows
        .iter()
        .map(|r| r["account"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["#anthropic", "#bed", "#compat"]);
    assert!(rows.iter().all(|r| r["subscription"] == false), "{rows:#?}");
    assert_eq!(seat_query(&source)["data"]["totals"]["requests"], 0);
    assert!(seat_query(&source)["data"]["earliest_ms"].is_null());
    assert!(item_kinds(&agg).is_empty(), "{:?}", item_kinds(&agg));
    assert_eq!(panel(&agg, "config")["source"]["listen_addr"], LISTEN);
}

/// The one-seat case: one subscription account in one pool of one, with
/// readings and traffic of its own.
#[test]
fn the_one_seat_case_is_one_pool_of_one() {
    let source = CaseSource::open(UiCase::OneSeat, FIXTURE_NOW_MS, LISTEN);
    let agg = source.aggregate(FIXTURE_NOW_MS);

    let rows = accounts(&agg);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["pools"], serde_json::json!(["main"]));
    assert_eq!(rows[0]["subscription"], true);
    assert_eq!(rows[0]["windows"].as_array().map(Vec::len), Some(2));
    let pools = panel(&agg, "health")["pools"].clone();
    assert_eq!(pools[0]["configured"], 1, "{pools:#}");
    assert_eq!(pools[0]["eligible"], 1, "{pools:#}");
    let groups = seat_query(&source)["data"]["groups"].clone();
    assert_eq!(groups[0]["account"], "anthropic#main", "{groups:#}");
    assert_eq!(groups[0]["metrics"]["requests"], 2, "{groups:#}");
}

/// The five-seat case: five accounts in one pool, two carrying traffic and
/// three idle, among them a seat never read and a stale reading.
#[test]
fn the_five_seat_case_has_idle_seats_reading_their_state() {
    let source = CaseSource::open(UiCase::FiveSeatPool, FIXTURE_NOW_MS, LISTEN);
    let agg = source.aggregate(FIXTURE_NOW_MS);

    let rows = accounts(&agg);
    assert_eq!(rows.len(), 5);
    assert!(
        rows.iter()
            .all(|r| r["pools"] == serde_json::json!(["seats"]))
    );
    let report = |account: &str| {
        rows.iter()
            .find(|r| r["account"] == account)
            .unwrap_or_else(|| panic!("no `{account}` row"))["report"]
            .clone()
    };
    assert_eq!(report("anthropic#s4"), "no_reading");
    assert_eq!(report("anthropic#s3"), "windows");
    let answer = seat_query(&source);
    let traffic: Vec<&str> = answer["data"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| g["account"].as_str().expect("an account"))
        .collect();
    assert_eq!(traffic.len(), 2, "{traffic:?}");
    assert!(traffic.contains(&"anthropic#s1") && traffic.contains(&"anthropic#s2"));
    assert!(item_kinds(&agg).contains(&"account_near_limit"));
}

/// The logged-out case: the seat reads unusable with the builder's plain
/// cause and the login fix, and the list holds one `account_unusable` item
/// for it; the pool reads one of two usable.
#[test]
fn the_logged_out_case_lists_an_unusable_item_with_its_fix() {
    let agg = aggregate(UiCase::LoggedOut);

    let out = accounts(&agg)
        .into_iter()
        .find(|r| r["account"] == "anthropic#b")
        .expect("the logged-out row")
        .clone();
    assert_eq!(out["credential"]["state"], "unusable");
    assert_eq!(out["credential"]["cause"], "Not logged in, for 40 min");
    assert_eq!(
        out["credential"]["fix"],
        "routectl login anthropic --label b"
    );
    let items = panel(&agg, "attention")["items"].clone();
    let unusable: Vec<&Value> = items
        .as_array()
        .expect("items")
        .iter()
        .filter(|i| i["kind"] == "account_unusable")
        .collect();
    assert_eq!(unusable.len(), 1, "{items:#}");
    assert_eq!(unusable[0]["account"], "anthropic#b");
    assert_eq!(unusable[0]["fix"], out["credential"]["fix"]);
    assert_eq!(panel(&agg, "health")["pools"][0]["eligible"], 1);
}

/// The poll case lists a `usage_poll_failing` item naming the poll's last
/// result; the one-seat case, the same seat polling cleanly, lists none.
#[test]
fn the_poll_case_lists_a_failing_usage_poll() {
    let agg = aggregate(UiCase::PollFailing);

    let item = panel(&agg, "attention")["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["kind"] == "usage_poll_failing")
        .cloned()
        .unwrap_or_else(|| panic!("no poll item: {:?}", item_kinds(&agg)));
    assert_eq!(item["raw_token"], "usage_poll_failing (http)");
    assert_eq!(item["account"], "anthropic#main");
    assert!(
        !item_kinds(&aggregate(UiCase::OneSeat)).contains(&"usage_poll_failing"),
        "control: a seat polling cleanly lists no poll item"
    );
}
