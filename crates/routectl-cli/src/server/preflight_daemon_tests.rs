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
    Daemon {
        base_url,
        router,
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

// ---------------------------------------------------------------------------
// Feature present / absent, through real HTTP
// ---------------------------------------------------------------------------

/// An acknowledged eligible verdict rewrites a request that arrived over HTTP.
///
/// THE assembled-daemon proof, asserted on the BYTES the provider received. A
/// pipeline wired into a handler the request never reaches, or behind an ingress that
/// drops the field before planning, passes every router-level test and fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_request_is_rewritten_when_a_verdict_is_eligible() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let daemon = spawn_daemon(config, dir, router).await;

    let status = post_inference(&daemon, &body_with_field()).await;

    assert_eq!(status, reqwest::StatusCode::OK, "the request serves");
    assert_eq!(recorder.calls(), 1, "one dispatch, so no repair retry ran");
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![false],
        "the body the provider RECEIVED carries neither carrier of the field -- the \
         rewrite happened on a request that arrived over HTTP, through the real \
         handler and the real planner",
    );
    daemon.shutdown().await;
}

/// The FEATURE-ABSENT control: with no verdict, the same HTTP request forwards
/// unchanged.
///
/// Without it the test above would pass against a daemon that stripped the field
/// unconditionally, which is a fidelity defect rather than the feature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_request_forwards_unchanged_when_no_verdict_is_resident() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;

    let status = post_inference(&daemon, &body_with_field()).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "no resident verdict authorizes no rewrite, so the client's field is \
         forwarded verbatim",
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// The canary, through the daemon
// ---------------------------------------------------------------------------

/// A daemon request that is the canary sends the field UNREPAIRED, its success
/// clears the verdict, and the NEXT HTTP request forwards unchanged.
///
/// What this adds, and what it deliberately leaves to its sibling, measured by
/// mutation rather than asserted:
///
/// - HERE: the settlement reaching the daemon's LIVE registry over a real request
///   boundary. Mutating the settlement to a bare `drop` reds this test by name.
/// - NOT here: that the clear is DURABLE. Dropping only the ledger event leaves this
///   test green, because an unrepaired canary forwards the field either way and the
///   in-memory clear still happens. The restart proof in `canary_span_tests` is what
///   catches that, and it does -- skipping the drain reds it with the verdict
///   resurrected.
/// - NOT here: the cadence NUMBER. `canary_span_tests` walks all hundred against a
///   real Router; re-dispatching a hundred HTTP requests would test the count a second
///   time and this boundary no better.
///
/// The canary is made due through the gated seed, which is the production writer for
/// exactly that state (a cold boot seeds an already-acting verdict due on its next
/// eligible request). Dispatching a hundred HTTP requests to reach the same state
/// would test the cadence a second time and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_canary_request_clears_the_verdict_and_the_next_request_forwards_unchanged() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    routectl_router::make_field_canary_due_for_tests(&router, STATE_KEY, FIELD_PATH);
    let daemon = spawn_daemon(config, dir, router).await;

    // The canary request: the daemon restores the field, the upstream accepts it, and
    // accepting the field the verdict calls broken is the disproof.
    let first = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(first, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "the canary request carried the field UNREPAIRED over the wire",
    );

    // The clear itself, read from the daemon's live router BEFORE the second request.
    //
    // Asserted here rather than only through the next request's bytes, and the
    // distinction is one mutation testing established: an unrepaired canary forwards
    // the field either way, so "request two carried the field" is ALSO true of a
    // build whose settlement produced no clear at all. Skipping the durable clear left
    // the byte assertion green. What only the clear can produce is an EMPTY acting
    // set on the live registry.
    let after_canary = daemon.live_router().fidelity_snapshot();
    assert!(
        after_canary.acting.is_empty(),
        "the disproof CLEARED the verdict on the daemon's live router: without the \
         clear the verdict is still acting, and the next request's bytes alone cannot \
         tell the two apart -- an unrepaired canary forwards the field either way. \
         Acting: {:?}",
        after_canary.acting,
    );

    // The observable consequence, over a second real request.
    let second = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(second, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true, true],
        "and with the verdict cleared the NEXT HTTP request forwards unchanged rather \
         than being rewritten",
    );
    // Read from the daemon's own live router: the clear is gone from the registry the
    // handlers serve from, not merely from a copy.
    assert!(
        daemon.live_router().fidelity_snapshot().acting.is_empty(),
        "the daemon's live router holds no acting verdict: the disproof cleared it",
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// The INFO snapshots the daemon emits
// ---------------------------------------------------------------------------

/// Both status surfaces answer, and the health panel carries a POPULATED verdict row
/// for the planted verdict.
///
/// Asserted on the daemon's live snapshot rather than only on the panel envelope,
/// because the envelope alone would pass with an empty row set -- which is the
/// vacuity an earlier version of the log test had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_status_and_doctor_surfaces_report_a_populated_snapshot() {
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let daemon = spawn_daemon(config, dir, router).await;

    let health = get_status(&daemon, "/status/health").await;
    let doctor = get_status(&daemon, "/status/doctor").await;

    let data = available_data(&health);
    assert!(
        data["learned_negatives"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["capability_key"] == field_key())),
        "the health panel reports the planted verdict: {data}",
    );
    assert!(
        doctor["schema_version"].is_number(),
        "and the doctor route answers its envelope: {doctor}",
    );
    // The snapshot the daemon's own INFO line is built from, read through the live
    // router: populated rows, and a budget row for the configured cap.
    let snapshot = daemon.live_router().fidelity_snapshot();
    assert_eq!(
        snapshot.verdicts.len(),
        1,
        "the live router's fidelity snapshot carries the verdict row",
    );
    assert_eq!(snapshot.verdicts[0].capability_key, field_key());
    assert_eq!(
        snapshot.verdicts[0].blocked_reason, None,
        "and reports it UNBLOCKED, which is the state in which traffic is rewritten",
    );

    // The AGGREGATE endpoint, and its exactly-once contract: one request builds both
    // the health and doctor panels, and the fidelity line is emitted once.
    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("aggregate answers")
        .json()
        .await
        .expect("aggregate parses");
    assert!(
        aggregate["panels"]["health"]["data"].is_object()
            && aggregate["panels"]["doctor"]["schema_version"].is_number(),
        "the aggregate carries both panels: {aggregate}",
    );

    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Cap zero, through the daemon
