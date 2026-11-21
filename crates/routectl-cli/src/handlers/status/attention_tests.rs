//! `/status/attention`: the served v1 shape, how each daemon source maps to
//! the builder's snapshots, the doctor coverage, the driver start-up window,
//! and that a read moves no router or usage state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use routectl_auth::SecretRef;
use routectl_core::failure_class::LastOutcome;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Role,
    UpstreamReason,
};
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::{CircuitPhase, ProviderGateStatus, UnusableCause};
use routectl_router::{
    CURRENT_CONFIG_VERSION, Config, Finding, ReportState, ResolvedModel, Router, Status,
    parse_config,
};
use routectl_testkit::ScopedEnv;
use serde_json::Value;
use tower::ServiceExt;

use super::super::doctor::fail_findings;
use super::super::{DaemonMeta, StatusState, status_router};
use super::*;
use crate::server::AppState;
use crate::server::seat_driver::SeatDriverHealth;
use crate::server::usage_poll::SeatPollSnapshot;
use routectl_router::UsageVendor;

/// A provider that fails at once, never dialing anything.
struct Failing;

#[async_trait::async_trait]
impl Provider for Failing {
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

const ANTHROPIC: &str = "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n";

/// Two models over one labeled Anthropic login (`anthropic#work`, entries
/// `seat-z` and `seat-b`) and one over an API key (`#gamma`).
fn seats_config() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat-z]\n{ANTHROPIC}credential = \"oauth://anthropic#work\"\n\
         [providers.seat-b]\n{ANTHROPIC}credential = \"oauth://anthropic#work\"\n\
         [providers.gamma]\nformat = \"openai-chat\"\nbase_url = \"https://gamma.example.com\"\n\
         auth = \"bearer\"\ncredential = \"env://GAMMA_KEY_FOR_TESTS\"\n\
         [models.m-z]\nprovider = \"seat-z\"\nupstream = \"u\"\n\
         [models.m-b]\nprovider = \"seat-b\"\nupstream = \"u\"\n\
         [models.m-env]\nprovider = \"gamma\"\nupstream = \"u\"\n"
    )
}

/// A router over [`seats_config`] with each model hand-installed.
fn seats_router() -> Router {
    let config = parse_config(&seats_config()).expect("fixture parses");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(Failing);
    let models = [
        ("m-z", "seat-z", "oauth://anthropic#work"),
        ("m-b", "seat-b", "oauth://anthropic#work"),
        ("m-env", "gamma", "env://GAMMA_KEY_FOR_TESTS"),
    ]
    .into_iter()
    .map(|(nickname, entry, uri)| {
        let model = ResolvedModel::new(nickname, entry, Arc::clone(&provider), "u")
            .with_auth_secret_ref(SecretRef::parse(uri).expect("fixture ref parses"));
        (nickname.to_string(), Arc::new(model))
    })
    .collect::<BTreeMap<_, _>>();
    router.install_resolved_models(models);
    router
}

/// A status state over `swap`, its attention memory started `uptime` ago.
fn state_over(
    swap: Arc<ArcSwap<Router>>,
    meta: Arc<DaemonMeta>,
    config_path: Option<std::path::PathBuf>,
    uptime: Duration,
) -> (Arc<StatusState>, Arc<AppState>) {
    let app = AppState::for_test(swap);
    let mut status = StatusState::from_app(&app, config_path, meta);
    status.attention = Arc::new(AttentionState::started_at(
        Instant::now()
            .checked_sub(uptime)
            .expect("the uptime fits the monotonic clock"),
    ));
    (Arc::new(status), app)
}

fn swap_of(router: Router) -> Arc<ArcSwap<Router>> {
    Arc::new(ArcSwap::from_pointee(router))
}

const LONG_UP: Duration = Duration::from_hours(1);

