//! The five per-request observation counts, end to end through the assembled
//! daemon: real HTTP into its ingress, the real router and egress, the real
//! usage writer, and the ledger read back.
//!
//! Each single-source case drives one request that moves exactly one count
//! and reconciles three views of it: the live counter's delta, the row's
//! column, and the ledger's `observation_totals` delta. The other four must
//! read 0 in all three.
//!
//! Two of the live counters are process-wide (the translation-drop registry
//! and the missing-terminal total), and tests elsewhere in this binary move
//! them without any shared serial key. A delta read in this process could
//! therefore absorb another test's request. Each case re-runs itself as a
//! child invocation of this test binary that runs only that case, so its
//! process holds no other traffic. The per-router counters (wire losses,
//! local refusals, the repairs histogram) are read on the daemon's own
//! router.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::stream_terminal_metrics::missing_terminal_total;
use routectl_providers::translation_drop_metrics::translation_drop_snapshot;
use routectl_router::{
    AliasValue, CatalogOverlay, Config, ModelEntry, ProviderEntry, ResolvedModel, RetryPolicy,
    Router,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) const ALIAS: &str = "a";
pub(super) const NICKNAME: &str = "m";
pub(super) const PROVIDER: &str = "p";
pub(super) const UPSTREAM: &str = "upstream";
/// The upstream model the field-repair table has rows for.
const REPAIRABLE_UPSTREAM: &str = "claude-sonnet-4-5";
pub(super) const WAIT: Duration = Duration::from_secs(10);

/// The five per-request observation counts, in one fixed order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) wire_losses: i64,
    pub(super) translation_drops: i64,
    pub(super) local_refusals: i64,
    pub(super) field_repairs: i64,
    pub(super) missing_terminals: i64,
}

impl Counts {
    const fn minus(self, before: Self) -> Self {
        Self {
            wire_losses: self.wire_losses - before.wire_losses,
            translation_drops: self.translation_drops - before.translation_drops,
            local_refusals: self.local_refusals - before.local_refusals,
            field_repairs: self.field_repairs - before.field_repairs,
            missing_terminals: self.missing_terminals - before.missing_terminals,
        }
    }
}

fn as_count(n: u64) -> i64 {
    i64::try_from(n).expect("a counter inside i64")
}

/// The live counters the daemon's router and this process hold right now.
/// Field repairs are the histogram's weighted sum: bucket `n` counts the
/// requests that spent exactly `n` repairs.
fn live_counts(router: &Router) -> Counts {
    let wire = router.wire_check_snapshot();
    Counts {
        wire_losses: as_count(wire.loss.iter().map(|c| c.reading.count).sum()),
        translation_drops: as_count(
            translation_drop_snapshot()
                .iter()
                .map(|e| e.drop_count)
                .sum(),
        ),
        local_refusals: as_count(wire.local_refusals.iter().map(|c| c.reading.count).sum()),
        field_repairs: as_count(
            router
                .repairs_per_request()
                .iter()
                .zip(0_u64..)
                .map(|(requests, n)| n * requests)
                .sum(),
        ),
        missing_terminals: as_count(missing_terminal_total()),
    }
}

