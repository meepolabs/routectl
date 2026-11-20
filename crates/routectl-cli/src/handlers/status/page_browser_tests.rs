// The dashboard driven in a real headless Chrome. `include!`d into a child
// module of `page::tests`.
//
// Every browser test is `#[ignore]`: it needs a system Chrome, so it runs
// with `cargo test -p routectl-cli page_browser -- --ignored`, and a missing
// binary FAILS it rather than skipping. `run_scenario` serves the assembled
// `PAGE` from a loopback test server, with the test-only driver
// (page_browser_driver.js) and any scenario snippets injected before
// `</body>`, answers /status, /status/usage and QUERY /status/query (every
// grouping the page asks for, seat included) from the populated panel
// fixtures with figures that change every round, loads the page in Chrome
// under virtual time and returns the JSON the driver reports.
//
// Adding a topic: write `page_browser_<topic>_tests.rs` plus
// `page_browser_<topic>_driver.js`, attach the tests beside this module in
// page.rs as `mod browser_<topic> { use super::*; include!(...); }`, register
// each scenario in the snippet with `routectlDriver.scenario(name, fn)`, and
// call `browser::run_scenario` with a `Scenario` whose `snippets` carries
// `include_str!` of that snippet.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::Method;
use axum::response::IntoResponse;
use serde_json::Value;

/// The test-only driver. Never part of [`PAGE`]: see
/// `the_released_page_carries_no_driver`.
const DRIVER: &str = include_str!("page_browser_driver.js");

/// The global the driver installs; its absence from [`PAGE`] is how the
/// release bytes are checked.
const DRIVER_MARKER: &str = "routectlDriver";

const RESULT_OPEN: &str = "<pre id=\"browser-driver-result\">";

/// Wall-clock ceiling for one Chrome run. Virtual time makes a run take a
/// few seconds; this only bounds a hung browser.
const CHROME_WALL_LIMIT: Duration = Duration::from_mins(2);

/// One page load: the view hash to open, the driver scenario to run, any
/// driver snippets registering it, and the virtual-time budget Chrome runs
/// the page for before dumping the DOM.
pub(super) struct Scenario {
    pub(super) name: &'static str,
    pub(super) hash: &'static str,
    pub(super) snippets: &'static [&'static str],
    pub(super) budget_ms: u64,
}

/// What a finished run reported: the scenario's own result object and every
/// request the page issued.
#[derive(Debug)]
pub(super) struct DriverReport {
    pub(super) result: Value,
    pub(super) requests: Vec<Value>,
}

/// The fixture payloads served, by route, before the per-round change.
struct Payloads {
    aggregate: Value,
    usage_all: Value,
    query_model: Value,
    query_alias: Value,
    query_provider: Value,
    query_seat: Value,
    query_series: Value,
}

impl Payloads {
    fn populated() -> Self {
        use crate::commands::usage::BucketUnit;
        use crate::handlers::status::query::fixture::populated_query_panel;
        use routectl_usage::GroupDim;
        let query = |dim, bucket| {
            serde_json::to_value(populated_query_panel(dim, bucket))
                .expect("the query panel serializes")
        };
        Self {
            aggregate: crate::handlers::status::panel_fixtures::aggregate_json(),
            usage_all: crate::handlers::status::usage::fixture::populated_usage(),
            query_model: query(GroupDim::Model, None),
            query_alias: query(GroupDim::Alias, None),
            query_provider: query(GroupDim::Provider, None),
            query_seat: query(GroupDim::Seat, None),
            query_series: query(GroupDim::Provider, Some(BucketUnit::Hour)),
        }
    }

    /// The QUERY answer for a request body's grouping and bucket, or None
    /// for a shape the fixtures do not cover.
    fn query_for(&self, body: &Value) -> Option<&Value> {
        let bucketed = body.get("bucket").is_some_and(|b| !b.is_null());
        match (body.get("group_by").and_then(Value::as_str), bucketed) {
            (Some("model"), false) => Some(&self.query_model),
            (Some("alias"), false) => Some(&self.query_alias),
            (Some("provider"), false) => Some(&self.query_provider),
            (Some("seat"), false) => Some(&self.query_seat),
            (Some("provider"), true) => Some(&self.query_series),
            _ => None,
        }
    }
}

