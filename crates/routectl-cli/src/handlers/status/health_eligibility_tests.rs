//! `/status/health` eligibility: each target's reason, cause, since and fix,
//! its credential account, the per-pool seat counts, the seat driver's
//! health, and that reading the panel moves no gate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_auth::{OAuthStore, SecretRef, SecretStore};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Role,
};
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{CURRENT_CONFIG_VERSION, Config, ResolvedModel, Router};
use routectl_testkit::ScopedEnv;
use serde_json::Value;
use tokio::sync::Notify;
use tower::ServiceExt;

use super::super::{StatusState, status_router};
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;

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

/// A provider whose `complete` signals `entered`, then holds until
/// `release`, so a dispatch stays in flight while the test reads.
struct Held {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl Provider for Held {
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
        self.entered.notify_one();
        self.release.notified().await;
        Err(Error::upstream("p", 500, "held"))
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

fn status_state(swap: Arc<ArcSwap<Router>>, meta: Arc<DaemonMeta>) -> Arc<StatusState> {
    let app = AppState::for_test(swap);
    Arc::new(StatusState::from_app(&app, None, meta))
}

/// One `GET /status/health` through the status router, as JSON.
async fn read_health(state: &Arc<StatusState>) -> Value {
    let resp = status_router()
        .with_state(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/status/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["unavailable"].is_null(), "{json}");
    json
}

fn row<'a>(json: &'a Value, state_key: &str) -> &'a Value {
    json["data"]["targets"]
        .as_array()
        .expect("targets")
        .iter()
        .find(|t| t["state_key"] == state_key)
        .unwrap_or_else(|| panic!("no target {state_key}: {json}"))
}

fn assert_in_window(value: &Value, before_ms: i64, after_ms: i64, what: &str) {
    let at = value
        .as_i64()
        .unwrap_or_else(|| panic!("{what} is a number: {value}"));
    assert!(
        (before_ms - 1_000..=after_ms).contains(&at),
        "{what} {at} not in {before_ms}..={after_ms}"
    );
}

/// `(state key, provider entry, kind, credential ref)` for each direct seat.
type Seat = (&'static str, &'static str, &'static str, &'static str);
const ANTHROPIC_LABELED: Seat = (
    "a-lab",
    "alpha-a",
    "anthropic-api",
    "oauth://anthropic#seat-a",
);
const ANTHROPIC_DEFAULT: Seat = ("a-def", "alpha", "anthropic-api", "oauth://anthropic");
const CODEX_LABELED: Seat = (
    "c-lab",
    "beta-c",
    "openai-responses",
    "oauth://codex#seat-c",
);
const CODEX_DEFAULT: Seat = ("c-def", "beta", "openai-responses", "oauth://codex");
const ENV_VAR: &str = "GAMMA_KEY_FOR_TESTS";
const ENV_SEAT: Seat = ("env", "gamma", "openai-compat", "env://GAMMA_KEY_FOR_TESTS");
const FILE_PATH: &str = "/fixture/delta.key";
const FILE_SEAT: Seat = (
    "file",
    "delta",
    "openai-compat",
    "file:///fixture/delta.key",
);
const UNKNOWN_PROVIDER: Seat = (
    "unknown",
    "epsilon",
    "anthropic-api",
    "oauth://made-up#seat-e",
);
const FREE: Seat = (
    "free",
    "alpha-free",
    "anthropic-api",
    "oauth://anthropic#seat-free",
);
const MARKED: [(Seat, UnusableCause); 7] = [
    (ANTHROPIC_LABELED, UnusableCause::Revoked),
    (ANTHROPIC_DEFAULT, UnusableCause::NotLoggedIn),
    (CODEX_LABELED, UnusableCause::Expired),
    (CODEX_DEFAULT, UnusableCause::Revoked),
    (ENV_SEAT, UnusableCause::Unreadable),
    (FILE_SEAT, UnusableCause::Unreadable),
    (UNKNOWN_PROVIDER, UnusableCause::Unreadable),
];

