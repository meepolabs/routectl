//! How far one upstream failure reaches, end to end through the assembled
//! daemon: real HTTP into the Anthropic ingress, a router built from a config
//! file the way serve builds it (no injected router), and real anthropic-api
//! egresses dialing two mock origins.
//!
//! The chain is pool A (three OAuth seats on origin X) -> a passthrough
//! entry on origin Y -> pool B (another `[models]` entry, one OAuth seat on
//! origin X). The two origins are distinct loopback ports. Every seat
//! presents its own bearer token, so origin X's received requests attribute
//! each call to the seat that made it.
//!
//! Pool A's seats use the test-only `oauth://test-loopback` family on the
//! oauth-bearer lane, so the egress reads errors on the same lane a managed
//! Anthropic seat does.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use routectl_core::failure_class::LastOutcome;
use routectl_router::runtime_state::CircuitPhase;
use routectl_router::{CURRENT_CONFIG_VERSION, Config, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::CompositeStore;
use super::config_load::load_effective_config;

const WAIT: Duration = Duration::from_secs(10);
const ALIAS: &str = "chain";
const POOL_A: &str = "claude-a";
const PASSTHROUGH: &str = "claude-y";
const POOL_B: &str = "claude-b";
const POOL_A_SEATS: [&str; 3] = ["a-0", "a-1", "a-2"];
const POOL_B_SEAT: &str = "b-0";
const BARE_429_CAPTURE: &str =
    include_str!("../../../routectl-providers/tests/fixtures/anthropic_api/bare_429_capture.json");

fn token_of(seat: &str) -> String {
    format!("tok-{seat}")
}

/// The recorded bare 429 answered to an own-credential OAuth seat: its
/// status, the response headers it carried, and its body.
fn bare_429() -> ResponseTemplate {
    let capture: Value = serde_json::from_str(BARE_429_CAPTURE).expect("capture is JSON");
    let response = &capture["responses"]["oauth-bare-429"];
    let status = u16::try_from(response["status"].as_u64().expect("capture status"))
        .expect("an HTTP status");
    let headers = response["headers"].as_array().expect("capture headers");
    assert!(headers.is_empty(), "the recorded 429 carried no headers");
    let body = response["body"].as_str().expect("capture body").to_string();
    ResponseTemplate::new(status).set_body_raw(body, "application/json")
}

/// A 400 naming no field and no capability.
fn generic_400() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": "invalid request"}
    }))
}

fn served() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_scope", "type": "message", "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn oauth_seat(member: &str, origin: &str, circuit_failures: u32) -> String {
    format!(
        "[providers.{member}]\nkind = \"anthropic-api\"\nbase_url = \"{origin}\"\n\
         api_key_ref = \"oauth://test-loopback#{member}\"\nauth_kind = \"oauth-bearer\"\n\
         circuit_failures = {circuit_failures}\n\n"
    )
}

/// The chain config. Every breaker trips on its second failure, so one debit
/// leaves a seat closed with a failure outcome and two would open it.
fn config_text(origin_x: &str, origin_y: &str, usage_db: &Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 3\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for member in POOL_A_SEATS {
        text.push_str(&oauth_seat(member, origin_x, 2));
    }
    text.push_str(&oauth_seat(POOL_B_SEAT, origin_x, 2));
    text.push_str(&format!(
        "[providers.passthrough]\nkind = \"anthropic-api\"\nbase_url = \"{origin_y}\"\n\
         api_key_ref = \"{key_ref}\"\ncircuit_failures = 2\n\n",
        key_ref = crate::test_secret::file_ref("passthrough-key"),
    ));
    text.push_str(&format!(
        "[pools.pool-a]\nmembers = [\"a-0\", \"a-1\", \"a-2\"]\n\n\
         [pools.pool-b]\nmembers = [\"{POOL_B_SEAT}\"]\n\n\
         [models.{POOL_A}]\nprovider = \"pool-a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.{PASSTHROUGH}]\nprovider = \"passthrough\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.{POOL_B}]\nprovider = \"pool-b\"\nupstream = \"claude-opus-4-6\"\n\n\
         [aliases]\n{ALIAS} = [\"{POOL_A}\", \"{PASSTHROUGH}\", \"{POOL_B}\"]\n"
    ));
    text
}