async fn send(state: &Arc<StatusState>, method: &str) -> (StatusCode, Value) {
    let resp = status_router()
        .with_state(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method(method)
                .uri("/status/attention")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn read(state: &Arc<StatusState>) -> Value {
    let (status, json) = send(state, "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "{json}");
    json
}

fn items(json: &Value) -> &Vec<Value> {
    json["data"]["items"].as_array().expect("items array")
}

fn keys_of(json: &Value) -> Vec<String> {
    items(json)
        .iter()
        .map(|item| item["key"].as_str().expect("key").to_string())
        .collect()
}

fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| (*key).to_string()).collect()
}

/// The route serves the v1 envelope: schema 1, verdict, counts, coverage and
/// items carrying every listed key; a non-GET is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_serves_the_v1_shape_and_post_is_refused() {
    // Arrange
    let router = seats_router();
    assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::NotLoggedIn));
    let (state, _app) = state_over(swap_of(router), DaemonMeta::for_test(), None, LONG_UP);

    // Act
    let json = read(&state).await;
    let (post, _) = send(&state, "POST").await;

    // Assert
    assert_eq!(json["schema_version"], 1);
    let data = &json["data"];
    assert_eq!(
        object_keys(data),
        set(&["verdict", "counts", "coverage", "items"])
    );
    assert_eq!(
        object_keys(&data["verdict"]),
        set(&["severity", "text", "target"])
    );
    assert_eq!(
        object_keys(&data["counts"]),
        set(&["broken", "degraded", "recovered", "doctor_fail"])
    );
    assert_eq!(object_keys(&data["coverage"]), set(&["doctor"]));
    let first = items(&json)
        .first()
        .expect("the unusable account is listed");
    assert_eq!(
        object_keys(first),
        set(&[
            "key",
            "kind",
            "severity",
            "state",
            "subject",
            "account",
            "cause",
            "raw_token",
            "since_ms",
            "until_ms",
            "recovered_at_ms",
            "fix",
            "remediation",
            "target",
            "targets",
        ])
    );
    assert_eq!(post, StatusCode::METHOD_NOT_ALLOWED);
}

/// With no doctor report, coverage says so and the routing and account
/// items still serialize; with a report built (over a malformed credential
/// store, which the auth section fails), coverage is current and its Fail
/// findings are listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn doctor_coverage_names_whether_a_report_was_built() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, seats_config()).expect("write config");
    let creds = dir.path().join("routectl").join("credentials.json");
    std::fs::create_dir_all(creds.parent().expect("parent")).expect("mkdir");
    std::fs::write(&creds, "not json").expect("write a malformed credential store");
    let marked = || {
        let router = seats_router();
        assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::Revoked));
        swap_of(router)
    };
    let (no_doctor, _a) = state_over(marked(), DaemonMeta::for_test(), None, LONG_UP);
    let (with_doctor, _b) =
        state_over(marked(), DaemonMeta::for_test(), Some(config_path), LONG_UP);

    // Act
    let unavailable = read(&no_doctor).await;
    let current = read(&with_doctor).await;

    // Assert
    assert_eq!(unavailable["data"]["coverage"]["doctor"], "unavailable");
    let keys = keys_of(&unavailable);
    assert!(
        keys.contains(&"account_unusable:anthropic#work".to_string()),
        "{keys:?}"
    );
    assert_eq!(current["data"]["coverage"]["doctor"], "current");
    let current_keys = keys_of(&current);
    assert!(
        current_keys
            .iter()
            .any(|key| key.starts_with("doctor_failure:")),
        "control: the built report's Fail findings are listed: {current_keys:?}"
    );
    assert!(
        current_keys.contains(&"account_unusable:anthropic#work".to_string()),
        "{current_keys:?}"
    );
}

/// Only Fail findings reach the builder.
#[test]
fn only_fail_findings_are_handed_to_the_builder() {
    // Arrange
    let finding = |name: &str, status| Finding {
        section: "config",
        name: name.to_string(),
        status,
        detail: String::new(),
        remediation: None,
    };
    let report = routectl_router::DoctorReport {
        schema_version: 1,
        findings: vec![
            finding("pass", Status::Pass),
            finding("warn", Status::Warn),
            finding("fail", Status::Fail),
        ],
        panels: routectl_router::DoctorPanels::default(),
    };

    // Act
    let fails = fail_findings(&report);

    // Assert
    let names: Vec<&str> = fails.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["fail"]);
}