fn direct_config(seats: &[Seat]) -> Config {
    let mut text = format!("version = {CURRENT_CONFIG_VERSION}\n");
    for (_, entry, kind, uri) in seats {
        let base = if *kind == "openai-compat" {
            "base_url = \"https://gamma.example.com\"\n"
        } else {
            ""
        };
        text.push_str(&format!(
            "[providers.{entry}]\nkind = \"{kind}\"\n{base}api_key_ref = \"{uri}\"\n"
        ));
    }
    toml::from_str(&text).expect("fixture config parses")
}

/// One non-pooled model per seat, each on its own credential account, with
/// every seat in [`MARKED`] marked unusable.
fn marked_direct_router() -> Router {
    let seats: Vec<Seat> = MARKED.iter().map(|(seat, _)| *seat).chain([FREE]).collect();
    let mut router = Router::new(Arc::new(direct_config(&seats)));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = seats
        .iter()
        .map(|(nickname, entry, _, uri)| {
            let model = ResolvedModel::new(*nickname, *entry, Arc::clone(&provider), "u")
                .with_auth_secret_ref(SecretRef::parse(uri).expect("fixture ref parses"));
            ((*nickname).to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    for ((key, _, _, _), cause) in MARKED {
        assert!(
            router.mark_credential_unusable_for_tests(key, cause),
            "{key}"
        );
    }
    router
}

/// Each unusable seat's row names the reason, its cause, when it was marked,
/// and the exact command that readmits it; an unmarked seat's row is all
/// null. An env or file seat's row names the entry and scheme only, and the
/// serialized payload carries neither the variable name nor the file path.
/// A seat whose `oauth://` provider `routectl login` does not accept names a
/// config correction, never a login command.
///
/// Mutation check: make `oauth_login_command` drop the label -> the two
/// labeled rows go red; make `SeatFix::for_oauth_ref` skip the registry check
/// -> the unknown-provider row goes red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_unusable_seat_row_names_its_reason_cause_since_and_exact_fix() {
    // Arrange
    let before_ms = chrono::Utc::now().timestamp_millis();
    let swap = Arc::new(ArcSwap::from_pointee(marked_direct_router()));
    let state = status_state(swap, DaemonMeta::for_test());

    // Act
    let json = read_health(&state).await;
    let after_ms = chrono::Utc::now().timestamp_millis();

    // Assert
    let rows: [(Seat, &str, &str); 7] = [
        (
            ANTHROPIC_LABELED,
            "anthropic#seat-a",
            "routectl login anthropic --label seat-a",
        ),
        (ANTHROPIC_DEFAULT, "anthropic", "routectl login anthropic"),
        (
            CODEX_LABELED,
            "codex#seat-c",
            "routectl login codex --label seat-c",
        ),
        (CODEX_DEFAULT, "codex", "routectl login codex"),
        (
            ENV_SEAT,
            "#gamma",
            "provide a readable env:// credential for provider entry `gamma`, then reload the \
             config",
        ),
        (
            FILE_SEAT,
            "#delta",
            "provide a readable file:// credential for provider entry `delta`, then reload the \
             config",
        ),
        (
            UNKNOWN_PROVIDER,
            "made-up#seat-e",
            "correct the unknown oauth provider `made-up` of provider entry `epsilon`, then \
             reload the config",
        ),
    ];
    for ((seat, account, fix), (_, cause)) in rows.into_iter().zip(MARKED) {
        let target = row(&json, seat.0);
        let eligibility = &target["eligibility"];
        assert_eq!(target["account"], account, "{}", seat.0);
        assert_eq!(eligibility["reason"], "credential_unusable", "{}", seat.0);
        assert_eq!(eligibility["cause"], cause.as_str(), "{}", seat.0);
        assert_eq!(eligibility["fix"], fix, "{}", seat.0);
        assert_in_window(&eligibility["since_ms"], before_ms, after_ms, seat.0);
    }
    let free = row(&json, FREE.0);
    assert_eq!(free["account"], "anthropic#seat-free");
    let free_eligibility = free["eligibility"].as_object().expect("eligibility object");
    for key in ["reason", "cause", "since_ms", "fix"] {
        assert!(
            free_eligibility.get(key).is_some_and(Value::is_null),
            "{key} is present and null on a dispatchable target: {free}"
        );
    }
    let text = json.to_string();
    for secret in [ENV_VAR, FILE_PATH, "delta.key"] {
        assert!(
            !text.contains(secret),
            "the payload names `{secret}`: {text}"
        );
    }
}

/// A fresh, far-off-expiry record for `seat`.
fn fresh_record(expires_at_unix: u64) -> Value {
    serde_json::json!({
        "access_token": "tok",
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires_at_unix,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": 0
    })
}

fn write_credentials(path: &std::path::Path, seats: &[&str]) {
    let expires = routectl_auth::oauth::unix_now() + 3600;
    let providers: serde_json::Map<String, Value> = seats
        .iter()
        .map(|seat| ((*seat).to_string(), fresh_record(expires)))
        .collect();
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// Pool `the-pool` of three anthropic seats behind model `pooled`.
fn pool_config_text(usage_db: &std::path::Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for member in ["seat-a", "seat-b", "seat-c"] {
        text.push_str(&format!(
            "[providers.{member}]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#{member}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(
        "[pools.the-pool]\nmembers = [\"seat-a\", \"seat-b\", \"seat-c\"]\n\n\
         [models.pooled]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n",
    );
    text
}

/// Over a real build with seat a logged in, seat b never logged in (a
/// placeholder) and seat c logged in but breaker-open, the pool row counts
/// one eligible seat and one of each reason, the seat rows say why, and the
/// seat driver's health is the driver's own record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn pool_counts_seat_rows_and_driver_health_match_the_fixture() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let creds = dir.path().join("routectl").join("credentials.json");
    write_credentials(&creds, &["anthropic#seat-a", "anthropic#seat-c"]);
    let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::Transient));
    let store = OAuthStore::open_with_scripted_refresh_for_tests(&creds, flow)
        .await
        .expect("open the scripted store");
    let secrets: Arc<dyn SecretStore> = Arc::new(store);
    let config = routectl_router::parse_config(&pool_config_text(&dir.path().join("usage.db")))
        .expect("fixture parses");
    let before_ms = chrono::Utc::now().timestamp_millis();
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("a pool with a logged-out member still builds");
    assert!(router.force_open_breaker("pooled#seat-c", Duration::from_hours(1)));
    let meta = DaemonMeta::for_test();
    meta.seat_driver_health().record_two_sweeps_for_tests(2, 1);
    let state = status_state(Arc::new(ArcSwap::from_pointee(router)), meta);

    // Act
    let json = read_health(&state).await;
    let after_ms = chrono::Utc::now().timestamp_millis();

    // Assert
    assert_eq!(
        json["data"]["pools"],
        serde_json::json!([{
            "pool": "the-pool",
            "configured": 3,
            "eligible": 1,
            "by_reason": [
                {"reason": "credential_unusable", "seats": 1},
                {"reason": "breaker_open", "seats": 1},
            ],
        }])
    );
    let placeholder = &row(&json, "pooled#seat-b")["eligibility"];
    assert_eq!(placeholder["reason"], "credential_unusable");
    assert_eq!(placeholder["cause"], "not_logged_in");
    assert_eq!(
        placeholder["fix"],
        "routectl login anthropic --label seat-b"
    );
    assert_in_window(
        &placeholder["since_ms"],
        before_ms,
        after_ms,
        "placeholder since",
    );
    let open = &row(&json, "pooled#seat-c")["eligibility"];
    assert_eq!(open["reason"], "breaker_open");
    assert!(open["cause"].is_null() && open["fix"].is_null(), "{open}");
    assert_in_window(&open["since_ms"], before_ms, after_ms, "breaker since");
    assert!(row(&json, "pooled#seat-a")["eligibility"]["reason"].is_null());

    let driver = &json["data"]["seat_driver"];
    assert_eq!(driver["running"], true);
    assert_eq!(driver["sweeps"], 2);
    assert_eq!(driver["transient"], 2);
    assert_eq!(driver["definitive"], 1);
    assert_in_window(&driver["last_sweep_ms"], before_ms, after_ms, "last sweep");
    assert_in_window(
        &driver["last_good_sweep_ms"],
        before_ms,
        after_ms,
        "last good sweep",
    );
    assert!(driver["last_good_sweep_ms"].as_i64() <= driver["last_sweep_ms"].as_i64());
}

/// A daemon with no driver reports a stopped driver that never swept, every
/// key present.
#[tokio::test]
async fn a_daemon_with_no_driver_reports_it_stopped_and_never_swept() {
    let swap = Arc::new(ArcSwap::from_pointee(Router::new(Arc::default())));
    let json = read_health(&status_state(swap, DaemonMeta::for_test())).await;

    assert_eq!(
        json["data"]["seat_driver"],
        serde_json::json!({
            "running": false,
            "last_sweep_ms": null,
            "last_good_sweep_ms": null,
            "sweeps": 0,
            "transient": 0,
            "definitive": 0,
        })
    );
    assert_eq!(json["data"]["pools"], serde_json::json!([]));
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
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
        }]
        .into(),
        ..Default::default()
    }
}