/// A fresh stored record for every OAuth seat, each carrying its own token.
fn write_credentials(creds: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs();
    let mut providers = serde_json::Map::new();
    for seat in POOL_A_SEATS.into_iter().chain([POOL_B_SEAT]) {
        providers.insert(
            format!("test-loopback#{seat}"),
            json!({
                "access_token": token_of(seat),
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": null },
                "obtained_at_unix": now
            }),
        );
    }
    let doc = json!({ "schema_version": 1, "providers": providers });
    std::fs::write(
        creds,
        serde_json::to_vec_pretty(&doc).expect("serialize creds"),
    )
    .expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(creds, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
}

/// Origin X: pool A's first seat answers `failure` on every call; every
/// other bearer is served.
async fn origin_x(failure: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header(
            "authorization",
            format!("Bearer {}", token_of(POOL_A_SEATS[0])).as_str(),
        ))
        .respond_with(failure)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(served())
        .mount(&server)
        .await;
    server
}

async fn origin_y() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(served())
        .mount(&server)
        .await;
    server
}

struct Rig {
    _dir: tempfile::TempDir,
    x: MockServer,
    y: MockServer,
    base_url: String,
    router: Arc<ArcSwap<Router>>,
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Rig {
    /// Write the config and credentials files, load the config from its file
    /// and serve it, building the router from the stored credentials.
    async fn start(failure: ResponseTemplate) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let x = origin_x(failure).await;
        let y = origin_y().await;
        let creds = dir.path().join("credentials.json");
        write_credentials(&creds);
        let cfg_path = dir.path().join("config.toml");
        let text = config_text(&x.uri(), &y.uri(), &dir.path().join("usage.db"));
        std::fs::write(&cfg_path, text).expect("write config");
        let config = load_effective_config(&cfg_path)
            .expect("the chain config loads and validates")
            .config;
        let secrets = CompositeStore::open_at(&creds)
            .await
            .expect("open the credentials store");
        let (base_url, router, stop, handle) = serve(config, Arc::new(secrets)).await;
        let rig = Self {
            _dir: dir,
            x,
            y,
            base_url,
            router,
            stop,
            handle,
        };
        rig.await_health().await;
        rig
    }

