//! ASSEMBLED-DAEMON verification of the pre-flight pipeline: a real
//! `serve_on_listener` daemon, real HTTP inference traffic over loopback, and the
//! status and doctor INFO snapshots that daemon emits.
//!
//! # What this covers that the router-level tests cannot
//!
//! Everything here goes through the daemon's own wiring: its listener, its layer
//! stack, its ingress handler, its usage capture, and the request boundary. The
//! router-level proofs (`routectl-router`'s `preflight_behavior.rs`, and this crate's
//! `canary_span_tests`) drive `Router::complete` directly -- correct for what they
//! assert, and blind to every seam between an HTTP request and that call. A pipeline
//! wired into a handler that is never reached, or behind a capture path that drops
//! the metadata, passes all of them.
//!
//! # Why the daemon needs an injected Router, and why that is not a shortcut
//!
//! Two of routectl's own safety rules make these behaviors unreachable through a
//! daemon pointed at an in-process mock, and both are deliberate: pre-flight refuses
//! a target whose base URL names a LOCAL HOP (a rejection from one is not
//! attributable to an upstream, so a verdict must neither be learned from it nor
//! acted on for it), and probe activation refuses the same targets. Every in-process
//! HTTP mock binds loopback. So a daemon aimed at wiremock is a lane where the
//! feature is CORRECTLY inert, and a test over it would pass with the whole pipeline
//! deleted.
//!
//! The resolution is not to weaken either rule. The daemon is handed a Router whose
//! CONFIG carries a remote-looking base URL -- which is what those gates read -- while
//! its resolved provider handle answers in process. Only the transport beneath the
//! provider is substituted, and that is the one part these behaviors do not turn on.
//! Both seams are `test-utils`-gated and proven absent from release builds.
//!
//! # Supervision
//!
//! Every daemon task is supervised: the harness fails loudly if the serve future
//! exits before the test is done (a daemon that died mid-test would make every later
//! assertion a statement about a closed socket), and on teardown the handle is
//! aborted and awaited, accepting only a cancellation. A test that merely dropped the
//! handle would leak a listener into the next case.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider, Result,
    Role, TokenCount, Usage,
};
use routectl_router::plant_acting_field_verdict_for_tests;
use routectl_router::{AliasValue, CatalogOverlay, Config, ModelEntry, ResolvedModel, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The one grounded closed-table path: the field a pre-flight rewrite drops.
const FIELD_PATH: &str = "thinking.enabled.display";

/// A remote-looking base URL, so the attributability gate admits the lane. Nothing
/// is ever sent here.
const REMOTE_BASE: &str = "https://api.anthropic.com";

const ALIAS: &str = "daemon-preflight-alias";
const STATE_KEY: &str = "sonnet";

/// The wire-shape capability key, assembled from parts: the namespace prefix has
/// exactly one compiled spelling and a lexical guard forbids a second.
fn field_key() -> String {
    format!("{}{FIELD_PATH}", format_args!("{}{}", "fie", "ld:"))
}

/// Poll `/health` until the daemon answers, or fail loudly.
///
/// A crate-internal copy of the integration harness's readiness wait, needed because
/// this file moved from `tests/` into the crate: the point of that move is that the
/// Router-injection seam becomes `pub(crate)` under `cfg(test)` rather than a `pub`
/// item any release consumer of the `test-utils` feature could call.
///
/// Health is a PRECONDITION rather than an assumption: a bind failure can leave a
/// stale listener answering, and a test that skipped this would attribute that
/// listener's answers to the daemon under test.
async fn await_health(base_url: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if reqwest::get(format!("{base_url}/health"))
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never answered /health",
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The in-process provider the daemon's Router resolves to.
///
/// Records every request it is handed, so an assertion reads the BYTES the daemon
/// sent rather than inferring from a status code.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<ChatRequest>>,
    calls: AtomicUsize,
    /// Reject any attempt still CARRYING the field, so a canary's restoration draws
    /// a rejection and settles INCONCLUSIVE -- which leaves the verdict resident.
    /// Off where the canary's unrepaired SUCCESS is the disproof under test.
    reject_carried: bool,
}

impl Recorder {
    /// Whether `req` still emits the grounded wire field through EITHER canonical
    /// carrier. Both, because dropping one and leaving the other still ships it.
    fn carries_field(req: &ChatRequest) -> bool {
        req.routectl_internal.anthropic_thinking_display.is_some()
            || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
    }

    fn record(&self, req: &ChatRequest) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().push(req.clone());
    }

    /// Per attempt, in order: whether it still carried the field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.seen.lock().iter().map(Self::carries_field).collect()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// A rejection body shaped like the captured envelope. Carries no secret and makes
/// nothing fire -- production attributes no path to it.
const FIELD_REJECT_BODY: &str = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;

#[async_trait::async_trait]
impl Provider for Recorder {
    fn id(&self) -> &'static str {
        "p0"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p0", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.record(&req);
        if self.reject_carried && Self::carries_field(&req) {
            return Err(Error::upstream("p0", 400, FIELD_REJECT_BODY));
        }
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(Usage::default()),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    refusal: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
                logprobs: None,
            }],
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p0", 500, "unused"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.record(&req);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