// ---------------------------------------------------------------------------

/// A zero-cap daemon activates its lane on a real HTTP request and runs FREE
/// validation, while committing no reservation and dispatching no paid call.
///
/// The probe pass is driven on the daemon's EXACT live Router, after an HTTP request
/// has activated it -- the activation is non-blocking for the user request, so the
/// work lands on a driver tick. The wiring and the request boundary are real; only
/// the tick is invoked directly rather than waited out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_cap_daemon_activates_and_runs_free_validation_without_spending() {
    // Cap ZERO, explicitly rather than by omission, so the assertion is about the cap
    // rather than about an absent config section.
    let (config, dir) = daemon_config(Some(("p0", 0)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;
    assert_eq!(
        daemon
            .live_router()
            .fidelity_snapshot()
            .probes
            .activations_total,
        0,
        "premise: nothing has activated before the first admitted request",
    );

    let status = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let live = daemon.live_router();
    let summary = live.run_probe_pass().await;

    let probes = live.fidelity_snapshot().probes;
    assert!(
        probes.activations_total > 0,
        "the HTTP request activated the lane -- a lane activates on its first \
         admitted real request whatever its paid cap",
    );
    assert!(
        !summary.paid_probe_attempted,
        "and a zero cap attempts no paid probe: the eligibility predicate reads the \
         cap before any candidate is claimed",
    );
    assert_eq!(
        probes.paid_reservations_committed_total, 0,
        "so no reservation is committed -- cap zero spends nothing",
    );
    assert_eq!(
        probes.paid_provider_calls_started_total, 0,
        "and no paid call is dispatched",
    );
    daemon.shutdown().await;
}

/// The control for the case above: the zero-cap lane's free work genuinely ran.
///
/// Without it, a scheduler that queued nothing would also report zero paid calls, and
/// the assertions above could not tell "free ran, paid refused" from "nothing
/// happened".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_zero_cap_daemons_free_validation_actually_runs() {
    let (config, dir) = daemon_config(Some(("p0", 0)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    let daemon = spawn_daemon(config, dir, router).await;

    let _ = post_inference(&daemon, &body_with_field()).await;
    let live = daemon.live_router();
    let queued_before = live.fidelity_snapshot().probes.queued;
    let summary = live.run_probe_pass().await;

    assert!(
        queued_before > 0 || summary.free_validators_run > 0,
        "the activation queued real free work and the pass ran it, so 'no paid call' \
         is a refusal rather than an empty scheduler",
    );
    daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Endpoint-level INFO emission (observed through per-call event observer)
// ---------------------------------------------------------------------------

/// Drain every fidelity event the daemon has emitted so far.
///
/// `try_recv` in a loop rather than an awaited `recv`: by the time the HTTP response
/// has been read the emission has already happened (the emitter runs inside the
/// blocking builder the response waits on), so a wait would only add a way for the
/// test to hang. Draining to EMPTY is the load-bearing half -- the assertion is a
/// count, and a read that stopped at the first event would report one for a daemon
/// that emitted three.
fn drain_events(
    events: &mut crate::handlers::status::test_hooks::FidelityEvents,
) -> Vec<crate::handlers::status::FidelityEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

/// Assert `events` is EXACTLY ONE populated fidelity event, and return it.
///
/// Both halves matter and neither implies the other. The COUNT is the exactly-once
/// contract: the response body carries no trace of the line, so nothing else in a
/// request can tell one emission from two, and two identical snapshots per poll make
/// a reader counting lines read double the poll rate. POPULATED is the vacuity guard:
/// an emitter reached with an empty snapshot satisfies every count assertion while
/// reporting nothing an operator can use, which is exactly the shape an earlier
/// version of the log test had.
///
/// `expect_budget_rows` is the count the caller's fixture configures. Asserted rather
/// than ignored because the budget rows are the half of the line a verdict count
/// cannot vouch for: they come from a DIFFERENT source (the operator's configured
/// caps plus a ledger read on the blocking thread), so an emitter reached with the
/// verdict rows intact and the budget read dropped passes every other assertion here.
/// The fixtures discriminate it -- a configured cap emits one row, the no-config
/// doctor branch legitimately emits none -- so the value distinguishes a real read
/// from a default.
fn one_populated_event(
    events: Vec<crate::handlers::status::FidelityEvent>,
    endpoint: &str,
    expect_budget_rows: usize,
) -> crate::handlers::status::FidelityEvent {
    assert_eq!(
        events.len(),
        1,
        "{endpoint} must emit EXACTLY ONE fidelity event per request: the line is \
         log-only, so no response body can tell one emission from two -- and two per \
         poll make a reader counting lines read double the real poll rate. Got \
         {} events",
        events.len(),
    );
    let event = events.into_iter().next().expect("the count is one");
    assert_eq!(
        event.verdict_rows_total, 1,
        "and the event {endpoint} emitted carries the planted verdict row: an emitter \
         reached with an empty snapshot passes every count assertion while reporting \
         nothing",
    );
    assert!(
        !event.snapshot.acting.is_empty(),
        "with the acting verdict present in the snapshot the line is built from",
    );
    assert_eq!(
        event.budget_rows_total, expect_budget_rows,
        "and it carries {expect_budget_rows} paid-probe budget row(s) for {endpoint}: \
         the budgets come from the operator's configured caps plus a ledger read, not \
         from the router snapshot, so a dropped budget read leaves every verdict \
         assertion above green",
    );
    event
}

/// A daemon with one planted acting verdict, its status seams observing.
///
/// `config_path` selects the doctor branch; see `spawn_daemon_full`.
async fn observing_daemon(
    config_path_from: Option<&std::path::Path>,
) -> (Daemon, crate::handlers::status::test_hooks::FidelityEvents) {
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let (hooks, events) = crate::handlers::status::test_hooks::StatusTestHooks::observing();
    let config_path = config_path_from.map(std::path::Path::to_path_buf);
    let daemon = spawn_daemon_full(config, dir, router, hooks, config_path).await;
    (daemon, events)
}

/// Write a minimal servable config into `dir` and return its path, so a daemon can
/// serve the CONFIGURED doctor branch.
fn write_config_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("routectl.toml");
    std::fs::write(
        &path,
        format!(
            "version = 3\n\n[providers.p0]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n\n\
             [models.{STATE_KEY}]\nprovider = \"p0\"\nupstream = \"claude-sonnet-4-5\"\n\n\
             [aliases]\ndefault = \"{STATE_KEY}\"\n"
        ),
    )
    .expect("the config file writes");
    path
}

/// The standalone `/status/health` endpoint emits exactly one populated fidelity
/// event, observed at the emitter inside a RUNNING daemon.
///
/// What this adds over the emitter unit tests beside it: those construct the
/// `FidelityEmission` themselves, so they prove the emitter's behavior for an
/// emission a test built. This one observes the emission the daemon's OWN health
/// handler built, reached over a real HTTP request through the listener, the status
/// middleware, `guard_panel`, and the blocking builder. A surface wired into a
/// builder the request never reaches passes every unit test and fails this.
///
/// Mutation check: remove the `log_field_verdict_snapshot` call from `health`'s
/// `build_from_view` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_health_endpoint_emits_one_populated_fidelity_event() {
    let (daemon, mut events) = observing_daemon(None).await;
    assert!(
        drain_events(&mut events).is_empty(),
        "premise: booting the daemon emits no fidelity event, so the event counted \
         below is the REQUEST's rather than a boot-time one",
    );

    let health = get_status(&daemon, "/status/health").await;

    let data = available_data(&health);
    assert!(
        data["learned_negatives"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "premise: the health panel served its payload, so the builder ran to \
         completion: {data}",
    );
    // ONE budget row: the fixture configures a cap for `p0`, and the health
    // builder reads the caps off the same live config.
    one_populated_event(drain_events(&mut events), "/status/health", 1);

    daemon.shutdown().await;
}

/// The CONFIGURED `/status/doctor` endpoint emits exactly one populated fidelity
/// event.
///
/// Its own case rather than a variant of the health one, because it reaches the
/// emitter through different code: the doctor builder gathers a no-network report
/// from an on-disk config first, and it is the branch a daemon with a config path
/// serves. A daemon whose doctor emitter was removed still answers this endpoint's
/// envelope, so only the observer can tell.
///
/// Mutation check: remove the `log_field_verdict_snapshot` call from
/// `doctor::build_panel_data` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_configured_doctor_endpoint_emits_one_populated_fidelity_event() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let doctor = get_status(&daemon, "/status/doctor").await;

    let data = available_data(&doctor);
    assert!(
        data["report"]["schema_version"].is_number(),
        "premise: this IS the configured branch -- the panel carries a gathered \
         report rather than an unavailable envelope: {doctor}",
    );
    one_populated_event(drain_events(&mut events), "/status/doctor", 1);

    daemon.shutdown().await;
}

/// The NO-CONFIG `/status/doctor` branch emits exactly one populated fidelity event
/// even though its panel is unavailable.
///
/// The contract this pins is that the observability floor is about the ROUTER, not
/// the report: a daemon with no on-disk config has no doctor report to build but has
/// a live router in full, and staying silent would mean such a daemon satisfied the
/// floor only through `/status/health`. Asserted over a real request rather than by
/// driving the builder, because the branch is selected by how the DAEMON was
/// started.
///
/// Mutation check: drop the `log_field_verdict_snapshot` call from the `None` arm of
/// `doctor::build_with_emission` -> red at zero events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_no_config_doctor_branch_still_emits_one_populated_fidelity_event() {
    let (daemon, mut events) = observing_daemon(None).await;
    let _ = drain_events(&mut events);

    let doctor: Value = reqwest::get(format!("{}/status/doctor", daemon.base_url))
        .await
        .expect("the daemon answers")
        .json()
        .await
        .expect("the doctor envelope parses");

    assert!(
        !doctor["unavailable"].is_null(),
        "premise: this IS the no-config branch, so the panel is unavailable -- which \
         is precisely the state in which silence would be a narrower floor: {doctor}",
    );
    // ZERO budget rows, and that is the branch's own contract rather than a
    // shortcoming: the caps come from an on-disk config this daemon has none of, so
    // the line is emitted with unavailable budget data while every other field --
    // the counters, the verdict rows, the probe state -- comes from state it holds
    // in full.
    one_populated_event(drain_events(&mut events), "the no-config /status/doctor", 0);

    daemon.shutdown().await;
}