/// The gate fields of `m` a mutating read would change.
fn gate_fields(json: &Value) -> (Value, Value, Value, Value) {
    let target = row(json, "m");
    (
        target["rpm_available"].clone(),
        target["half_open_probe_in_flight"].clone(),
        target["circuit"].clone(),
        target["eligibility"]["reason"].clone(),
    )
}

/// Model `m` allowing one request per minute, its breaker open with the
/// cooldown already elapsed: a half-open-ready gate with a full one-token
/// bucket, the state an admission changes by spending the token and claiming
/// the half-open slot.
fn half_open_ready(entered: &Arc<Notify>, release: &Arc<Notify>) -> Arc<ArcSwap<Router>> {
    let config: Config = toml::from_str(&format!(
        "version = {CURRENT_CONFIG_VERSION}\n[retry]\nmax_attempts = 1\n\n\
         [providers.p]\nkind = \"anthropic-api\"\napi_key_ref = \"env://P_KEY_FOR_TESTS\"\n\
         rpm_limit = 1\ncircuit_failures = 1\n"
    ))
    .expect("fixture config parses");
    let provider: Arc<dyn Provider> = Arc::new(Held {
        entered: Arc::clone(entered),
        release: Arc::clone(release),
    });
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "u")),
    )]));
    assert!(router.force_open_breaker("m", Duration::ZERO));
    Arc::new(ArcSwap::from_pointee(router))
}