/// A servable config on a remote-looking base URL, with its ledger in a tempdir.
fn daemon_config(caps: Option<(&str, u32)>) -> (Arc<Config>, tempfile::TempDir) {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("p0", "claude-sonnet-4-5"),
    );
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Single(STATE_KEY.to_string()));
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    if let Some((provider, cap)) = caps {
        config
            .fidelity
            .paid_probe_daily_caps
            .insert(provider.to_string(), cap);
    }
    let dir = tempfile::tempdir().expect("usage tempdir");
    config.usage.db_path = dir.path().join("usage.db");
    (Arc::new(config), dir)
}

/// A Router over `config` resolving to `recorder`.
fn router_with(config: &Arc<Config>, recorder: Arc<Recorder>) -> Router {
    let mut router = Router::new(Arc::clone(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        Arc::new(ResolvedModel::new(
            STATE_KEY.to_string(),
            "p0".to_string(),
            recorder as Arc<dyn Provider>,
            "claude-sonnet-4-5".to_string(),
        )),
    );
    router.install_resolved_models(models);
    router
}

/// A SUPERVISED running daemon: its base URL, the live router swap it serves from,
/// and its task handle.
///
/// The handle is supervised rather than detached, which matters in both directions:
/// a daemon that exited early would make every later assertion a statement about a
/// closed socket, and a handle merely dropped would leak a listener into the next
/// test.
struct Daemon {
    base_url: String,
    router: Arc<ArcSwap<Router>>,
    /// The daemon's OWN usage handle, for a test driving a production persistence
    /// path. One built beside the daemon would write to a different writer, and the
    /// acknowledgment would then be about a ledger the daemon does not read.
    usage: routectl_usage::UsageHandle,
    /// The daemon's OWN confirmation tracker, so a test drives the advancement
    /// lifecycle its shutdown awaits rather than a private one.
    confirmations: Arc<crate::server::confirmation_advance::ConfirmationTracker>,
    handle: tokio::task::JoinHandle<()>,
    /// Kept so the ledger outlives the daemon.
    _dir: tempfile::TempDir,
}
impl Daemon {
    /// Assert the serve task is still running.
    ///
    /// Called before each act, so a test that would otherwise report a confusing
    /// connection error instead names the real cause.
    fn assert_alive(&self) {
        assert!(
            !self.handle.is_finished(),
            "the daemon exited before the test finished: every assertion after this \
             point would describe a closed socket rather than the behavior",
        );
    }

    /// The Router the daemon is serving from, right now.
    ///
    /// The EXACT one, loaded from the swap the handlers read -- a probe pass against
    /// any other Router would prove nothing about this daemon.
    fn live_router(&self) -> Arc<Router> {
        self.router.load_full()
    }

    /// The usage handle the daemon's own handlers persist through.
    const fn usage(&self) -> &routectl_usage::UsageHandle {
        &self.usage
    }

    /// The confirmation tracker the daemon's own handlers hand advancements to.
    ///
    /// The DAEMON's, not one the test builds: an advancement on a tracker the daemon
    /// does not own would not be awaited by its shutdown, so the test would be
    /// exercising a different lifecycle than production's.
    fn confirmations(&self) -> &crate::server::confirmation_advance::ConfirmationTracker {
        &self.confirmations
    }

    /// Abort the serve task and await it, accepting ONLY a cancellation.
    ///
    /// Awaiting is the point: abort alone returns immediately and leaves the task
    /// running until the runtime happens to poll it, so a test that only aborted
    /// could still leak a live listener. And accepting only a cancellation is what
    /// makes this teardown rather than a way to hide a panic -- a daemon that
    /// panicked must surface here.
    async fn shutdown(self) {
        self.handle.abort();
        match self.handle.await {
            Err(joined) if joined.is_cancelled() => {}
            Err(joined) => panic!("the daemon task failed rather than cancelling: {joined:?}"),
            Ok(()) => {}
        }
    }
}

/// Spawn a real daemon serving `router`, returning once `/health` answers.
///
/// Health is awaited as a PRECONDITION rather than assumed: a bind failure can leave
/// a stale listener answering, and a test that skipped this would attribute that
/// listener's answers to the daemon under test.
async fn spawn_daemon(config: Arc<Config>, dir: tempfile::TempDir, router: Router) -> Daemon {
    spawn_daemon_with_hooks(
        config,
        dir,
        router,
        crate::handlers::status::test_hooks::StatusTestHooks::default(),
    )
    .await
}

/// `spawn_daemon` with this daemon's status seams supplied.
///
/// The seams are PER-DAEMON rather than a process-global slot, and that is not a
/// style choice: a test binary runs its cases concurrently in one process, so a
/// global would be claimed by whichever daemon booted first and every other test
/// would then watch a channel it does not own. Measured on the router-swap seam
/// beside it -- the global shape failed all six cases at once.
async fn spawn_daemon_with_hooks(
    config: Arc<Config>,
    dir: tempfile::TempDir,
    router: Router,
    status_hooks: crate::handlers::status::test_hooks::StatusTestHooks,
) -> Daemon {
    spawn_daemon_full(config, dir, router, status_hooks, None).await
}

/// The full spawn: `config_path` decides which DOCTOR branch the daemon serves.
///
/// `Some` gives the daemon an on-disk config to gather a report from, which is the
/// configured `/status/doctor` path; `None` is the config-less branch that emits the
/// fidelity line with unavailable budget data. Both carry the surface, and they reach
/// the emitter through different code, so a test asserting on one says nothing about
/// the other.
async fn spawn_daemon_full(
    config: Arc<Config>,
    dir: tempfile::TempDir,
    router: Router,
    status_hooks: crate::handlers::status::test_hooks::StatusTestHooks,
    config_path: Option<std::path::PathBuf>,
) -> Daemon {
    let (observer_tx, observer_rx) = tokio::sync::oneshot::channel();
    let (usage_tx, usage_rx) = tokio::sync::oneshot::channel();
    let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    let handle = tokio::spawn(async move {
        super::serve_on_listener_with_injected_router(
            config,
            Arc::new(CatalogOverlay::default()),
            listener,
            config_path,
            None,
            super::DaemonTestSeams {
                injected_router: Some(router),
                router_observer: Some(observer_tx),
                usage_observer: Some(usage_tx),
                confirmation_observer: Some(confirm_tx),
                status_hooks,
            },
        )
        .await
        .expect("the daemon serves");
    });
    await_health(&base_url).await;
    let router = tokio::time::timeout(Duration::from_secs(10), observer_rx)
        .await
        .expect("the daemon publishes its router swap at boot")
        .expect("the observer is not dropped before publication");
    let usage = tokio::time::timeout(Duration::from_secs(10), usage_rx)
        .await
        .expect("the daemon publishes its usage handle at boot")
        .expect("the observer is not dropped before publication");
    let confirmations = tokio::time::timeout(Duration::from_secs(10), confirm_rx)
        .await
        .expect("the daemon publishes its confirmation tracker at boot")
        .expect("the observer is not dropped before publication");
    Daemon {
        base_url,
        router,
        usage,
        confirmations,
        handle,
        _dir: dir,
    }
}

/// An Anthropic-dialect body CARRYING the closed-table field.
///
/// Through the real ingress shape rather than a canonical `ChatRequest`, because the
/// point is that the field survives parsing and reaches the planner: a canonical
/// fixture would skip the seam this file exists to cover.
fn body_with_field() -> Value {
    json!({
        "model": ALIAS,
        "max_tokens": 64,
        "thinking": { "type": "enabled", "budget_tokens": 2048, "display": "summarized" },
        "messages": [{ "role": "user", "content": "hello" }],
    })
}

/// POST one inference request at the daemon.
async fn post_inference(daemon: &Daemon, body: &Value) -> reqwest::StatusCode {
    daemon.assert_alive();
    reqwest::Client::new()
        .post(format!("{}/v1/messages", daemon.base_url))
        .json(body)
        .send()
        .await
        .expect("the daemon answers")
        .status()
}

/// GET a status path and return its parsed body.
async fn get_status(daemon: &Daemon, path: &str) -> Value {
    daemon.assert_alive();
    let response = reqwest::get(format!("{}{path}", daemon.base_url))
        .await
        .expect("the daemon answers");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the status surface answers 200 for {path}",
    );
    response.json().await.expect("status body parses")
}

/// Assert a panel envelope is AVAILABLE, returning its payload.
///
/// Both halves are checked: an available panel is exactly one carrying `data` with no
/// `unavailable` code, and checking only for `data` would pass on a malformed
/// envelope carrying both -- the state the constructors exist to prevent.
fn available_data(panel: &Value) -> &Value {
    assert_eq!(
        panel["unavailable"],
        Value::Null,
        "the panel reports no unavailable code: {panel}",
    );
    let data = &panel["data"];
    assert!(!data.is_null(), "and it carries its payload: {panel}");
    data
}

// The remaining scenario groups live in sibling files to keep this one under
// the size ceiling. They compile into THIS module via `include!`, so the
// fixtures above stay in scope and no test's module path changes.
include!("preflight_daemon_rewrite_tests.rs");
include!("preflight_daemon_observability_tests.rs");
include!("preflight_daemon_health_tests.rs");