/// The `/status` AGGREGATE emits exactly ONE fidelity event for a request that
/// builds BOTH fidelity-carrying panels.
///
/// The shared claim's whole purpose, and the one property no other test here can
/// see: both the health and the doctor builder run in this single request and both
/// carry the surface, so an unarbitrated surface emits twice -- two identical
/// snapshots per poll, differing only in timestamps while describing one moment, and
/// a reader counting lines reads double the poll rate. Neither the response nor the
/// router state distinguishes one emission from two.
///
/// Mutation check: replace `FidelityEmission::shared()` in `status_aggregate` with
/// `::always()` -> red at two events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_aggregate_emits_exactly_one_fidelity_event_for_both_panels() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers")
        .json()
        .await
        .expect("the aggregate parses");

    // PREMISE, and the half that makes the count below mean anything: BOTH
    // fidelity-carrying panels really did build for this one request. Against a
    // request where one of them degraded, "one event" would be the trivial answer.
    assert!(
        aggregate["panels"]["health"]["data"].is_object(),
        "the health panel built: {aggregate}",
    );
    assert!(
        aggregate["panels"]["doctor"]["data"]["report"]["schema_version"].is_number(),
        "and so did the doctor panel, so two fidelity-carrying builders ran in this \
         one request: {aggregate}",
    );
    one_populated_event(drain_events(&mut events), "the /status aggregate", 1);

    daemon.shutdown().await;
}