/// Answers counted per route, so each answer carries a new round's figures.
#[derive(Default)]
struct Rounds {
    aggregate: AtomicU64,
    usage_all: AtomicU64,
    query: AtomicU64,
}

struct Harness {
    page: String,
    payloads: Payloads,
    rounds: Rounds,
}

/// `payload` as round `round` of a changing reading: request counts, the
/// config's load age, the replay tally and each quota utilization move by
/// the round number, so every view has figures that differ between rounds.
fn at_round(payload: &Value, round: u64) -> Value {
    let mut out = payload.clone();
    shift_figures(&mut out, round);
    out
}

fn shift_figures(value: &mut Value, round: u64) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match (key.as_str(), child.as_u64(), child.as_f64()) {
                    ("requests" | "replayed", Some(n), _) => *child = Value::from(n + round),
                    ("loaded_age_ms", Some(n), _) => *child = Value::from(n + round * 60_000),
                    ("utilization", _, Some(u)) => {
                        *child = Value::from((u + round as f64 / 100.0).min(1.0));
                    }
                    _ => shift_figures(child, round),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| shift_figures(item, round)),
        _ => {}
    }
}

fn json_response(value: Value) -> axum::response::Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        axum::Json(value),
    )
        .into_response()
}

async fn serve_test_page(State(h): State<Arc<Harness>>) -> axum::response::Html<String> {
    axum::response::Html(h.page.clone())
}

async fn serve_aggregate(State(h): State<Arc<Harness>>) -> axum::response::Response {
    let round = h.rounds.aggregate.fetch_add(1, Ordering::SeqCst);
    json_response(at_round(&h.payloads.aggregate, round))
}

async fn serve_usage_all(State(h): State<Arc<Harness>>) -> axum::response::Response {
    let round = h.rounds.usage_all.fetch_add(1, Ordering::SeqCst);
    json_response(at_round(&h.payloads.usage_all, round))
}

async fn serve_query(
    State(h): State<Arc<Harness>>,
    method: Method,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if method.as_str() != "QUERY" {
        return axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(payload) = h.payloads.query_for(&parsed) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let round = h.rounds.query.fetch_add(1, Ordering::SeqCst);
    json_response(at_round(payload, round))
}

/// The page as the test server serves it: [`PAGE`] with the driver, the
/// scenario's snippets and the call that starts it, just before `</body>`.
fn page_with_driver(scenario: &Scenario) -> String {
    let at = PAGE.rfind("</body>").expect("the page closes its body");
    let mut injected = String::from("<script>\n");
    injected.push_str(DRIVER);
    for snippet in scenario.snippets {
        injected.push('\n');
        injected.push_str(snippet);
    }
    injected.push_str(&format!(
        "\nwindow.routectlDriver.run({});\n</script>\n",
        serde_json::to_string(scenario.name).expect("a name serializes")
    ));
    format!("{}{}{}", &PAGE[..at], injected, &PAGE[at..])
}

fn harness_router(harness: Arc<Harness>) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(serve_test_page))
        .route("/status", axum::routing::get(serve_aggregate))
        .route("/status/usage", axum::routing::get(serve_usage_all))
        .route("/status/query", axum::routing::any(serve_query))
        .with_state(harness)
}

fn chrome_binary() -> String {
    std::env::var("ROUTECTL_TEST_CHROME").unwrap_or_else(|_| "google-chrome".to_owned())
}

