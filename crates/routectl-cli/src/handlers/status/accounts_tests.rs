//! `/status/accounts`: which accounts are listed and under which identity,
//! how a reading and the poll state map to the wire, the reload boundary,
//! and that no credential reference or body content reaches the panel.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use routectl_auth::{OAuthStore, SecretRef};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
use routectl_router::{
    AccountReading, AccountReadings, AccountWindow, CURRENT_CONFIG_VERSION, Config, LastReading,
    LengthClass, ReadingSource, ReadingWindow, RejectionTotals, ReportState, ResolvedModel, Router,
    Started, UsageVendor,
};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

use super::*;
use crate::handlers::status::{DaemonMeta, StatusState, status_router};
use crate::server::AppState;
use crate::server::seat_driver::SeatDriverHealth;
use crate::server::usage_poll::{SeatPollSnapshot, UsagePollJob};
use routectl_router::runtime_state::UnusableCause;

const ENV_SENTINEL: &str = "SENTINEL_ENV_VAR_7c41";
const FILE_SENTINEL: &str = "/fixture/sentinel-key-2b9e.key";
const AWS_PROFILE_SENTINEL: &str = "sentinel-profile-55d0";
const BODY_SENTINEL: &str = "sentinel-body-a3f8";

/// A provider that is never dialed.
struct Unused;

#[async_trait::async_trait]
impl Provider for Unused {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Err(Error::upstream("p", 500, "unused"))
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

/// `(model nickname, provider entry, provider block body, credential ref)`.
pub(in crate::handlers::status) type Seat = (&'static str, &'static str, &'static str, String);

pub(in crate::handlers::status) fn anthropic_block() -> &'static str {
    "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n"
}

pub(in crate::handlers::status) fn codex_block() -> &'static str {
    "format = \"openai-responses\"\nbase_url = \"https://chatgpt.com/backend-api/codex\"\n\
     auth = \"bearer\"\n"
}

fn compat_block() -> &'static str {
    "format = \"openai-chat\"\nbase_url = \"https://gamma.example.com\"\nauth = \"bearer\"\n"
}

fn bedrock_block() -> &'static str {
    "format = \"bedrock-invoke\"\ncloud = { name = \"aws\", region = \"us-west-2\" }\n\
     auth = \"aws-sigv4\"\n"
}

/// One labeled Anthropic login reached through two entries (`seat-z` and
/// `seat-b`), an unlabeled Codex login, an `env://` key, a `file://` key and
/// an AWS cloud credential.
pub(super) fn every_kind() -> Vec<Seat> {
    vec![
        (
            "m-z",
            "seat-z",
            anthropic_block(),
            "oauth://anthropic#work".into(),
        ),
        (
            "m-b",
            "seat-b",
            anthropic_block(),
            "oauth://anthropic#work".into(),
        ),
        (
            "m-codex",
            "codex-one",
            codex_block(),
            "oauth://codex".into(),
        ),
        (
            "m-env",
            "gamma",
            compat_block(),
            format!("env://{ENV_SENTINEL}"),
        ),
        (
            "m-file",
            "delta",
            compat_block(),
            format!("file://{FILE_SENTINEL}"),
        ),
        (
            "m-cloud",
            "bedrock-west",
            bedrock_block(),
            format!("aws://profile/{AWS_PROFILE_SENTINEL}"),
        ),
    ]
}

fn config_of(seats: &[Seat]) -> Config {
    let mut text = format!("version = {CURRENT_CONFIG_VERSION}\n");
    let mut entries = std::collections::BTreeSet::new();
    for (_, entry, block, uri) in seats {
        if entries.insert(*entry) {
            text.push_str(&format!(
                "[providers.{entry}]\n{block}credential = \"{uri}\"\n"
            ));
        }
    }
    for (nickname, entry, _, _) in seats {
        text.push_str(&format!(
            "[models.{nickname}]\nprovider = \"{entry}\"\nupstream = \"u\"\n"
        ));
    }
    toml::from_str(&text).expect("fixture config parses")
}

