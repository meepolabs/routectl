// The dashboard driven in a real headless Chrome. `include!`d into a child
// module of `page::tests`.
//
// Every browser test is `#[ignore]`: it needs a system Chrome, so it runs
// with `cargo test -p routectl-cli page_browser -- --ignored`, and a missing
// binary FAILS it rather than skipping. `run_scenario` serves the assembled
// `PAGE` from a loopback test server, with the test-only driver
// (page_browser_driver.js) and any scenario snippets injected before
// `</body>`, answers /status and QUERY /status/query (every
// grouping the page asks for, seat included) from the populated panel
// fixtures with figures that change every round, or from one review case
// (`run_case_scenario`) as its builders make it, loads the page in Chrome
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

/// What the harness serves: the populated panel fixtures, changed every
/// round, or one review case as its builders make it, unchanged.
enum Payloads {
    Populated(Box<Populated>),
    Case(Box<CaseServed>),
}

/// One review case and the instant its payloads are built at: a fixed one,
/// or the wall clock at each answer.
pub(super) struct CaseServed {
    pub(super) source: crate::handlers::status::ui_case_fixtures::CaseSource,
    pub(super) now_ms: Option<i64>,
}

impl CaseServed {
    fn now_ms(&self) -> i64 {
        self.now_ms
            .unwrap_or_else(|| crate::local_time::unix_ms(time::OffsetDateTime::now_utc()))
    }
}

/// The fixture payloads served, by route, before the per-round change.
struct Populated {
    aggregate: Value,
    query_model: Value,
    query_alias: Value,
    query_provider: Value,
    query_seat: Value,
    query_series: Value,
}