/// Two hundred concurrent `/status/health` reads leave the RPM bucket and
/// the half-open probe slot as they found them; one real admission, held in
/// flight, visibly spends the token and claims the slot on the same read.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_health_reads_leave_rpm_and_probe_state_unchanged() {
    const READS: usize = 200;
    // Arrange
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let swap = half_open_ready(&entered, &release);
    let state = status_state(Arc::clone(&swap), DaemonMeta::for_test());
    let before = gate_fields(&read_health(&state).await);
    assert_eq!(
        before,
        (
            Value::from(1.0),
            Value::from(false),
            Value::from("half_open_ready"),
            Value::Null
        ),
        "premise: a full bucket and a free half-open slot"
    );

    // Act
    let reads: Vec<_> = (0..READS)
        .map(|_| {
            let state = Arc::clone(&state);
            tokio::spawn(async move { gate_fields(&read_health(&state).await) })
        })
        .collect();
    let mut seen = Vec::with_capacity(READS);
    for read in reads {
        seen.push(read.await.expect("read task"));
    }
    let after = gate_fields(&read_health(&state).await);

    // Assert
    assert_eq!(seen.len(), READS);
    assert!(seen.iter().all(|fields| *fields == before), "{seen:?}");
    assert_eq!(after, before, "the reads moved the gate");

    // Positive control: one admission, held in flight, changes both.
    let router = swap.load_full();
    let dispatch = tokio::spawn(async move { router.complete(request("m")).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the dispatch reaches the provider");
    let during = gate_fields(&read_health(&state).await);
    release.notify_one();
    let _ = dispatch.await.expect("dispatch task");
    assert!(
        during.0.as_f64().is_some_and(|tokens| tokens < 1.0),
        "the admission spent the token: {during:?}"
    );
    assert_eq!(during.1, Value::from(true), "the admission holds the slot");
    assert_eq!(during.3, Value::from("breaker_open"));
}