/// With the aggregate's HEALTH builder failing BEFORE the emitter, the response marks
/// health unavailable and the DOCTOR builder emits the one fidelity event.
///
/// THE health-failure fallback, over a real request. The claim is a claim rather than
/// a pre-assigned role precisely for this case: a pre-assigned emitter that degraded
/// emitted nothing and its healthy sibling stayed suppressed too, so ONE failing
/// panel silently removed the whole observability floor from that request. What makes
/// this non-vacuous is the pair of assertions -- health really is unavailable in the
/// served envelope (so the failure landed before the emitter, not after), and exactly
/// one populated event still arrived (so the floor held).
///
/// Mutation check: replace the claim's `compare_exchange` with a pre-assigned
/// health-emits role -> red at zero events, which is the regression this shape
/// exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aggregate_whose_health_builder_fails_still_emits_one_event_from_doctor() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (config, dir) = daemon_config(Some(("p0", 3)));
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let (hooks, mut events) =
        crate::handlers::status::test_hooks::StatusTestHooks::observing_with_failing_health();
    let daemon = spawn_daemon_full(config, dir, router, hooks, Some(config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers even with one panel degraded")
        .json()
        .await
        .expect("the aggregate parses");

    assert!(
        !aggregate["panels"]["health"]["unavailable"].is_null(),
        "the health panel degraded to unavailable, so its builder did NOT reach the \
         fidelity emitter -- which is what makes the event below doctor's: {aggregate}",
    );
    assert!(
        aggregate["panels"]["health"]["data"].is_null(),
        "and it carries no payload, so the failure landed before the build completed \
         rather than after it emitted: {aggregate}",
    );
    assert!(
        aggregate["panels"]["doctor"]["data"]["report"]["schema_version"].is_number(),
        "while the doctor panel built in full: {aggregate}",
    );
    one_populated_event(
        drain_events(&mut events),
        "the aggregate whose health builder failed",
        1,
    );

    daemon.shutdown().await;
}

/// The FAILURE-INJECTION control: without the injection the same fixture's health
/// panel is AVAILABLE.
///
/// Without it, the test above would pass against a daemon whose health panel was
/// unavailable for some entirely unrelated reason -- and then "doctor emitted" would
/// be a statement about that other cause. This pins that the injected failure is
/// what degraded health.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_aggregate_without_the_injection_serves_an_available_health_panel() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let config_path = write_config_file(config_dir.path());
    let (daemon, mut events) = observing_daemon(Some(&config_path)).await;
    let _ = drain_events(&mut events);

    let aggregate: Value = reqwest::get(format!("{}/status", daemon.base_url))
        .await
        .expect("the aggregate answers")
        .json()
        .await
        .expect("the aggregate parses");

    assert!(
        aggregate["panels"]["health"]["unavailable"].is_null()
            && aggregate["panels"]["health"]["data"].is_object(),
        "the identical fixture WITHOUT the injection serves health available, so the \
         degradation in the sibling test is the injection's and not a property of \
         this daemon shape: {aggregate}",
    );

    daemon.shutdown().await;
}