/// A daemon with no credential store runs neither driver and has no managed
/// seat, so neither driver is listed as stopped; the same idle drivers with a
/// managed seat are (control).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_stopped_is_not_raised_without_managed_seats() {
    // Arrange
    let bare = Router::new(Arc::new(Config::default()));
    let (no_seats, _a) = state_over(swap_of(bare), DaemonMeta::for_test(), None, LONG_UP);
    let (seats, _b) = state_over(
        swap_of(seats_router()),
        DaemonMeta::for_test(),
        None,
        LONG_UP,
    );

    // Act
    let without = read(&no_seats).await;
    let with = read(&seats).await;

    // Assert
    assert!(
        !keys_of(&without)
            .iter()
            .any(|key| key.starts_with("driver_stopped")),
        "{without}"
    );
    assert_eq!(without["data"]["verdict"]["severity"], "ok");
    let with_keys = keys_of(&with);
    for key in ["driver_stopped:refresh", "driver_stopped:poll"] {
        assert!(with_keys.contains(&key.to_string()), "{key}: {with_keys:?}");
    }
}

/// A read before the drivers' tasks have marked themselves running, early
/// in the daemon's life, lists no stopped driver; the same read after the
/// start-up grace does (control).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_in_the_boot_window_lists_no_stopped_driver() {
    // Arrange
    let (booting, _a) = state_over(
        swap_of(seats_router()),
        DaemonMeta::for_test(),
        None,
        Duration::ZERO,
    );
    let (settled, _b) = state_over(
        swap_of(seats_router()),
        DaemonMeta::for_test(),
        None,
        DRIVER_START_GRACE + Duration::from_secs(1),
    );

    // Act
    let early = read(&booting).await;
    let late = read(&settled).await;

    // Assert
    assert!(
        !keys_of(&early)
            .iter()
            .any(|key| key.starts_with("driver_stopped")),
        "{early}"
    );
    assert!(
        keys_of(&late).contains(&"driver_stopped:refresh".to_string()),
        "{late}"
    );
}

/// The running rule: a running driver runs; one that never swept is
/// starting until the grace passes; one that swept and stopped is stopped
/// at once.
#[test]
fn a_driver_counts_as_running_by_its_health_and_the_start_up_grace() {
    let idle = SeatDriverHealth::default().snapshot();
    let swept = SeatDriverHealthSnapshot {
        last_sweep_ms: Some(1),
        ..idle
    };
    let running = SeatDriverHealthSnapshot {
        running: true,
        ..swept
    };
    let just_under = DRIVER_START_GRACE.saturating_sub(Duration::from_millis(1));
    let rows = [
        ("running, late", running, LONG_UP, true),
        ("never swept, at boot", idle, Duration::ZERO, true),
        ("never swept, just under grace", idle, just_under, true),
        ("never swept, at grace", idle, DRIVER_START_GRACE, false),
        ("swept then stopped, early", swept, Duration::ZERO, false),
    ];
    for (name, health, uptime, expected) in rows {
        assert_eq!(counts_as_running(health, uptime), expected, "{name}");
    }
}

fn target(account: &str, gate: ProviderGateStatus) -> RouteTargetStatus {
    RouteTargetStatus {
        state_key: format!("m-{account}"),
        nickname: format!("m-{account}"),
        provider_name: account.to_string(),
        upstream: "u".to_string(),
        seat_label: None,
        account: Some(account.to_string()),
        login_label: None,
        learned_lane: None,
        gate,
    }
}

fn gate(circuit: CircuitPhase, probe: bool, elapsed: Option<Duration>) -> ProviderGateStatus {
    ProviderGateStatus {
        reason: None,
        rpm_available: None,
        circuit,
        half_open_probe_in_flight: probe,
        circuit_open_elapsed: elapsed,
        last_outcome: Some(LastOutcome::TransportError),
        last_outcome_elapsed: None,
        credential_park: None,
        credential_unusable: None,
    }
}