impl Populated {
    fn new() -> Self {
        use crate::commands::usage::BucketUnit;
        use crate::handlers::status::query::fixture::populated_query_panel;
        use routectl_usage::GroupDim;
        let query = |dim, bucket| {
            serde_json::to_value(populated_query_panel(dim, bucket))
                .expect("the query panel serializes")
        };
        Self {
            aggregate: crate::handlers::status::panel_fixtures::aggregate_json(),
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
    query: AtomicU64,
}

struct Harness {
    page: String,
    payloads: Payloads,
    rounds: Rounds,
}

/// `payload` as round `round` of a changing reading: request counts, the
/// config's load age, the dropped capability writes and each quota
/// utilization move by the round number, so every view has figures that
/// differ between rounds.
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
                    ("requests" | "capability_events_dropped_full", Some(n), _) => {
                        *child = Value::from(n + round)
                    }
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
    match &h.payloads {
        Payloads::Populated(p) => {
            let round = h.rounds.aggregate.fetch_add(1, Ordering::SeqCst);
            json_response(at_round(&p.aggregate, round))
        }
        Payloads::Case(c) => json_response(c.source.aggregate(c.now_ms())),
    }
}

async fn serve_query(
    State(h): State<Arc<Harness>>,
    method: Method,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if method.as_str() != "QUERY" {
        return axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let p = match &h.payloads {
        Payloads::Populated(p) => p,
        Payloads::Case(c) => {
            return c.source.query(&body, c.now_ms()).map_or_else(
                || axum::http::StatusCode::BAD_REQUEST.into_response(),
                json_response,
            );
        }
    };
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(payload) = p.query_for(&parsed) else {
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
            // Exposes `Element.computedName`, the accessible name Chrome's
            // own accessibility tree computes, to the driver.
            "--enable-features=ComputedAccessibilityInfo",
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
    run_page(
        scenario,
        page_with_driver(scenario),
        chrome_switches,
        None,
        Payloads::Populated(Box::new(Populated::new())),
    )
}

/// [`run_scenario`] against one review case built at the fixture instant
/// instead of the populated fixtures.
pub(super) fn run_case_scenario(scenario: &Scenario, case: CaseServed) -> DriverReport {
    run_page(
        scenario,
        page_with_driver(scenario),
        &[],
        None,
        Payloads::Case(Box::new(case)),
    )
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
        Payloads::Populated(Box::new(Populated::new())),
    )
}

/// [`run_scenario`] on the page as `patch` rewrites it, for a test that needs
/// page code no fixture can reach (a view composed only for the test).
pub(super) fn run_scenario_on_patched_page(
    scenario: &Scenario,
    patch: impl FnOnce(String) -> String,
) -> DriverReport {
    run_page(
        scenario,
        patch(page_with_driver(scenario)),
        &[],
        None,
        Payloads::Populated(Box::new(Populated::new())),
    )
}

/// Serve the released page (no driver) and `case` built at the wall clock
/// on a loopback port the OS picks, print the address, and keep serving
/// until the process is killed. Reads no config folder and opens no
/// outbound connection: every payload comes from the case's builders over
/// a ledger in a temp dir.
pub(super) fn serve_review_case(case: crate::handlers::status::ui_case_fixtures::UiCase) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("a local address");
    let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    let harness = Arc::new(Harness {
        page: PAGE.to_owned(),
        payloads: Payloads::Case(Box::new(CaseServed {
            source: crate::handlers::status::ui_case_fixtures::CaseSource::open(
                case,
                now_ms,
                &addr.to_string(),
            ),
            now_ms: None,
        })),
        rounds: Rounds::default(),
    });
    println!("review case {} at http://{addr}/", case.token());
    let app = harness_router(harness);
    let served = runtime.block_on(async move { axum::serve(listener, app).await });
    panic!("the review server stopped: {served:?}");
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
    payloads: Payloads,
) -> DriverReport {
    let harness = Arc::new(Harness {
        page,
        payloads,
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

/// The populated config fixture's pool, read off the Pools card: each
/// member is named "entry (label)" as every other view names a seat, in
/// configured order, beside the strategy token as the config spells it, and
/// a copy of a labeled member is that name alone. Positive control for the
/// faint state: the unlabeled member reads its entry alone and says it has
/// no login label rather than leaving a gap.
///
/// Mutation check: draw the entry alone in poolMembers (no seatName) -> red
/// (`claude-seat` where `claude-seat (a)` is wanted).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_pools_card_names_members_as_entry_and_label() {
    use crate::handlers::status::config::fixture::{FIXTURE_POOL, FIXTURE_POOL_MEMBERS};

    let report = run_scenario(&scenario("config-pools", "config"));

    let rows = report.result["rows"].as_array().expect("pool rows");
    let pool = rows
        .iter()
        .find(|r| r["pool"] == FIXTURE_POOL)
        .unwrap_or_else(|| panic!("the fixture pool: {rows:#?}"));
    assert_eq!(pool["seat_selection"], "fill-first");
    let members = pool["members"].as_array().expect("members");
    let names: Vec<&str> = members
        .iter()
        .map(|m| m["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        [
            FIXTURE_POOL_MEMBERS[0].to_string(),
            format!("{} (a)", FIXTURE_POOL_MEMBERS[1])
        ]
    );
    assert_eq!(members[0]["login"], "no login label");
    assert_eq!(members[0]["login_faint"], true);
    assert_eq!(members[1]["login"], Value::Null, "{:#}", members[1]);
    assert_eq!(members[1]["copied"], names[1], "{:#}", members[1]);
}

/// The members column reads "Members" for every pool, and only the
/// fill-first pool says its members are tried in the order listed: once, in
/// the faint ink the column heads use, under an ordered list. The sticky-least-loaded pool (the same
/// members under another key) carries no such line and lists them unordered.
///
/// Mutation checks: add the order note for every strategy in
/// poolMembersCell -> red (the sticky pool says it); restore the head
/// `members, in order` -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_pools_say_the_order_only_for_fill_first() {
    let report = run_scenario(&scenario("config-pools", "config"));

    let heads = report.result["heads"].as_array().expect("heads");
    assert_eq!(
        heads.last().expect("a members head"),
        "MEMBERS",
        "{heads:?}"
    );
    let rows = report.result["rows"].as_array().expect("pool rows");
    let by_selection = |selection: &str| {
        rows.iter()
            .find(|r| r["seat_selection"] == selection)
            .unwrap_or_else(|| panic!("a {selection} pool: {rows:#?}"))
    };
    let fill = by_selection("fill-first");
    let spread_selection = report.result["spread_selection"]
        .as_str()
        .expect("the planted strategy");
    let spread = by_selection(spread_selection);

    assert_eq!(fill["list_tag"], "ol", "{fill:#}");
    assert_eq!(fill["order_note"], "tried in the order listed", "{fill:#}");
    assert_eq!(
        fill["cell_text"]
            .as_str()
            .expect("cell text")
            .matches("order listed")
            .count(),
        1,
        "{fill:#}"
    );
    assert_eq!(
        fill["order_note_color"], report.result["head_color"],
        "the note is drawn in the faint ink of a column head: {fill:#}"
    );
    assert_eq!(spread["list_tag"], "ul", "{spread:#}");
    assert_eq!(spread["order_note"], Value::Null, "{spread:#}");
    assert_eq!(
        spread["members"].as_array().map(Vec::len),
        fill["members"].as_array().map(Vec::len),
        "positive control: the same members drawn: {spread:#}"
    );
}

/// The priced fixture model's detail row at `width`: the pairs as drawn.
fn config_price_pairs(width: u32) -> Vec<Value> {
    let window = format!("--window-size={width},900");
    let report = run_scenario_with(&scenario("config-prices", "config"), &[&window]);
    assert_eq!(
        report.result["inner_width"].as_u64(),
        Some(u64::from(width))
    );
    report.result["pairs"]
        .as_array()
        .expect("detail pairs")
        .clone()
}

/// At 1440 and 1024 the cache-write price labels read as the config writes
/// the lifetime ("30m", never "30M") in duration order, and every pair in the
/// detail row stacks its value under its label at the label's left edge,
/// with the figure on one line.
///
/// Mutation checks: restore the `.deflist dt` upper-casing on the price
/// labels -> red (the `30m` label); drop the column layout of the scoped
/// pairs -> red (value beside its label).
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_price_labels_read_as_written_and_stack_evenly() {
    for width in [1440, 1024] {
        let pairs = config_price_pairs(width);
        let writes: Vec<&str> = pairs
            .iter()
            .filter_map(|p| p["label"].as_str())
            .filter(|l| l.to_ascii_lowercase().starts_with("cache write"))
            .collect();
        assert_eq!(writes.len(), 3, "at {width}: {pairs:#?}");
        let misread: Vec<(&str, &str)> = writes
            .iter()
            .zip(["5m", "30m", "1h"])
            .filter(|(label, lifetime)| !label.contains(&format!(" {lifetime} ")))
            .map(|(label, lifetime)| (*label, lifetime))
            .collect();
        assert_eq!(
            misread,
            [],
            "at {width}: labels not reading their lifetime as written, in duration order"
        );
        for pair in &pairs {
            let (label, value) = (
                pair["label_left"].as_f64().expect("a label edge"),
                pair["value_left"].as_f64().expect("a value edge"),
            );
            assert!(
                (label - value).abs() <= 1.0,
                "at {width}: value not under its label: {pair:#}"
            );
            assert_eq!(pair["value_below_label"], true, "at {width}: {pair:#}");
            assert_eq!(
                pair["figure_lines"], 1,
                "at {width}: a figure wraps: {pair:#}"
            );
        }
    }
}

/// The strip shows the version the payload serves, here one no build
/// carries, so a hardcoded or dropped field cannot pass.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_config_strip_shows_the_served_version() {
    let report = run_scenario(&scenario("config-version", "config"));

    let served = report.result["served"].as_str().expect("the served value");
    assert_ne!(
        served,
        env!("CARGO_PKG_VERSION"),
        "premise: a distinct value"
    );
    assert_eq!(report.result["facts"]["version"], served);
    assert_eq!(
        report.result["facts"]["listening on"], "127.0.0.1:8787",
        "positive control: the strip read the served source"
    );
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
            "capability_writes": {"capability_events_dropped_full": 2, "write_errors": 1},
            "source": {"loaded_age_ms": 42_000, "listen_addr": "x"},
            "quota": [{"utilization": 0.5}]
        }
    });

    let next = at_round(&payload, 2);

    assert_eq!(next["data"]["groups"][0]["requests"], 5);
    assert_eq!(next["data"]["groups"][0]["errors"], 1);
    assert_eq!(
        next["data"]["capability_writes"]["capability_events_dropped_full"],
        4
    );
    assert_eq!(next["data"]["capability_writes"]["write_errors"], 1);
    assert_eq!(next["data"]["source"]["loaded_age_ms"], 162_000);
    assert_eq!(next["data"]["quota"][0]["utilization"], 0.52);
    assert_eq!(at_round(&payload, 0), payload);
}

/// The seat-grouped answer names a configured login, the API-key entries'
/// own accounts, a login the served config no longer names and the
/// unattributed rows, so a view reading it can tell them apart.
#[test]
fn the_harness_answers_a_seat_grouped_query_from_the_seat_fixture() {
    let payloads = Populated::new();
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
    assert_eq!(account("#anthropic")["account"], "#anthropic");
    assert_eq!(account("#anthropic")["provider_entry"], "anthropic");
    assert_eq!(account("#anthropic")["login_label"], Value::Null);
    assert_eq!(account("#compat")["provider_entry"], "compat");
    assert_eq!(account("(unattributed)")["account"], Value::Null);
    assert_eq!(groups.len(), 5, "{groups:#?}");
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