/// Load `url` in headless Chrome under a virtual-time budget, with `extra`
/// switches after the fixed ones, and return the DOM it dumps. A missing
/// binary, a failed exit or a hung browser fails.
fn dump_dom(url: &str, budget_ms: u64, extra: &[&str], scratch: &Path) -> String {
    let binary = chrome_binary();
    let out_path = scratch.join("dom.html");
    let err_path = scratch.join("chrome.log");
    let mut child = Command::new(&binary)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-extensions",
            "--disable-sync",
            "--mute-audio",
        ])
        .args(extra)
        .arg(format!(
            "--user-data-dir={}",
            scratch.join("profile").display()
        ))
        .arg(format!("--virtual-time-budget={budget_ms}"))
        .arg("--dump-dom")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).expect("create the dom file"))
        .stderr(std::fs::File::create(&err_path).expect("create the chrome log"))
        .spawn()
        .unwrap_or_else(|e| {
            panic!("headless Chrome `{binary}` could not start ({e}); set ROUTECTL_TEST_CHROME")
        });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll chrome") {
            break status;
        }
        if started.elapsed() > CHROME_WALL_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("headless Chrome ran past {CHROME_WALL_LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    assert!(
        status.success(),
        "headless Chrome failed ({status}):\n{log}"
    );
    std::fs::read_to_string(&out_path).expect("read the dumped dom")
}

/// The driver's report out of a dumped DOM. The JSON was written as the text
/// of one `<pre>`, so the serializer escaped exactly `&`, `<` and `>`.
fn driver_report_json(dom: &str) -> Value {
    let start = dom
        .find(RESULT_OPEN)
        .unwrap_or_else(|| panic!("the driver reported nothing; the page ran out of virtual time"))
        + RESULT_OPEN.len();
    let len = dom[start..]
        .find("</pre>")
        .expect("the report element closes");
    let text = dom[start..start + len]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&text).expect("the driver report is JSON")
}

/// Every request the page made went to the test server, on the paths the
/// harness answers.
fn assert_harness_requests_only(requests: &[Value], origin: &str) {
    assert!(!requests.is_empty(), "the page issued no request");
    for req in requests {
        let url = req["url"].as_str().expect("a request url");
        assert!(
            url.starts_with(&format!("{origin}/status")),
            "request left the harness: {url}"
        );
    }
}

/// Serve the page with the driver, run `scenario` in headless Chrome and
/// return what it reported. Panics when the scenario threw or never finished.
pub(super) fn run_scenario(scenario: &Scenario) -> DriverReport {
    run_scenario_with(scenario, &[])
}

/// [`run_scenario`] with extra Chrome switches, such as a forced media
/// preference.
pub(super) fn run_scenario_with(scenario: &Scenario, chrome_switches: &[&str]) -> DriverReport {
    run_page(scenario, page_with_driver(scenario), chrome_switches, None)
}

/// [`run_scenario_with`] in a browser profile whose `Default/Preferences`
/// starts as `preferences`, for a setting only the profile carries (the
/// default font size).
pub(super) fn run_scenario_in_profile(
    scenario: &Scenario,
    chrome_switches: &[&str],
    preferences: &Value,
) -> DriverReport {
    run_page(
        scenario,
        page_with_driver(scenario),
        chrome_switches,
        Some(preferences),
    )
}

/// [`run_scenario`] on the page as `patch` rewrites it, for a test that needs
/// page code no fixture can reach (a view composed only for the test).
pub(super) fn run_scenario_on_patched_page(
    scenario: &Scenario,
    patch: impl FnOnce(String) -> String,
) -> DriverReport {
    run_page(scenario, patch(page_with_driver(scenario)), &[], None)
}

fn seed_profile(scratch: &Path, preferences: &Value) {
    let dir = scratch.join("profile").join("Default");
    std::fs::create_dir_all(&dir).expect("create the profile dir");
    std::fs::write(dir.join("Preferences"), preferences.to_string())
        .expect("write the profile preferences");
}

