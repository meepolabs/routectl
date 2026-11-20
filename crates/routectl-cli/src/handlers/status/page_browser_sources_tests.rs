// The aggregate round in headless Chrome applies the attention and accounts
// panels to their own source records: validated against their own wire
// versions, held as live records, and refused one at a time. No view reads
// either record yet, so the page is patched to publish its records for the
// driver to read. `include!`d into a child module of `page::tests` beside
// `browser`; the scenario lives in page_browser_sources_driver.js.

use serde_json::Value;

const SOURCES_DRIVER: &str = include_str!("page_browser_sources_driver.js");

/// The line that builds the page's source records.
const RECORDS_BUILT: &str =
    "  ALL_SOURCES.forEach(function (n) { SOURCES[n] = freshRecord(n); });\n";

/// The page with its source record map published on `window`. Records are
/// replaced inside the map, never the map itself, so one reference reads
/// every later record.
fn with_published_records(page: String) -> String {
    assert_eq!(
        page.matches(RECORDS_BUILT).count(),
        1,
        "the page builds its source records as this test expects"
    );
    page.replace(
        RECORDS_BUILT,
        &format!("{RECORDS_BUILT}  window.routectlSourceRecords = SOURCES;\n"),
    )
}

fn run_sources_scenario() -> Value {
    let scenario = browser::Scenario {
        name: "sources-aggregate-records",
        hash: "now",
        snippets: &[SOURCES_DRIVER],
        budget_ms: 30_000,
    };
    browser::run_scenario_on_patched_page(&scenario, with_published_records).result
}

fn fixture_panel(panel: &str) -> Value {
    crate::handlers::status::panel_fixtures::aggregate_json()["panels"][panel].clone()
}

fn account_keys(accounts: &Value) -> Vec<Value> {
    accounts["data"]["accounts"]
        .as_array()
        .expect("the accounts fixture lists rows")
        .iter()
        .map(|row| row["account"].clone())
        .collect()
}

/// Before any answer both records are loading; the first aggregate answer
/// lands each panel in its own live record with the fixture's payload; an
/// answer whose attention envelope is of another wire version is refused as
/// incompatible on attention alone, while accounts in the same answer stays
/// live.
///
/// Mutation checks: drop `attention` from GET_SOURCES -> red (the page holds
/// no attention record); drop `accounts` -> red (likewise); drop the
/// `attention` entry from EXPECTED -> red (the first answer lands
/// incompatible).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_the_aggregate_round_applies_attention_and_accounts() {
    let attention = fixture_panel("attention");
    let accounts = fixture_panel("accounts");

    let result = run_sources_scenario();

    let before = &result["before"];
    assert_eq!(before["attention"]["state"], "loading", "{before:#}");
    assert_eq!(before["accounts"]["state"], "loading", "{before:#}");

    let applied = &result["applied"];
    assert_eq!(applied["attention"]["state"], "live", "{applied:#}");
    assert_eq!(
        applied["attention"]["verdict"], attention["data"]["verdict"],
        "{applied:#}"
    );
    assert_eq!(
        applied["attention"]["items"],
        attention["data"]["items"]
            .as_array()
            .expect("the attention fixture lists items")
            .len(),
        "{applied:#}"
    );
    assert_eq!(applied["attention"]["as_of"], attention["as_of"]);
    assert_eq!(applied["accounts"]["state"], "live", "{applied:#}");
    assert_eq!(
        applied["accounts"]["rows"],
        Value::from(account_keys(&accounts)),
        "{applied:#}"
    );
    assert_eq!(applied["accounts"]["as_of"], accounts["as_of"]);

    let refused = &result["refused"];
    assert_eq!(refused["attention"]["state"], "incompatible", "{refused:#}");
    let version = attention["schema_version"]
        .as_u64()
        .expect("a schema version");
    assert_eq!(
        refused["attention"]["code"],
        format!("expected {version}, received {}", version + 1),
        "{refused:#}"
    );
    assert_eq!(refused["accounts"]["state"], "live", "{refused:#}");
}