/// The breaker phases as the list reads them: open within the cooldown and
/// while a half-open probe is in flight (every other request is refused),
/// not open once half-open-ready or closed. The open time is the elapsed
/// age against the read's own epoch anchor.
#[test]
fn a_route_is_open_in_cooldown_and_with_a_probe_in_flight_only() {
    const NOW_MS: i64 = 1_000_000_000;
    let twenty = Some(Duration::from_secs(20));
    let rows = [
        (
            "cooldown",
            gate(CircuitPhase::Open, false, twenty),
            true,
            Some(NOW_MS - 20_000),
        ),
        (
            "half-open in flight",
            gate(CircuitPhase::Open, true, twenty),
            true,
            Some(NOW_MS - 20_000),
        ),
        (
            "half-open ready",
            gate(CircuitPhase::HalfOpenReady, false, twenty),
            false,
            None,
        ),
        (
            "closed",
            gate(CircuitPhase::Closed, false, None),
            false,
            None,
        ),
    ];
    for (name, gate, open, since) in rows {
        let route = route_snapshot(target("#p", gate), NOW_MS);
        assert_eq!(route.circuit_open, open, "{name}");
        assert_eq!(route.open_since_ms, since, "{name}");
        assert_eq!(route.account.as_deref(), Some("#p"), "{name}");
    }
}

/// An account behind two routes is one item, and an unusable mark outranks
/// a live park on it; neither of its routes is listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_behind_two_routes_is_one_unusable_item() {
    // Arrange
    let router = seats_router();
    assert!(router.park_credential_for_tests(
        "m-z",
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_mins(10),
    ));
    assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::NotLoggedIn));
    let (state, _app) = state_over(swap_of(router), DaemonMeta::for_test(), None, LONG_UP);

    // Act
    let json = read(&state).await;

    // Assert
    let work: Vec<&Value> = items(&json)
        .iter()
        .filter(|item| item["account"] == "anthropic#work")
        .collect();
    assert_eq!(work.len(), 1, "{json}");
    assert_eq!(work[0]["kind"], "account_unusable");
    assert_eq!(work[0]["subject"], "seat-b (work)");
    assert_eq!(work[0]["fix"], "routectl login anthropic --label work");
    assert!(
        !keys_of(&json)
            .iter()
            .any(|key| key.starts_with("route_circuit_open")),
        "{json}"
    );
}

/// A parked account alone reads parked, with the park's deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_account_is_listed_with_its_deadline() {
    // Arrange
    let router = seats_router();
    assert!(router.park_credential_for_tests(
        "m-env",
        UpstreamReason::CredentialExpired,
        Instant::now() + Duration::from_mins(10),
    ));
    let (state, _app) = state_over(swap_of(router), DaemonMeta::for_test(), None, LONG_UP);

    // Act
    let json = read(&state).await;

    // Assert
    let parked = items(&json)
        .iter()
        .find(|item| item["key"] == "account_parked:#gamma")
        .unwrap_or_else(|| panic!("listed: {json}"));
    assert!(parked["until_ms"].as_i64().is_some(), "{parked}");
}

/// `fix` is a bare command or null: the login account's unusable item
/// carries the login command and no remediation, and the API-key account,
/// which no command readmits, carries its correction sentence as
/// `remediation` and a null `fix`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unusable_key_account_carries_its_correction_as_remediation_not_fix() {
    // Arrange
    let router = seats_router();
    assert!(router.mark_credential_unusable_for_tests("m-b", UnusableCause::NotLoggedIn));
    assert!(router.mark_credential_unusable_for_tests("m-env", UnusableCause::Unreadable));
    let (state, _app) = state_over(swap_of(router), DaemonMeta::for_test(), None, LONG_UP);

    // Act
    let json = read(&state).await;

    // Assert
    let item = |key: &str| {
        items(&json)
            .iter()
            .find(|item| item["key"] == key)
            .unwrap_or_else(|| panic!("{key} listed: {json}"))
            .clone()
    };
    let key_account = item("account_unusable:#gamma");
    assert!(key_account["fix"].is_null(), "{key_account}");
    assert!(
        key_account["remediation"]
            .as_str()
            .is_some_and(|text| text.contains("`gamma`")),
        "{key_account}"
    );
    let login = item("account_unusable:anthropic#work");
    assert_eq!(login["fix"], "routectl login anthropic --label work");
    assert!(login["remediation"].is_null(), "{login}");
}