    async fn await_health(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(!self.handle.is_finished(), "the daemon exited at boot");
            assert!(
                Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn send(&self) -> u16 {
        reqwest::Client::new()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": ALIAS, "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .expect("the daemon answers")
            .status()
            .as_u16()
    }

    /// Calls origin X received from `seat`'s bearer.
    async fn calls_from(&self, seat: &str) -> usize {
        let bearer = format!("Bearer {}", token_of(seat));
        self.x
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .filter(|r| {
                r.headers
                    .get("authorization")
                    .is_some_and(|v| v.as_bytes() == bearer.as_bytes())
            })
            .count()
    }

    async fn total_calls(&self) -> usize {
        let x = self
            .x
            .received_requests()
            .await
            .expect("recording on")
            .len();
        let y = self
            .y
            .received_requests()
            .await
            .expect("recording on")
            .len();
        x + y
    }

    async fn calls_on_y(&self) -> usize {
        self.y
            .received_requests()
            .await
            .expect("recording on")
            .len()
    }

    /// `(circuit, last settled outcome)` of the target keyed `state_key`.
    fn breaker_of(&self, state_key: &str) -> (CircuitPhase, Option<LastOutcome>) {
        let gate = self
            .router
            .load()
            .status_targets(Instant::now())
            .into_iter()
            .find(|t| t.state_key == state_key)
            .unwrap_or_else(|| panic!("no status target {state_key}"))
            .gate;
        (gate.circuit, gate.last_outcome)
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(WAIT, self.handle)
            .await
            .expect("the daemon stops")
            .expect("the daemon task does not panic");
    }
}

async fn serve(
    config: Config,
    secrets: Arc<CompositeStore>,
) -> (
    String,
    Arc<ArcSwap<Router>>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    let (observer_tx, observer_rx) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        super::serve::serve_on_listener_with_injected_router(
            Arc::new(config),
            listener,
            None,
            Some(secrets),
            super::serve::DaemonTestSeams {
                router_observer: Some(observer_tx),
                shutdown: Some(stopped),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon serves");
    });
    let router = tokio::time::timeout(WAIT, observer_rx)
        .await
        .expect("the daemon publishes its router at boot")
        .expect("the observer is not dropped before publication");
    (base_url, router, stop, handle)
}

/// A pooled seat's runtime-state key.
fn seat_key(nickname: &str, member: &str) -> String {
    format!("{nickname}#{member}")
}

/// A generic 400 on pool A's first seat excludes origin X for the request:
/// the passthrough on origin Y serves, pool A's siblings and pool B (both on
/// origin X) are never dialed, and nothing debits.
#[tokio::test]
async fn a_generic_400_on_the_first_seat_makes_two_upstream_calls() {
    // Arrange
    let rig = Rig::start(generic_400()).await;

    // Act
    let status = rig.send().await;

    // Assert
    assert_eq!(status, 200, "the passthrough serves the request");
    assert_eq!(rig.calls_from(POOL_A_SEATS[0]).await, 1, "pool A seat 1");
    for sibling in &POOL_A_SEATS[1..] {
        assert_eq!(rig.calls_from(sibling).await, 0, "pool A {sibling}");
    }
    assert_eq!(rig.calls_from(POOL_B_SEAT).await, 0, "pool B");
    assert_eq!(rig.calls_on_y().await, 1, "origin Y");
    assert_eq!(rig.total_calls().await, 2);
    assert_eq!(
        rig.breaker_of(&seat_key(POOL_A, POOL_A_SEATS[0])),
        (CircuitPhase::Closed, None),
        "a 400 never debits"
    );
    rig.stop().await;
}

/// The recorded bare 429 on pool A's first seat, read on the oauth-bearer
/// lane, is recognized: it is not retried, it excludes origin X, and the
/// seat takes exactly one debit. Each breaker trips on its second failure,
/// so a rate-limited outcome on a still-closed breaker is exactly one.
#[tokio::test]
async fn a_bare_429_on_the_oauth_lane_makes_two_calls_and_debits_the_seat_once() {
    // Arrange
    let rig = Rig::start(bare_429()).await;

    // Act
    let status = rig.send().await;

    // Assert
    assert_eq!(status, 200, "the passthrough serves the request");
    assert_eq!(rig.calls_from(POOL_A_SEATS[0]).await, 1, "pool A seat 1");
    for sibling in &POOL_A_SEATS[1..] {
        assert_eq!(rig.calls_from(sibling).await, 0, "pool A {sibling}");
    }
    assert_eq!(rig.calls_from(POOL_B_SEAT).await, 0, "pool B");
    assert_eq!(rig.calls_on_y().await, 1, "origin Y");
    assert_eq!(rig.total_calls().await, 2);
    assert_eq!(
        rig.breaker_of(&seat_key(POOL_A, POOL_A_SEATS[0])),
        (CircuitPhase::Closed, Some(LastOutcome::RateLimited)),
        "exactly one debit on the seat that answered"
    );
    for sibling in &POOL_A_SEATS[1..] {
        assert_eq!(
            rig.breaker_of(&seat_key(POOL_A, sibling)).1,
            None,
            "pool A {sibling} never settled"
        );
    }
    rig.stop().await;
}