/// A running daemon serving an injected router over its own ledger.
pub(super) struct Daemon {
    pub(super) base_url: String,
    pub(super) usage_db: std::path::PathBuf,
    router: Arc<ArcSwap<Router>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// Serve `router` with `config`, whose `usage.db_path` is already set.
    pub(super) async fn spawn(config: Config, router: Router) -> Self {
        let usage_db = config.usage.db_path.clone();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
        let (observer_tx, observer_rx) = tokio::sync::oneshot::channel();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let handle = tokio::spawn(async move {
            super::serve::serve_on_listener_with_injected_router(
                Arc::new(config),
                Arc::new(CatalogOverlay::default()),
                listener,
                None,
                Some(secrets),
                super::serve::DaemonTestSeams {
                    injected_router: Some(router),
                    router_observer: Some(observer_tx),
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
        let daemon = Self {
            base_url,
            usage_db,
            router,
            handle,
        };
        daemon.await_health().await;
        daemon
    }

    async fn await_health(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn live_router(&self) -> Arc<Router> {
        self.router.load_full()
    }

    /// POST `body` to the Anthropic ingress under `request_id`.
    pub(super) fn messages(&self, request_id: &str, body: &Value) -> reqwest::RequestBuilder {
        assert!(!self.handle.is_finished(), "the daemon exited mid-test");
        reqwest::Client::new()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("x-request-id", request_id)
            .json(body)
    }

    /// Wait until the writer has committed the row for `request_id`.
    pub(super) async fn await_row(&self, request_id: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !row_exists(&self.usage_db, request_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no ledger row for {request_id}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The five counts the ledger sums over every row, read through
    /// `observation_totals`.
    fn ledger_counts(&self) -> Counts {
        let db = routectl_usage::open_readonly(&self.usage_db).expect("read-only open");
        let totals =
            routectl_usage::observation_totals(&db, 0, i64::MAX).expect("observation totals");
        Counts {
            wire_losses: totals.wire_losses,
            translation_drops: totals.translation_drops,
            local_refusals: totals.local_refusals,
            field_repairs: totals.field_repairs,
            missing_terminals: totals.missing_terminals,
        }
    }

    pub(super) async fn shutdown(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

fn row_exists(db_path: &std::path::Path, request_id: &str) -> bool {
    let Ok(db) = routectl_usage::open_readonly(db_path) else {
        return false;
    };
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM requests WHERE request_id = ?1",
            [request_id],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
}

/// The five count columns of `request_id`'s row.
pub(super) fn row_counts(db_path: &std::path::Path, request_id: &str) -> Counts {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    db.conn()
        .query_row(
            "SELECT wire_losses, translation_drops, local_refusals, field_repairs, \
                    missing_terminals FROM requests WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok(Counts {
                    wire_losses: r.get(0)?,
                    translation_drops: r.get(1)?,
                    local_refusals: r.get(2)?,
                    field_repairs: r.get(3)?,
                    missing_terminals: r.get(4)?,
                })
            },
        )
        .expect("exactly one row")
}

/// One alias, one model, one provider, one attempt per target, with its
/// ledger at `usage_db`.
///
/// The ledger path is set BEFORE the router is built from this config: the
/// status panels read it off the router's own copy, so a path set only on
/// the copy handed to the daemon would leave `/status/usage` reading a
/// different file than the writer fills.
pub(super) fn single_target_config(
    provider: ProviderEntry,
    upstream: &str,
    usage_db: &std::path::Path,
) -> Config {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers: BTreeMap::from([(PROVIDER.to_string(), provider)]),
        models: BTreeMap::from([(NICKNAME.to_string(), ModelEntry::new(PROVIDER, upstream))]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    config.usage.db_path = usage_db.to_path_buf();
    config
}

/// A fresh tempdir holding this test's ledger path.
pub(super) fn usage_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("usage tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

/// A config and router over an anthropic-api provider at `origin`.
pub(super) async fn anthropic_api_at(origin: &str, usage_db: &std::path::Path) -> (Config, Router) {
    built_router(
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")).with_base_url(origin),
        usage_db,
    )
    .await
}

async fn built_router(provider: ProviderEntry, usage_db: &std::path::Path) -> (Config, Router) {
    let config = single_target_config(provider, UPSTREAM, usage_db);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = super::build_router_from_config(Arc::new(config.clone()), secrets)
        .await
        .expect("build router");
    (config, router)
}

pub(super) fn plain_body(stream: bool) -> Value {
    json!({
        "model": ALIAS, "max_tokens": 16, "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// A body carrying one more cache breakpoint than prompt caching allows,
/// which the anthropic-api egress refuses locally.
pub(super) fn over_breakpoint_cap_body() -> Value {
    let block = json!({"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}});
    let content: Vec<Value> = (0..=routectl_core::cache_control::MAX_BREAKPOINTS)
        .map(|_| block.clone())
        .collect();
    json!({"model": ALIAS, "max_tokens": 16,
           "messages": [{"role": "user", "content": content}]})
}

pub(super) fn anthropic_message() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn chat_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": UPSTREAM,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }))
}

/// The opening frames of an Anthropic stream: a message and some text,
/// but never `message_stop`.
pub(super) fn stream_without_terminal() -> String {
    let frames = [
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": UPSTREAM,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
    ];
    frames
        .iter()
        .map(|data| {
            format!(
                "event: {}\ndata: {data}\n\n",
                data["type"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

pub(super) async fn upstream_answering(answer: ResponseTemplate) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer)
        .mount(&upstream)
        .await;
    upstream
}

/// The committed anthropic-api capture of an adaptive-only model refusing
/// `thinking.type: disabled`.
fn captured_disabled_thinking_rejection() -> ResponseTemplate {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../routectl-router/tests/fixtures/rejections/anthropic-api/\
         thinking-type-disabled-unsupported.json",
    );
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture).expect("fixture reads"))
            .expect("fixture parses");
    let status =
        u16::try_from(fixture["status"].as_u64().expect("fixture status")).expect("an HTTP status");
    let body = fixture["body"].as_str().expect("fixture body").to_string();
    ResponseTemplate::new(status).set_body_raw(body, "application/json")
}

/// A config on the remote Anthropic base URL (which the field-repair
/// attributability gate reads) and a router whose real anthropic-api
/// provider dials that host on `upstream`'s port, resolved to `upstream`.
fn repairable_lane(upstream: &MockServer, usage_db: &std::path::Path) -> (Config, Router) {
    let config = single_target_config(
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k"))
            .with_base_url("https://api.anthropic.com"),
        REPAIRABLE_UPSTREAM,
        usage_db,
    );
    let addr = *upstream.address();
    let provider = AnthropicApiProvider::new(AnthropicApiConfig {
        id: PROVIDER.into(),
        auth: Arc::new(routectl_core::StaticToken::new("reconcile-key")),
        base_url: format!("http://api.anthropic.com:{}", addr.port()),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        mantle: None,
    })
    .with_host_resolved_to_for_tests(addr);
    let mut router = Router::new(Arc::new(config.clone()));
    router.install_resolved_models(BTreeMap::from([(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            PROVIDER,
            Arc::new(provider) as Arc<dyn routectl_core::Provider>,
            REPAIRABLE_UPSTREAM,
        )),
    )]));
    (config, router)
}

/// One single-source case: what it serves, what it sends, and the one
/// count it must move.
#[derive(Debug, Clone, Copy)]
enum Case {
    WireLoss,
    TranslationDrop,
    LocalRefusal,
    FieldRepair,
    MissingTerminal,
}

impl Case {
    const fn request_id(self) -> &'static str {
        match self {
            Self::WireLoss => "reconcile-wire-loss",
            Self::TranslationDrop => "reconcile-translation-drop",
            Self::LocalRefusal => "reconcile-local-refusal",
            Self::FieldRepair => "reconcile-field-repair",
            Self::MissingTerminal => "reconcile-missing-terminal",
        }
    }

    const fn expected(self) -> Counts {
        let zero = Counts {
            wire_losses: 0,
            translation_drops: 0,
            local_refusals: 0,
            field_repairs: 0,
            missing_terminals: 0,
        };
        match self {
            Self::WireLoss => Counts {
                wire_losses: 1,
                ..zero
            },
            Self::TranslationDrop => Counts {
                translation_drops: 1,
                ..zero
            },
            Self::LocalRefusal => Counts {
                local_refusals: 1,
                ..zero
            },
            Self::FieldRepair => Counts {
                field_repairs: 1,
                ..zero
            },
            Self::MissingTerminal => Counts {
                missing_terminals: 1,
                ..zero
            },
        }
    }

    /// The upstream, config and router this case serves from.
    async fn lane(self, usage_db: &std::path::Path) -> (MockServer, Config, Router) {
        match self {
            Self::WireLoss => {
                let upstream = upstream_answering(anthropic_message()).await;
                let (config, router) = anthropic_api_at(&upstream.uri(), usage_db).await;
                (
                    upstream,
                    config,
                    router.with_wire_tap_drop_for_tests("/max_tokens"),
                )
            }
            Self::TranslationDrop => {
                let upstream = upstream_answering(chat_completion()).await;
                let (config, router) = built_router(
                    ProviderEntry::openai_compat(upstream.uri(), crate::test_secret::file_ref("k")),
                    usage_db,
                )
                .await;
                (upstream, config, router)
            }
            Self::LocalRefusal => {
                let upstream = upstream_answering(anthropic_message()).await;
                let (config, router) = anthropic_api_at(&upstream.uri(), usage_db).await;
                (upstream, config, router)
            }
            Self::FieldRepair => {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .and(body_partial_json(json!({"thinking": {"type": "disabled"}})))
                    .respond_with(captured_disabled_thinking_rejection())
                    .with_priority(1)
                    .mount(&upstream)
                    .await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(anthropic_message())
                    .mount(&upstream)
                    .await;
                let (config, router) = repairable_lane(&upstream, usage_db);
                (upstream, config, router)
            }
            Self::MissingTerminal => {
                let upstream = upstream_answering(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(stream_without_terminal()),
                )
                .await;
                let (config, router) = anthropic_api_at(&upstream.uri(), usage_db).await;
                (upstream, config, router)
            }
        }
    }

    /// Send this case's one request and check the status it must draw.
    async fn send(self, daemon: &Daemon) {
        let id = self.request_id();
        let (request, expected) = match self {
            Self::WireLoss => (daemon.messages(id, &plain_body(false)), 200),
            Self::TranslationDrop => (
                daemon
                    .messages(id, &plain_body(false))
                    .header("anthropic-beta", "kept-2099-01-01"),
                200,
            ),
            Self::LocalRefusal => (daemon.messages(id, &over_breakpoint_cap_body()), 400),
            Self::FieldRepair => (
                daemon.messages(
                    id,
                    &json!({
                        "model": ALIAS, "max_tokens": 64, "stream": false,
                        "thinking": {"type": "disabled"},
                        "messages": [{"role": "user", "content": "hello"}],
                    }),
                ),
                200,
            ),
            Self::MissingTerminal => (daemon.messages(id, &plain_body(true)), 200),
        };
        let response = request.send().await.expect("the daemon answers");
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        assert_eq!(status, expected, "{self:?}: {body}");
    }
}

/// Drive `case` through a fresh daemon and reconcile its three views.
async fn reconcile(case: Case) {
    // Arrange
    let (_usage_dir, usage_db) = usage_dir();
    let (_upstream, config, router) = case.lane(&usage_db).await;
    let daemon = Daemon::spawn(config, router).await;
    let live_before = live_counts(&daemon.live_router());
    let ledger_before = daemon.ledger_counts();

    // Act
    case.send(&daemon).await;
    daemon.await_row(case.request_id()).await;

    // Assert
    let live = live_counts(&daemon.live_router()).minus(live_before);
    let row = row_counts(&daemon.usage_db, case.request_id());
    let ledger = daemon.ledger_counts().minus(ledger_before);
    daemon.shutdown().await;
    let expected = case.expected();
    assert_eq!(live, expected, "{case:?}: live counter delta");
    assert_eq!(row, expected, "{case:?}: row columns");
    assert_eq!(ledger, expected, "{case:?}: observation_totals delta");
}

/// Set only in a child's environment, naming the one case it runs.
const CHILD_ENV: &str = "ROUTECTL_OBSERVATION_RECONCILE_CHILD";
/// Printed by a child after its case passed, so the parent can tell a child
/// that ran the case from one whose filter selected nothing (libtest exits
/// 0 on an empty selection).
const CHILD_PASSED: &str = "observation-reconcile-child-passed";

/// Run `case` alone in a child invocation of this test binary, or, inside
/// that child, run it here.
fn in_own_process(test: &'static str, case: Case) {
    if std::env::var_os(CHILD_ENV).is_some_and(|named| named == test) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(reconcile(case));
        // libtest has already printed `test <name> ... ` without a newline.
        println!("\n{CHILD_PASSED}");
        return;
    }
    let module = module_path!()
        .split_once("::")
        .map_or("", |(_crate, rest)| rest);
    let program = std::env::current_exe().expect("the running test binary");
    let run = std::process::Command::new(&program)
        .args([
            &format!("{module}::{test}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, test)
        .output()
        .unwrap_or_else(|why| panic!("run child {}: {why}", program.display()));
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success() && stdout.lines().any(|line| line == CHILD_PASSED),
        "the {test} child must pass and print its marker; status {}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        run.status
    );
}

#[test]
#[serial_test::serial(observation_reconcile)]
fn a_wire_loss_reconciles_live_row_and_ledger_with_the_other_counts_zero() {
    in_own_process(
        "a_wire_loss_reconciles_live_row_and_ledger_with_the_other_counts_zero",
        Case::WireLoss,
    );
}

#[test]
#[serial_test::serial(observation_reconcile)]
fn a_translation_drop_reconciles_live_row_and_ledger_with_the_other_counts_zero() {
    in_own_process(
        "a_translation_drop_reconciles_live_row_and_ledger_with_the_other_counts_zero",
        Case::TranslationDrop,
    );
}

#[test]
#[serial_test::serial(observation_reconcile)]
fn a_local_refusal_reconciles_live_row_and_ledger_with_the_other_counts_zero() {
    in_own_process(
        "a_local_refusal_reconciles_live_row_and_ledger_with_the_other_counts_zero",
        Case::LocalRefusal,
    );
}

#[test]
#[serial_test::serial(observation_reconcile)]
fn a_field_repair_reconciles_live_row_and_ledger_with_the_other_counts_zero() {
    in_own_process(
        "a_field_repair_reconciles_live_row_and_ledger_with_the_other_counts_zero",
        Case::FieldRepair,
    );
}

#[test]
#[serial_test::serial(observation_reconcile)]
fn a_missing_terminal_reconciles_live_row_and_ledger_with_the_other_counts_zero() {
    in_own_process(
        "a_missing_terminal_reconciles_live_row_and_ledger_with_the_other_counts_zero",
        Case::MissingTerminal,
    );
}

/// Two requests in flight at once, each moving a different count: the held
/// one loses a member on the wire, the other is refused locally and
/// finishes while the first is still waiting on its upstream. Each row
/// carries only its own count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_interleaved_requests_each_persist_only_their_own_counts() {
    // Arrange
    let upstream = upstream_answering(anthropic_message().set_delay(Duration::from_secs(3))).await;
    let (_usage_dir, usage_db) = usage_dir();
    let (config, router) = anthropic_api_at(&upstream.uri(), &usage_db).await;
    let daemon = Daemon::spawn(config, router.with_wire_tap_drop_for_tests("/max_tokens")).await;
    let held = daemon.messages("interleaved-held", &plain_body(false));
    let refused = daemon.messages("interleaved-refused", &over_breakpoint_cap_body());

    // Act: the held request reaches its upstream, then the refused one runs
    // start to finish beside it.
    let held = tokio::spawn(async move { held.send().await.expect("answers").status() });
    let deadline = tokio::time::Instant::now() + WAIT;
    while upstream
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held request never reached upstream"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let refused_status = refused.send().await.expect("answers").status();
    let held_was_in_flight = !held.is_finished();
    let held_status = held.await.expect("held request task");
    daemon.await_row("interleaved-held").await;
    daemon.await_row("interleaved-refused").await;

    // Assert
    assert!(held_was_in_flight, "premise: the two requests overlapped");
    assert_eq!(refused_status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(held_status, reqwest::StatusCode::OK);
    assert_eq!(
        row_counts(&daemon.usage_db, "interleaved-held"),
        Counts {
            wire_losses: 1,
            ..Counts::default()
        }
    );
    assert_eq!(
        row_counts(&daemon.usage_db, "interleaved-refused"),
        Counts {
            local_refusals: 1,
            ..Counts::default()
        }
    );
    daemon.shutdown().await;
}