/// The refresh driver's current failure on a usable account is listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_failure_on_a_usable_account_is_listed() {
    // Arrange
    let meta = DaemonMeta::for_test();
    meta.refresh_failures()
        .record_failure_for_tests("anthropic#work", 0, "transient");
    let (state, _app) = state_over(swap_of(seats_router()), meta, None, LONG_UP);

    // Act
    let json = read(&state).await;

    // Assert
    let item = items(&json)
        .iter()
        .find(|item| item["key"] == "account_refresh_failing:anthropic#work")
        .unwrap_or_else(|| panic!("listed: {json}"));
    assert_eq!(item["raw_token"], "refresh_failing (transient)");
}

fn poll_seat(account: &str, awaiting: bool, since: Option<i64>) -> SeatPollSnapshot {
    SeatPollSnapshot {
        account: account.to_string(),
        vendor: UsageVendor::Anthropic,
        awaiting_login: awaiting,
        last_attempt_ms: since,
        last_result: since.map(|_| "timeout"),
        next_due_ms: None,
        failure_since_ms: since,
    }
}

/// A poll failure comes only from a recorded failure run: a polled seat with
/// one has it; a polled seat without one, a seat awaiting login, an account
/// the poll does not read and a daemon running no poll have none.
#[test]
fn a_poll_failure_comes_only_from_a_recorded_failure_run() {
    let poll = PollStateSnapshot {
        seats: vec![
            poll_seat("failing", false, Some(42)),
            poll_seat("quiet", false, None),
            poll_seat("awaiting", true, None),
        ],
        paused_until_ms: Vec::new(),
        switched_off: false,
    };
    let rows = [
        (
            "failing",
            Some(&poll),
            Some(PollFailure {
                since_ms: 42,
                last_result: Some("timeout".to_string()),
            }),
        ),
        ("quiet", Some(&poll), None),
        ("awaiting", Some(&poll), None),
        ("not polled", Some(&poll), None),
        ("failing", None, None),
    ];
    for (account, poll, expected) in rows {
        assert_eq!(poll_failure(poll, account), expected, "{account}");
    }
}

fn reading_billed(billing: Billing, billing_fresh: bool) -> AccountReading {
    AccountReading {
        account: "anthropic#work".to_string(),
        report: ReportState::NoReading,
        windows: Vec::new(),
        last_accepted: None,
        billing,
        billing_fresh,
    }
}

/// The account reading's billing state and its freshness are copied across
/// as is; an account with no reading has unknown billing that is not fresh.
#[test]
fn an_account_snapshot_carries_its_reading_billing_and_freshness() {
    let rows = [
        (
            "fresh overage",
            Some(reading_billed(Billing::Overage, true)),
            (Billing::Overage, true),
        ),
        (
            "overage not fresh",
            Some(reading_billed(Billing::Overage, false)),
            (Billing::Overage, false),
        ),
        (
            "fresh included",
            Some(reading_billed(Billing::Included, true)),
            (Billing::Included, true),
        ),
        (
            "unknown",
            Some(reading_billed(Billing::Unknown, false)),
            (Billing::Unknown, false),
        ),
        ("no reading", None, (Billing::Unknown, false)),
    ];
    for (name, reading, expected) in rows {
        let (_, billing, billing_fresh) = reading_parts(reading);

        assert_eq!((billing, billing_fresh), expected, "{name}");
    }
}