fn run_page(
    scenario: &Scenario,
    page: String,
    chrome_switches: &[&str],
    preferences: Option<&Value>,
) -> DriverReport {
    let harness = Arc::new(Harness {
        page,
        payloads: Payloads::populated(),
        rounds: Rounds::default(),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind a loopback port");
    let origin = format!("http://{}", listener.local_addr().expect("a local address"));
    let app = harness_router(harness);
    runtime.spawn(async move { axum::serve(listener, app).await });

    let scratch = tempfile::tempdir().expect("a scratch dir");
    if let Some(preferences) = preferences {
        seed_profile(scratch.path(), preferences);
    }
    let dom = dump_dom(
        &format!("{origin}/#{}", scenario.hash),
        scenario.budget_ms,
        chrome_switches,
        scratch.path(),
    );
    runtime.shutdown_background();

    let report = driver_report_json(&dom);
    assert_eq!(report["scenario"], scenario.name);
    assert_eq!(
        report["error"],
        Value::Null,
        "scenario {} failed",
        scenario.name
    );
    assert_eq!(report["done"], true);
    let requests = report["requests"]
        .as_array()
        .expect("the request log")
        .clone();
    assert_harness_requests_only(&requests, &origin);
    DriverReport {
        result: report["result"].clone(),
        requests,
    }
}

const FOCUS_BUDGET_MS: u64 = 30_000;

fn scenario(name: &'static str, hash: &'static str) -> Scenario {
    Scenario {
        name,
        hash,
        snippets: &[],
        budget_ms: FOCUS_BUDGET_MS,
    }
}

/// The observation taken before the rounds, then the one after each round.
fn observations(result: &Value) -> Vec<&Value> {
    let rounds = result["rounds"].as_array().expect("rounds observed");
    assert_eq!(rounds.len(), 2, "two changed rounds were observed");
    std::iter::once(&result["baseline"]).chain(rounds).collect()
}

/// The positive control: the rounds really repainted, so a kept node is a
/// node the morph kept, not one nothing touched.
fn assert_figure_changed_every_round(obs: &[&Value]) {
    for pair in obs.windows(2) {
        assert_ne!(
            pair[0]["figure"], pair[1]["figure"],
            "a figure stayed the same across a round: {obs:#?}"
        );
    }
}

fn assert_flags(obs: &[&Value], flags: &[&str]) {
    for (i, o) in obs.iter().enumerate() {
        for flag in flags {
            assert_eq!(o[*flag], true, "`{flag}` broke at observation {i}: {o:#}");
        }
    }
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_overview_provider_button_keeps_focus_across_rounds() {
    let report = run_scenario(&scenario("overview-provider-focus", "now"));

    let obs = observations(&report.result);

    assert_flags(&obs, &["same_node", "connected", "focused"]);
    assert_figure_changed_every_round(&obs);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_usage_group_by_button_keeps_focus_across_rounds() {
    let report = run_scenario(&scenario("usage-groupby-focus", "traffic"));

    let obs = observations(&report.result);

    assert_flags(&obs, &["same_node", "connected", "focused", "pressed"]);
    assert_figure_changed_every_round(&obs);
    let asked_alias = report
        .requests
        .iter()
        .any(|r| r["body"].as_str().is_some_and(|b| b.contains("\"alias\"")));
    assert!(asked_alias, "the click issued the alias QUERY");
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_models_row_keeps_focus_and_stays_open() {
    let report = run_scenario(&scenario("config-row-focus", "config"));

    let obs = observations(&report.result);

    assert_flags(
        &obs,
        &[
            "same_node",
            "connected",
            "focused",
            "expanded",
            "same_detail",
            "detail_shown",
            "same_disclosure",
            "disclosure_open",
        ],
    );
    assert_figure_changed_every_round(&obs);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_doctor_fix_command_selection_survives_rounds() {
    let report = run_scenario(&scenario("doctor-selection", "doctor"));

    let obs = observations(&report.result);

    assert_ne!(obs[0]["picked"], "", "the scenario selected some text");
    assert_flags(&obs, &["same_text", "same_anchor", "connected"]);
    assert_figure_changed_every_round(&obs);
}

#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_seat_dialog_stays_open_with_focus_inside() {
    let report = run_scenario(&scenario("seat-dialog-focus", "now"));

    let obs = observations(&report.result);

    assert_flags(&obs, &["open", "same_dialog", "same_node", "focus_inside"]);
    assert_figure_changed_every_round(&obs);
}

/// Models: a dispatch target card and the rate-limit figure inside it are the
/// same nodes across two changed rounds, and the figure shows each round's
/// value. Models has no control to focus or open, so the kept nodes are the
/// card and the figure the rounds change.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_models_target_card_and_figure_are_kept_across_rounds() {
    let report = run_scenario(&scenario("models-target-kept", "models"));

    let obs = observations(&report.result);

    assert_flags(&obs, &["same_card", "same_figure_node", "connected"]);
    assert_figure_changed_every_round(&obs);
}

#[test]
fn the_released_page_carries_no_driver() {
    let served = page_with_driver(&scenario("any", "now"));

    assert!(
        DRIVER.contains(DRIVER_MARKER),
        "the marker names the driver"
    );
    assert!(
        served.contains(DRIVER_MARKER),
        "the test page carries the driver"
    );
    assert!(
        !PAGE.contains(DRIVER_MARKER),
        "the released page carries the driver"
    );
}

#[test]
fn only_the_test_module_embeds_the_driver() {
    let page_rs = include_str!("page.rs");
    let release_part = &page_rs[..page_rs
        .find("#[cfg(test)]")
        .expect("page.rs has a test module")];

    assert!(
        page_rs[release_part.len()..].contains("page_browser_tests.rs"),
        "the harness is attached under the test module"
    );
    assert!(
        !release_part.contains("page_browser"),
        "release code names the browser driver"
    );
}

#[test]
fn round_figures_change_and_untouched_keys_stay() {
    let payload = serde_json::json!({
        "data": {
            "groups": [{"label": "a", "requests": 3, "errors": 1}],
            "source": {"loaded_age_ms": 42_000, "listen_addr": "x"},
            "quota": [{"utilization": 0.5}]
        }
    });

    let next = at_round(&payload, 2);

    assert_eq!(next["data"]["groups"][0]["requests"], 5);
    assert_eq!(next["data"]["groups"][0]["errors"], 1);
    assert_eq!(next["data"]["source"]["loaded_age_ms"], 162_000);
    assert_eq!(next["data"]["quota"][0]["utilization"], 0.52);
    assert_eq!(at_round(&payload, 0), payload);
}

/// The seat-grouped answer names a configured account, one the served
/// config no longer names and the unattributed rows, so a view reading it
/// can tell all three apart.
#[test]
fn the_harness_answers_a_seat_grouped_query_from_the_seat_fixture() {
    let payloads = Payloads::populated();
    let body = serde_json::json!({"window": "today", "group_by": "seat"});

    let answer = payloads.query_for(&body).expect("a seat-grouped answer");

    let groups = answer["data"]["groups"].as_array().expect("groups");
    let account = |label: &str| {
        groups
            .iter()
            .find(|g| g["label"] == label)
            .unwrap_or_else(|| panic!("a `{label}` group: {groups:#?}"))
    };
    assert_eq!(account("anthropic#a")["account"], "anthropic#a");
    assert_eq!(account("anthropic#a")["provider_entry"], "claude-seat");
    assert_eq!(account("anthropic#retired")["account"], "anthropic#retired");
    assert_eq!(account("anthropic#retired")["provider_entry"], Value::Null);
    assert_eq!(account("(unattributed)")["account"], Value::Null);
    assert_eq!(groups.len(), 3, "{groups:#?}");
    assert!(
        payloads
            .query_for(&serde_json::json!({"window": "today", "group_by": "model"}))
            .is_some_and(|model| model["data"]["groups"][0].get("account").is_none()),
        "positive control: a non-seat grouping carries no account identity"
    );
}

#[test]
fn the_driver_report_is_read_from_the_dumped_dom() {
    let dom =
        format!("<html><body>{RESULT_OPEN}{{\"a\":\"x &amp;&lt;b&gt;\"}}</pre></body></html>");

    assert_eq!(driver_report_json(&dom)["a"], "x &<b>");
}