/// One non-pooled model per seat.
pub(in crate::handlers::status) fn router_of(seats: &[Seat]) -> Router {
    let mut router = Router::new(Arc::new(config_of(seats)));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = seats
        .iter()
        .map(|(nickname, entry, _, uri)| {
            let model = ResolvedModel::new(*nickname, *entry, Arc::clone(&provider), "u");
            // A cloud credential is no secret reference: it resolves through
            // the cloud SDK, so its model carries none.
            let model = if uri.starts_with("aws://") {
                model
            } else {
                model.with_auth_secret_ref(SecretRef::parse(uri).expect("fixture ref parses"))
            };
            ((*nickname).to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn rfc3339_in(offset: Duration) -> String {
    OffsetDateTime::from(SystemTime::now() + offset)
        .format(&Rfc3339)
        .expect("a formattable instant")
}

/// An Anthropic usage body running the 5-hour window at `percent`, carrying
/// the body sentinel in a member the reducer ignores.
fn anthropic_body(percent: u32) -> Vec<u8> {
    format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{}"}}, "seven_day": null, "note": "{BODY_SENTINEL}"}}"#,
        rfc3339_in(Duration::from_hours(2))
    )
    .into_bytes()
}

fn land_poll(router: &Router, account: &str, percent: u32) {
    let generation = router.acquire_credential_generation(account);
    router
        .observe_polled_usage(
            account,
            UsageVendor::Anthropic,
            &anthropic_body(percent),
            generation,
        )
        .expect("the body is accepted");
}

fn state_over(swap: Arc<ArcSwap<Router>>, meta: Arc<DaemonMeta>) -> Arc<StatusState> {
    let app = AppState::for_test(swap);
    Arc::new(StatusState::from_app(&app, None, meta))
}

async fn get_json(state: &Arc<StatusState>, uri: &str) -> Value {
    let resp = status_router()
        .with_state(Arc::clone(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub(super) async fn read_accounts(state: &Arc<StatusState>) -> Value {
    let json = get_json(state, "/status/accounts").await;
    assert!(json["unavailable"].is_null(), "{json}");
    json
}

fn rows(json: &Value) -> &Vec<Value> {
    json["data"]["accounts"].as_array().expect("accounts array")
}

pub(super) fn row<'a>(json: &'a Value, account: &str) -> &'a Value {
    rows(json)
        .iter()
        .find(|row| row["account"] == account)
        .unwrap_or_else(|| panic!("no account {account}: {json}"))
}

/// Every configured account is listed once, API-key and cloud credentials
/// included with no windows, each with its key, provider entry and login
/// label as three separate keys. A login two entries name is one row under
/// the smaller entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_configured_account_is_listed_with_three_separate_identity_keys() {
    // Arrange
    let state = state_over(
        Arc::new(ArcSwap::from_pointee(router_of(&every_kind()))),
        DaemonMeta::for_test(),
    );

    // Act
    let json = read_accounts(&state).await;

    // Assert
    assert_eq!(json["schema_version"], 1);
    let expected: [(&str, &str, Value); 5] = [
        ("#bedrock-west", "bedrock-west", Value::Null),
        ("#delta", "delta", Value::Null),
        ("#gamma", "gamma", Value::Null),
        ("anthropic#work", "seat-b", Value::from("work")),
        ("codex", "codex-one", Value::Null),
    ];
    let listed: Vec<&str> = rows(&json)
        .iter()
        .map(|row| row["account"].as_str().expect("account is a string"))
        .collect();
    assert_eq!(
        listed,
        expected.iter().map(|(a, _, _)| *a).collect::<Vec<_>>(),
        "{json}"
    );
    for (account, entry, label) in expected {
        let row = row(&json, account);
        let obj = row.as_object().expect("row object");
        assert_eq!(obj["provider_entry"], entry, "{account}");
        assert!(obj.contains_key("login_label"), "{account}");
        assert_eq!(obj["login_label"], label, "{account}");
        assert_eq!(obj["report"], "no_reading", "{account}");
        assert_eq!(obj["windows"], Value::Array(Vec::new()), "{account}");
        assert!(obj["last_accepted"].is_null(), "{account}");
        assert_eq!(obj["poll"]["applicability"], "not_polled", "{account}");
    }
}

/// A login the background driver does not manage, reached through two
/// entries, is one row under the smaller entry whatever the model order.
#[test]
fn an_unmanaged_login_under_two_entries_reports_the_smaller_entry() {
    // Arrange
    let seats: Vec<Seat> = vec![
        (
            "m-1",
            "zz-entry",
            anthropic_block(),
            "oauth://made-up#x".into(),
        ),
        (
            "m-2",
            "aa-entry",
            anthropic_block(),
            "oauth://made-up#x".into(),
        ),
        (
            "m-3",
            "mm-entry",
            anthropic_block(),
            "oauth://made-up#x".into(),
        ),
    ];
    let router = router_of(&seats);
    let identities = AccountIdentities::from_config(&router.config);
    let readings = router.account_readings(&routectl_router::ObservationStamp::now());

    // Act
    let json =
        serde_json::to_value(build_from_sources(sources(identities, readings, None))).unwrap();

    // Assert
    let accounts = json["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1, "{json}");
    assert_eq!(accounts[0]["account"], "made-up#x");
    assert_eq!(accounts[0]["provider_entry"], "aa-entry");
    assert_eq!(accounts[0]["login_label"], "x");
}

/// An accepted poll reading maps every window field onto its own account's
/// row and no other; neither the body sentinel nor any credential reference
/// reaches the panel, while the mapped reading shows the body did land.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_polled_reading_maps_to_the_wire_and_carries_no_credential_or_body_content() {
    // Arrange
    let router = router_of(&every_kind());
    land_poll(&router, "anthropic#work", 42);
    let state = state_over(
        Arc::new(ArcSwap::from_pointee(router)),
        DaemonMeta::for_test(),
    );
    let before_ms = crate::local_time::unix_ms(OffsetDateTime::now_utc());

    // Act
    let json = read_accounts(&state).await;

    // Assert
    let seat = row(&json, "anthropic#work");
    assert_eq!(seat["report"], "windows");
    assert_eq!(seat["last_accepted"]["source"], "poll");
    let [window] = seat["windows"].as_array().expect("windows").as_slice() else {
        panic!("one window: {seat}");
    };
    assert_eq!(window["class"], "short");
    assert_eq!(window["length_secs"], 18_000);
    assert!(window["scope"].is_null());
    assert!((window["used"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    assert!((window["remaining"].as_f64().unwrap() - 0.58).abs() < 1e-9);
    assert_eq!(window["started"], "yes");
    assert_eq!(window["source"], "poll");
    assert_eq!(window["stale"], false);
    let observed = window["observed_at_ms"].as_i64().expect("observed ms");
    assert!((before_ms - 60_000..=before_ms).contains(&observed));
    assert!(window["reset_at_ms"].as_i64().expect("reset ms") > observed);
    assert!(window["age_ms"].as_i64().expect("age ms") >= 0);
    for other in rows(&json)
        .iter()
        .filter(|row| row["account"] != "anthropic#work")
    {
        assert_eq!(
            other["report"], "no_reading",
            "only the polled account: {other}"
        );
        assert!(other["last_accepted"].is_null(), "{other}");
    }
    let text = json.to_string();
    for sentinel in [
        ENV_SENTINEL,
        FILE_SENTINEL,
        "sentinel-key-2b9e",
        AWS_PROFILE_SENTINEL,
        BODY_SENTINEL,
    ] {
        assert!(!text.contains(sentinel), "`{sentinel}` in {text}");
    }
}

/// A config reload that drops a login drops its row and its reading; the
/// seat that stays keeps its reading across the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_that_drops_a_login_drops_its_row_and_keeps_the_others() {
    // Arrange
    let both: Vec<Seat> = vec![
        (
            "m-a",
            "seat-a",
            anthropic_block(),
            "oauth://anthropic#a".into(),
        ),
        (
            "m-r",
            "seat-r",
            anthropic_block(),
            "oauth://anthropic#retired".into(),
        ),
    ];
    let before = router_of(&both);
    land_poll(&before, "anthropic#a", 10);
    land_poll(&before, "anthropic#retired", 20);
    let swap = Arc::new(ArcSwap::from_pointee(before));
    let state = state_over(Arc::clone(&swap), DaemonMeta::for_test());
    let first = read_accounts(&state).await;
    assert_eq!(rows(&first).len(), 2, "premise: both listed: {first}");

    // Act
    let mut after = router_of(&both[..1]);
    after.carry_over_quota_from(&swap.load());
    swap.store(Arc::new(after));
    let json = read_accounts(&state).await;

    // Assert
    let listed: Vec<&Value> = rows(&json).iter().map(|row| &row["account"]).collect();
    assert_eq!(listed, vec!["anthropic#a"], "{json}");
    assert_eq!(row(&json, "anthropic#a")["report"], "windows");
}

/// A reading the meter holds for an account no route target names is never
/// listed, and an account with no reading still is.
#[test]
fn a_reading_for_an_unconfigured_account_is_not_listed() {
    // Arrange
    let router = router_of(&every_kind()[2..3]);
    let identities = AccountIdentities::from_config(&router.config);
    let readings = AccountReadings {
        accounts: vec![AccountReading {
            account: "anthropic#gone".into(),
            report: ReportState::NoWindows,
            windows: Vec::new(),
            last_accepted: None,
            billing: routectl_router::Billing::Unknown,
            billing_fresh: false,
        }],
        rejections: zero_totals(),
        refused_by_admission: 0,
        corrections_confirmed: 0,
    };

    // Act
    let panel = build_from_sources(sources(identities, readings, None));

    // Assert
    let json = serde_json::to_value(panel).unwrap();
    let listed: Vec<&Value> = json["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["account"])
        .collect();
    assert_eq!(listed, vec!["codex"]);
}

const fn zero_totals() -> RejectionTotals {
    RejectionTotals {
        invalid_utilization: 0,
        expired_reset: 0,
        implausible_reset: 0,
        overflow: 0,
        contradictory: 0,
        unknown_length: 0,
        invalid_started: 0,
        malformed_body: 0,
    }
}

fn sources(
    identities: AccountIdentities,
    readings: AccountReadings,
    poll: Option<PollStateSnapshot>,
) -> AccountSources {
    AccountSources {
        identities,
        readings,
        poll,
        history: AccountReadingHistory::default(),
        poll_driver: SeatDriverHealth::default().snapshot(),
    }
}

fn at_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

/// A stale window keeps its values and reports no room; an unstarted window
/// with no reset maps to nulls; every enum maps to its token.
#[test]
fn window_tokens_and_stale_room_map_one_to_one() {
    // Arrange
    let stale = AccountWindow {
        window: ReadingWindow {
            length_secs: 604_800,
            class: LengthClass::Long,
            scope: Some("opus".into()),
            used: 0.9,
            reset: Some(at_ms(9_000)),
            started: Started::Unknown,
        },
        source: ReadingSource::Response,
        observed_at: at_ms(1_000),
        age: None,
        stale: true,
    };
    let unstarted = AccountWindow {
        window: ReadingWindow {
            length_secs: 18_000,
            class: LengthClass::Short,
            scope: None,
            used: 0.0,
            reset: None,
            started: Started::No,
        },
        source: ReadingSource::Poll,
        observed_at: at_ms(2_000),
        age: Some(Duration::from_millis(1_500)),
        stale: false,
    };

    // Act
    let stale = serde_json::to_value(map_window(&stale)).unwrap();
    let unstarted = serde_json::to_value(map_window(&unstarted)).unwrap();

    // Assert
    assert_eq!(
        stale,
        serde_json::json!({
            "class": "long", "length_secs": 604_800, "scope": "opus", "used": 0.9,
            "remaining": null, "reset_at_ms": 9_000, "started": "unknown",
            "source": "response", "observed_at_ms": 1_000, "age_ms": null, "stale": true,
        })
    );
    assert_eq!(
        unstarted,
        serde_json::json!({
            "class": "short", "length_secs": 18_000, "scope": null, "used": 0.0,
            "remaining": 1.0, "reset_at_ms": null, "started": "no",
            "source": "poll", "observed_at_ms": 2_000, "age_ms": 1_500, "stale": false,
        })
    );
}

#[test]
fn report_states_map_to_their_tokens() {
    let rows = [
        (ReportState::NoReading, "no_reading"),
        (ReportState::Windows, "windows"),
        (ReportState::NoWindows, "no_windows"),
    ];
    for (state, token) in rows {
        assert_eq!(report_token(state), token, "{state:?}");
    }
    let last = map_last_accepted(LastReading {
        observed_at: at_ms(5_000),
        source: ReadingSource::Response,
        fresh: true,
    });
    assert_eq!(
        serde_json::to_value(last).unwrap(),
        serde_json::json!({"observed_at_ms": 5_000, "source": "response"})
    );
}

/// The poll row of a polled account carries its vendor, last attempt,
/// result and due time; an account the poll does not read carries
/// `not_polled` and nulls; a paused vendor is listed with its pause end.
#[test]
fn poll_state_maps_per_account_and_vendor_pauses_are_listed() {
    // Arrange
    let router = router_of(&every_kind());
    let identities = AccountIdentities::from_config(&router.config);
    let readings = router.account_readings(&routectl_router::ObservationStamp::now());
    let poll = PollStateSnapshot {
        switched_off: false,
        seats: vec![SeatPollSnapshot {
            account: "anthropic#work".into(),
            vendor: UsageVendor::Anthropic,
            awaiting_login: false,
            last_attempt_ms: Some(1_000),
            last_result: Some("malformed"),
            next_due_ms: Some(301_000),
            failure_since_ms: Some(400),
        }],
        paused_until_ms: vec![(UsageVendor::Codex, 7_000)],
    };

    // Act
    let json = serde_json::to_value(build_from_sources(sources(
        identities,
        readings,
        Some(poll),
    )))
    .unwrap();

    // Assert
    let find = |account: &str| {
        json["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["account"] == account)
            .unwrap()["poll"]
            .clone()
    };
    assert_eq!(
        find("anthropic#work"),
        serde_json::json!({
            "applicability": "polled", "vendor": "anthropic", "last_attempt_ms": 1_000,
            "last_result": "malformed", "next_due_ms": 301_000, "failure_since_ms": 400,
        })
    );
    assert_eq!(
        find("#gamma"),
        serde_json::json!({
            "applicability": "not_polled", "vendor": null, "last_attempt_ms": null,
            "last_result": null, "next_due_ms": null, "failure_since_ms": null,
        })
    );
    assert_eq!(
        json["vendor_pauses"],
        serde_json::json!([{"vendor": "codex", "paused_until_ms": 7_000}])
    );
}

/// The top-level counters come from their sources: the meter's refusal
/// totals and admission refusals, the history writer counters and the poll
/// driver's health. Each source has its own distinct value so a swapped or
/// dropped field shows.
#[test]
fn top_level_counters_come_from_their_own_sources() {
    // Arrange
    let readings = AccountReadings {
        accounts: Vec::new(),
        rejections: RejectionTotals {
            invalid_utilization: 1,
            expired_reset: 2,
            implausible_reset: 3,
            overflow: 4,
            contradictory: 5,
            unknown_length: 6,
            invalid_started: 7,
            malformed_body: 8,
        },
        refused_by_admission: 9,
        corrections_confirmed: 0,
    };
    let mut sources = sources(AccountIdentities::default(), readings, None);
    sources.history = AccountReadingHistory {
        enqueued: 11,
        dropped_full: 12,
        dropped_closed: 13,
        persisted: 14,
        write_errors: 15,
    };

    // Act
    let json = serde_json::to_value(build_from_sources(sources)).unwrap();

    // Assert
    assert_eq!(
        json["rejections"],
        serde_json::json!({
            "invalid_utilization": 1, "expired_reset": 2, "implausible_reset": 3,
            "overflow": 4, "contradictory": 5, "unknown_length": 6,
            "invalid_started": 7, "malformed_body": 8,
        })
    );
    assert_eq!(json["refused_by_admission"], 9);
    assert_eq!(
        json["history"],
        serde_json::json!({
            "enqueued": 11, "dropped_full": 12, "dropped_closed": 13,
            "persisted": 14, "write_errors": 15,
        })
    );
    assert_eq!(json["poll_driver"]["running"], false);
    assert_eq!(json["vendor_pauses"], serde_json::json!([]));
}

/// A daemon running the usage poll reports its managed logins as polled
/// through the published poll state, and every other account as not polled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_poll_marks_its_managed_logins_polled() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let creds = dir.path().join("credentials.json");
    std::fs::write(&creds, br#"{"schema_version": 1, "providers": {}}"#).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let swap = Arc::new(ArcSwap::from_pointee(router_of(&every_kind())));
    let job = UsagePollJob::new(
        store,
        routectl_usage::handle_over_channel(tx),
        Arc::clone(&swap),
    )
    .expect("job");
    let meta = DaemonMeta::for_test();
    assert!(meta.install_usage_poll_state(job.state()).is_ok());
    let state = state_over(swap, meta);

    // Act
    let json = read_accounts(&state).await;

    // Assert
    let expected = [
        ("anthropic#work", "polled", Value::from("anthropic")),
        ("codex", "polled", Value::from("codex")),
        ("#gamma", "not_polled", Value::Null),
        ("#bedrock-west", "not_polled", Value::Null),
    ];
    for (account, applicability, vendor) in expected {
        let poll = &row(&json, account)["poll"];
        assert_eq!(poll["applicability"], applicability, "{account}");
        assert_eq!(poll["vendor"], vendor, "{account}");
        assert!(poll["last_attempt_ms"].is_null(), "{account}: never swept");
    }
}

pub(super) async fn state_with_running_poll(swap: Arc<ArcSwap<Router>>) -> Arc<StatusState> {
    let dir = tempfile::tempdir().expect("tempdir");
    let creds = dir.path().join("credentials.json");
    std::fs::write(&creds, br#"{"schema_version": 1, "providers": {}}"#).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let job = UsagePollJob::new(
        store,
        routectl_usage::handle_over_channel(tx),
        Arc::clone(&swap),
    )
    .expect("job");
    let meta = DaemonMeta::for_test();
    assert!(meta.install_usage_poll_state(job.state()).is_ok());
    state_over(swap, meta)
}

/// A login the router holds as needing a fresh login reads `awaiting_login`
/// under its vendor, not `polled`, while the poll skips it; once a login
/// lifts the mark it reads `polled` again. Other accounts are unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seat_awaiting_login_reads_awaiting_login_until_its_mark_is_lifted() {
    for cause in [UnusableCause::Revoked, UnusableCause::NotLoggedIn] {
        // Arrange
        let swap = Arc::new(ArcSwap::from_pointee(router_of(&every_kind())));
        let state = state_with_running_poll(Arc::clone(&swap)).await;
        assert!(
            swap.load().mark_credential_unusable_for_tests("m-b", cause),
            "{cause:?}: premise, marked"
        );

        // Act
        let marked = read_accounts(&state).await;
        swap.load().lift_oauth_credential_parks(["anthropic#work"]);
        let lifted = read_accounts(&state).await;

        // Assert
        let poll = &row(&marked, "anthropic#work")["poll"];
        assert_eq!(poll["applicability"], "awaiting_login", "{cause:?}: {poll}");
        assert_eq!(poll["vendor"], "anthropic", "{cause:?}: {poll}");
        assert_eq!(
            row(&marked, "codex")["poll"]["applicability"],
            "polled",
            "{cause:?}: another login is unaffected"
        );
        let poll = &row(&lifted, "anthropic#work")["poll"];
        assert_eq!(poll["applicability"], "polled", "{cause:?}: {poll}");
        assert_eq!(poll["vendor"], "anthropic", "{cause:?}: {poll}");
    }
}

/// A seat marked for a cause that clears on its own is still polled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seat_marked_for_a_transient_cause_still_reads_polled() {
    // Arrange
    let swap = Arc::new(ArcSwap::from_pointee(router_of(&every_kind())));
    let state = state_with_running_poll(Arc::clone(&swap)).await;
    assert!(
        swap.load()
            .mark_credential_unusable_for_tests("m-b", UnusableCause::Expired)
    );

    // Act
    let json = read_accounts(&state).await;

    // Assert
    assert_eq!(
        row(&json, "anthropic#work")["poll"]["applicability"],
        "polled"
    );
}

/// The panel's poll driver row has its own fixed key set, each key mapped
/// from its own snapshot field.
#[test]
fn the_poll_driver_row_carries_its_own_key_set() {
    // Arrange
    let snapshot = SeatDriverHealthSnapshot {
        running: true,
        last_sweep_ms: Some(1),
        last_good_sweep_ms: Some(2),
        sweeps: 3,
        transient: 4,
        definitive: 5,
    };

    // Act
    let json = serde_json::to_value(map_poll_driver(snapshot)).unwrap();

    // Assert
    assert_eq!(
        json,
        serde_json::json!({
            "running": true, "last_sweep_ms": 1, "last_good_sweep_ms": 2,
            "sweeps": 3, "transient": 4, "definitive": 5,
        })
    );
}

/// The panel's window and source tokens equal the tokens the reading
/// history stores for the same reading, so a reader can join the two.
#[test]
fn window_tokens_equal_the_history_tokens_of_the_same_reading() {
    // Arrange
    let mut windows = Vec::new();
    for class in [LengthClass::Short, LengthClass::Long] {
        for started in [Started::Yes, Started::No, Started::Unknown] {
            windows.push(ReadingWindow {
                length_secs: match class {
                    LengthClass::Short => 18_000,
                    LengthClass::Long => 604_800,
                },
                class,
                scope: None,
                used: 0.0,
                reset: None,
                started,
            });
        }
    }
    let accepted = routectl_router::AcceptedReading {
        observed_at: at_ms(1_000),
        windows: windows.clone(),
    };

    // Act
    let history = crate::server::usage_poll::history_rows_for_tests("anthropic#a", &accepted);
    let empty = crate::server::usage_poll::history_rows_for_tests(
        "anthropic#a",
        &routectl_router::AcceptedReading {
            observed_at: at_ms(1_000),
            windows: Vec::new(),
        },
    );

    // Assert
    assert_eq!(history.len(), windows.len());
    for (window, stored) in windows.iter().zip(&history) {
        let stored_window = stored.window.as_ref().expect("a windows row");
        assert_eq!(
            class_token(window.class),
            stored_window.length_class.as_str()
        );
        assert_eq!(
            started_token(window.started),
            stored_window.started.as_str()
        );
        assert_eq!(report_token(ReportState::Windows), stored.report().as_str());
        assert_eq!(source_token(ReadingSource::Poll), stored.source.as_str());
    }
    let [no_windows] = empty.as_slice() else {
        panic!("one no_windows row: {empty:?}");
    };
    assert_eq!(
        report_token(ReportState::NoWindows),
        no_windows.report().as_str()
    );
    assert_eq!(
        source_token(ReadingSource::Response),
        routectl_usage::AccountReadingSource::Response.as_str(),
        "the history token for a response reading"
    );
}

/// The aggregate carries the accounts panel at its own version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_aggregate_carries_the_accounts_panel() {
    let state = state_over(
        Arc::new(ArcSwap::from_pointee(router_of(&every_kind()))),
        DaemonMeta::for_test(),
    );

    let json = get_json(&state, "/status").await;

    let panel = &json["panels"]["accounts"];
    assert_eq!(panel["schema_version"], SCHEMA_VERSION);
    assert!(panel["unavailable"].is_null(), "{panel}");
    assert_eq!(panel["data"]["accounts"].as_array().unwrap().len(), 5);
}