/// An account no built target names carries nothing the router knows
/// against it, so its credential reads usable.
#[test]
fn an_account_with_no_built_target_reads_usable() {
    assert_eq!(credential_state(None), CredentialState::Usable);
}

/// The live gate fields a mutating read would move, for every target.
fn gate_fields(router: &Router) -> Vec<(String, Option<u64>, bool, CircuitPhase)> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .map(|target| {
            (
                target.state_key,
                target
                    .gate
                    .rpm_available
                    .map(|tokens| (tokens * 1000.0) as u64),
                target.gate.half_open_probe_in_flight,
                target.gate.circuit,
            )
        })
        .collect()
}

fn usage_counters(app: &AppState) -> [u64; 5] {
    let counters = app.usage.counters();
    [
        counters.enqueued(),
        counters.dropped_full(),
        counters.dropped_closed(),
        counters.persisted(),
        counters.write_errors(),
    ]
}

/// Model `m` allowing one request per minute, its breaker open with the
/// cooldown elapsed: the state an admission changes by spending the token
/// and claiming the half-open slot.
fn half_open_ready_router() -> Router {
    let config: Config = toml::from_str(&format!(
        "version = {CURRENT_CONFIG_VERSION}\n[retry]\nmax_attempts = 1\n\n\
         [providers.p]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
         credential = \"env://P_KEY_FOR_TESTS\"\nauth = \"x-api-key\"\n\
         rpm_limit = 1\ncircuit_failures = 1\n"
    ))
    .expect("fixture config parses");
    let provider: Arc<dyn Provider> = Arc::new(Failing);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "u")),
    )]));
    assert!(router.force_open_breaker("m", Duration::ZERO));
    router
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        max_tokens: Some(16),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        ..Default::default()
    }
}

/// Many concurrent attention reads leave every gate field and the usage
/// writer's counters as they were. Controls: one real dispatch moves the
/// gate, and one enqueued row moves the counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_attention_read_leaves_the_router_and_usage_counters_unchanged() {
    const READS: usize = 100;
    // Arrange
    let swap = swap_of(half_open_ready_router());
    let (state, app) = state_over(Arc::clone(&swap), DaemonMeta::for_test(), None, LONG_UP);
    let gates_before = gate_fields(&swap.load_full());
    let counters_before = usage_counters(&app);
    assert_eq!(
        gates_before[0].1,
        Some(1000),
        "premise: a full one-token bucket"
    );
    assert_eq!(gates_before[0].3, CircuitPhase::HalfOpenReady, "premise");

    // Act
    let reads: Vec<_> = (0..READS)
        .map(|_| {
            let state = Arc::clone(&state);
            tokio::spawn(async move { read(&state).await })
        })
        .collect();
    for read in reads {
        read.await.expect("read task");
    }

    // Assert
    assert_eq!(gate_fields(&swap.load_full()), gates_before);
    assert_eq!(usage_counters(&app), counters_before);

    // Controls: each comparison can see a change.
    let router = swap.load_full();
    let _ = router.complete(request()).await;
    assert_ne!(
        gate_fields(&router),
        gates_before,
        "a dispatch moves the gate"
    );
    app.usage.try_send(routectl_usage::UsageRecord::default());
    assert_ne!(
        usage_counters(&app),
        counters_before,
        "an enqueued row moves the counters"
    );
}

/// Each build records its outcome on the panel's own availability counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_build_is_recorded_on_the_attention_counters() {
    // Arrange
    let (state, _app) = state_over(
        swap_of(seats_router()),
        DaemonMeta::for_test(),
        None,
        LONG_UP,
    );
    assert!(
        state
            .observability
            .attention
            .last_available
            .lock()
            .is_none()
    );

    // Act
    let json = read(&state).await;

    // Assert
    assert_eq!(
        state
            .observability
            .attention
            .last_available
            .lock()
            .as_deref(),
        json["as_of"].as_str()
    );
    assert!(
        state.observability.accounts.last_available.lock().is_none(),
        "control: another panel's counters are untouched"
    );
}
