//! A streamed reply's response evidence through the real stream walk: a real
//! router dispatch, its evidence carrier, `drive_stream`, and the settle at the
//! natural end. Only a stream delivered to the client in full, end-of-stream
//! frames included, may admit an observation, and it admits verified ones
//! only, read through the router published when it settles.
//!
//! Each row also reads the carrier's own natural-end flag through a clone
//! taken at dispatch, so the router guard and the CLI settle position are
//! pinned independently: either one alone keeps every non-natural row at zero.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use futures::stream::{BoxStream, StreamExt as _};
use routectl_core::capability::{STRUCTURED_OUTPUT, Verdict};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
    TranslationKey, TranslationReason,
};
use routectl_router::{
    BoundaryCut, Config, DispatchedStream, ResolvedModel, Router, RouterOptions, StreamEvidence,
};
use routectl_usage::{UsageCounters, UsageHandle};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use super::{DispatchFut, warm_render_task};
use crate::handlers::opening_meter::StreamTurn;
use crate::handlers::stream_evidence_settle::{
    StreamEvidenceCounters, StreamEvidenceSettler, StreamEvidenceSlot,
};
use crate::handlers::usage_capture::{UsageCapture, build_usage_draft};
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamRequestContext,
};
use crate::server::confirmation_advance::ConfirmationTracker;

// --- upstream script -------------------------------------------------------

fn chunk(index: u32, content: Option<&str>, finish_reason: Option<&str>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index,
            delta: ChunkDelta {
                content: content.map(str::to_string),
                ..ChunkDelta::default()
            },
            finish_reason: finish_reason.map(str::to_string),
            matched_stop_sequence: None,
        }],
        ..ChatChunk::default()
    }
}

fn text(t: &str) -> ChatChunk {
    chunk(0, Some(t), None)
}

fn stop(reason: &str) -> ChatChunk {
    chunk(0, None, Some(reason))
}

/// The reply every clean row streams: it satisfies the requested schema, so
/// a clean stop yields exactly one verified structured-output observation.
fn conforming() -> Vec<ChatChunk> {
    vec![text(r#"{"na"#), text(r#"me":"ok"}"#), stop("stop")]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    End,
    Error,
}

/// A provider streaming `chunks`. With a `gate`, the stream yields its first
/// chunk and then waits for the gate before the rest, so a test can act
/// mid-stream; a gate never opened holds the stream open.
struct Scripted {
    chunks: Vec<ChatChunk>,
    tail: Tail,
    gate: Option<Arc<Notify>>,
    untranslated: Option<&'static str>,
}

#[async_trait::async_trait]
impl Provider for Scripted {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p1", 500, "unused"))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        if self.chunks.is_empty() {
            return Err(Error::upstream("p1", 500, "refused before content"));
        }
        if let (Some(key), Some(report)) = (
            self.untranslated,
            req.routectl_internal.translation_report.as_ref(),
        ) {
            let key = TranslationKey::from_tool_type(key).expect("token-shaped key");
            report.record(TranslationReason::TranslationRejected(key));
        }
        let mut items: Vec<Result<ChatChunk>> = self.chunks.iter().cloned().map(Ok).collect();
        if self.tail == Tail::Error {
            items.push(Err(Error::upstream("p1", 502, "mid-stream failure")));
        }
        let head = futures::stream::iter([items.remove(0)]);
        let gate = self.gate.clone();
        let rest = futures::stream::once(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            futures::stream::iter(items)
        })
        .flatten();
        let s = head.chain(rest);
        Ok(s.boxed())
    }
}

const OPENAI_P1: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

const OPENAI_P1_DISABLED: &str = r#"
[capability]
enabled = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

const ANTHROPIC_P1_FORWARDED: &str = r#"
[providers.p1]
kind = "anthropic-api"
base_url = "https://example.test"
credential_source = "forwarded"
"#;

fn router_over(config_toml: &str, provider: &Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(config_toml).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            Arc::clone(provider),
            "wire-model",
        )),
    );
    router.install_resolved_models(models);
    router
}

/// A strict-output request whose schema the conforming reply satisfies.
fn strict_request() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        stream: Some(true),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": { "type": "object", "required": ["name"] }
                }
            }
        })),
        ..ChatRequest::default()
    }
}

// --- client side -----------------------------------------------------------

type SharedRx = Arc<Mutex<Option<mpsc::Receiver<SseEvent>>>>;

/// The OpenAI ingress, optionally failing the render of chunk `fail_at`, or
/// dropping the client's receiver the moment end-of-stream is rendered (so
/// the end-of-stream send is the one that fails).
struct ClientAdapter {
    inner: OpenAiIngress,
    calls: std::sync::atomic::AtomicUsize,
    fail_at: Option<usize>,
    drop_rx_at_eos: Option<SharedRx>,
}

impl IngressAdapter for ClientAdapter {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn dialect(&self) -> routectl_core::wire_check::IngressDialect {
        self.inner.dialect()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        self.inner.error_envelope_shape()
    }
    fn parse_request(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> Result<ChatRequest> {
        self.inner.parse_request(headers, body)
    }
    fn render_response(&self, resp: ChatResponse) -> Result<bytes::Bytes> {
        self.inner.render_response(resp)
    }
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        self.inner.new_stream_state(ctx)
    }
    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_at == Some(n) {
            return Err(Error::Streaming("synthetic render failure".into()));
        }
        self.inner.render_chunk(chunk, state)
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        if let Some(rx) = &self.drop_rx_at_eos {
            drop(rx.lock().expect("rx lock").take());
        }
        self.inner.render_eos(state)
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &crate::ingress::StreamErrorClass,
    ) -> Vec<SseEvent> {
        self.inner.render_error_eos(state, error, class)
    }
}

/// How the client consumes the stream, and what the test does mid-stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    /// Read to the end.
    ReadAll,
    /// Read the first event, then hang up.
    HangUpAfterFirst,
    /// Hang up before the dispatch is even awaited.
    GoneBeforeContent,
    /// Hang up while end-of-stream is being sent.
    HangUpAtEos,
    /// Read the first event, publish a replacement router, then read on.
    ReloadAfterFirst(Reload),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reload {
    /// The replacement turns the capability subsystem off; the registry
    /// generation does not move.
    KillSwitchOff,
    /// The replacement crosses a catalog boundary: the generation advances.
    CatalogBoundary,
}

struct Row {
    name: &'static str,
    config: &'static str,
    chunks: Vec<ChatChunk>,
    tail: Tail,
    request: ChatRequest,
    client: Client,
    fail_render_at: Option<usize>,
    untranslated: Option<&'static str>,
    /// Whether the dispatch hands out a carrier at all.
    carries: bool,
    /// The carrier's own natural-end flag once the walk is over.
    natural_end: bool,
    /// Verified entries and ledger rows the row ends with.
    observations: usize,
    /// The stream-evidence counter cells the row leaves.
    counted: Vec<(&'static str, &'static str, u64)>,
}

impl Row {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            config: OPENAI_P1,
            chunks: conforming(),
            tail: Tail::End,
            request: strict_request(),
            client: Client::ReadAll,
            fail_render_at: None,
            untranslated: None,
            carries: true,
            natural_end: true,
            observations: 0,
            counted: vec![("settled", "ended", 1)],
        }
    }
}

/// What one walk left behind.
struct Outcome {
    carried: Option<StreamEvidence>,
    verified_entries: usize,
    ledger_rows: u64,
    counted: Vec<(&'static str, &'static str, u64)>,
}

fn verified_entries(router: &Router) -> usize {
    router
        .learned_registry()
        .snapshot()
        .iter()
        .filter(|e| e.verdict == Verdict::VerifiedWorking)
        .count()
}

/// The published router after `reload`: a replacement attached to the same
/// learned registry, as the reload coordinator builds one.
fn replacement(previous: &Router, provider: &Arc<dyn Provider>, reload: Reload) -> Router {
    match reload {
        Reload::KillSwitchOff => {
            let mut next = router_over(OPENAI_P1_DISABLED, provider);
            next.carry_over_learned_from(previous);
            next
        }
        Reload::CatalogBoundary => {
            let mut next = router_over(OPENAI_P1, provider);
            next.install_catalog_overlay(Arc::new(routectl_router::CatalogOverlay {
                revision: previous.overlay_revision() + 1,
                ..routectl_router::CatalogOverlay::default()
            }));
            next.carry_over_learned_from(previous);
            let registry = next.learned_registry();
            let before = registry.generation();
            let BoundaryCut::Taken { receipt, .. } =
                registry.with_boundary_cut(|_, _| (), |()| true)
            else {
                panic!("the boundary cut is taken");
            };
            assert!(matches!(
                registry.commit_boundary_transition(&receipt),
                routectl_router::BoundarySettlement::Applied { .. }
            ));
            assert!(registry.generation() > before, "the generation advanced");
            next
        }
    }
}

/// Drive `row` through the warm stream walk: a real dispatch, the carrier
/// held from its meta, `drive_stream`, and the settle at the natural end.
async fn walk(row: &Row) -> Outcome {
    let gate = matches!(
        row.client,
        Client::HangUpAfterFirst | Client::ReloadAfterFirst(_)
    )
    .then(|| Arc::new(Notify::new()));
    let provider: Arc<dyn Provider> = Arc::new(Scripted {
        chunks: row.chunks.clone(),
        tail: row.tail,
        gate: gate.clone(),
        untranslated: row.untranslated,
    });
    let router = Arc::new(router_over(row.config, &provider));
    let live = Arc::new(ArcSwap::from(Arc::clone(&router)));
    let counters: &'static StreamEvidenceCounters =
        Box::leak(Box::new(StreamEvidenceCounters::default()));
    let settler = StreamEvidenceSettler::with_counters(Arc::clone(&live), counters);
    let usage_counters = Arc::new(UsageCounters::default());
    let (usage_tx, _usage_rx) = mpsc::channel(1024);
    let usage: UsageHandle =
        routectl_usage::handle_over_channel_with(usage_tx, Arc::clone(&usage_counters));
    let capture = UsageCapture::new(
        build_usage_draft("openai", &row.request, row.name.to_string()),
        usage,
        "openai".to_string(),
        routectl_core::RequestObservation::new(),
    );
    let turn = StreamTurn {
        session_key: None,
        ctx: StreamRequestContext::default(),
        meter: None,
        evidence: StreamEvidenceSlot::new(settler),
    };
    let carried: Arc<Mutex<Option<StreamEvidence>>> = Arc::default();
    let fut: DispatchFut = {
        let router = Arc::clone(&router);
        let req = row.request.clone();
        let carried = Arc::clone(&carried);
        Box::pin(async move {
            let dispatched: DispatchedStream = router
                .stream_with_options(req, RouterOptions::default())
                .await;
            carried
                .lock()
                .expect("carried lock")
                .clone_from(&dispatched.meta.stream_evidence);
            dispatched
        })
    };
    let (tx, rx) = mpsc::channel::<SseEvent>(64);
    let shared_rx: SharedRx = Arc::new(Mutex::new(Some(rx)));
    let adapter = ClientAdapter {
        inner: OpenAiIngress,
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_at: row.fail_render_at,
        drop_rx_at_eos: (row.client == Client::HangUpAtEos).then(|| Arc::clone(&shared_rx)),
    };
    if row.client == Client::GoneBeforeContent {
        drop(shared_rx.lock().expect("rx lock").take());
    }
    let task = warm_render_task(
        fut,
        adapter,
        capture,
        tx,
        Arc::clone(&router),
        Arc::new(ConfirmationTracker::new()),
        turn,
    );
    let client = consume(row.client, &shared_rx, gate, &live, &router, &provider);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Box::pin(async { tokio::join!(task, client) }),
    )
    .await
    .expect("the walk ends");
    let published = live.load();
    Outcome {
        carried: carried.lock().expect("carried lock").clone(),
        verified_entries: verified_entries(&published),
        ledger_rows: usage_counters.capability_events_enqueued(),
        counted: counters.snapshot(),
    }
}

async fn consume(
    client: Client,
    shared_rx: &SharedRx,
    gate: Option<Arc<Notify>>,
    live: &ArcSwap<Router>,
    router: &Router,
    provider: &Arc<dyn Provider>,
) {
    // Left in place for the adapter, which drops it when end-of-stream is
    // rendered; until then the buffered channel absorbs the content events.
    if matches!(client, Client::HangUpAtEos | Client::GoneBeforeContent) {
        return;
    }
    let Some(mut rx) = shared_rx.lock().expect("rx lock").take() else {
        return;
    };
    match client {
        Client::ReadAll => while rx.recv().await.is_some() {},
        Client::HangUpAtEos | Client::GoneBeforeContent => {}
        Client::HangUpAfterFirst => {
            rx.recv().await.expect("first content event");
            drop(rx);
        }
        Client::ReloadAfterFirst(reload) => {
            rx.recv().await.expect("first content event");
            live.store(Arc::new(replacement(router, provider, reload)));
            gate.expect("gated stream").notify_one();
            while rx.recv().await.is_some() {}
        }
    }
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            observations: 1,
            ..Row::new("natural end, clean stop (positive control)")
        },
        Row {
            tail: Tail::Error,
            natural_end: false,
            counted: vec![("abandoned", "upstream_error", 1)],
            ..Row::new("error after content")
        },
        Row {
            client: Client::HangUpAfterFirst,
            natural_end: false,
            counted: vec![("abandoned", "disconnect", 1)],
            ..Row::new("client disconnect mid-stream")
        },
        Row {
            fail_render_at: Some(1),
            natural_end: false,
            counted: vec![("abandoned", "render_failure", 1)],
            ..Row::new("render failure after content")
        },
        Row {
            chunks: Vec::new(),
            carries: false,
            natural_end: false,
            counted: Vec::new(),
            ..Row::new("pre-content dispatch error")
        },
        Row {
            client: Client::GoneBeforeContent,
            carries: false,
            natural_end: false,
            counted: Vec::new(),
            ..Row::new("client gone before first content")
        },
        Row {
            chunks: vec![text(r#"{"name":"ok"}"#), stop("length")],
            ..Row::new("finish_reason length")
        },
        Row {
            chunks: vec![text(r#"{"name":"ok"}"#), stop("content_filter")],
            ..Row::new("finish_reason content_filter")
        },
        Row {
            chunks: vec![
                chunk(0, Some(r#"{"name":"ok"}"#), None),
                chunk(1, Some(r#"{"name":"ok"}"#), None),
                chunk(0, None, Some("stop")),
                chunk(1, None, Some("stop")),
            ],
            counted: vec![("abandoned", "ended", 1)],
            ..Row::new("two choices")
        },
        Row {
            chunks: over_byte_cap(),
            counted: vec![("abandoned", "ended", 1)],
            ..Row::new("fold over the byte cap")
        },
        Row {
            request: forced_web_search_request(),
            chunks: vec![text("no search was run"), stop("stop")],
            ..Row::new("forced web search with no evidence")
        },
        Row {
            untranslated: Some(STRUCTURED_OUTPUT),
            ..Row::new("untranslated capability")
        },
        Row {
            config: ANTHROPIC_P1_FORWARDED,
            request: forwarded_request(),
            carries: false,
            counted: Vec::new(),
            ..Row::new("forwarded credential")
        },
        Row {
            config: OPENAI_P1_DISABLED,
            carries: false,
            counted: Vec::new(),
            ..Row::new("kill switch off at dispatch")
        },
        Row {
            client: Client::ReloadAfterFirst(Reload::KillSwitchOff),
            counted: vec![("abandoned", "ended", 1)],
            ..Row::new("kill switch turned off by a reload mid-stream")
        },
        Row {
            client: Client::ReloadAfterFirst(Reload::CatalogBoundary),
            ..Row::new("reload across a catalog boundary mid-stream (stale)")
        },
        Row {
            client: Client::HangUpAtEos,
            counted: vec![("abandoned", "disconnect", 1)],
            ..Row::new("client disconnect during end-of-stream")
        },
    ]
}

/// A conforming reply whose text runs past the fold's byte cap, so only the
/// cap stands between it and a verified observation.
fn over_byte_cap() -> Vec<ChatChunk> {
    let pad = "a".repeat(64 * 1024);
    let mut chunks = vec![text(r#"{"name":"ok","pad":""#)];
    for _ in 0..17 {
        chunks.push(text(&pad));
    }
    chunks.push(text(r#""}"#));
    chunks.push(stop("stop"));
    chunks
}

fn forced_web_search_request() -> ChatRequest {
    ChatRequest {
        tools: Some(vec![routectl_core::ToolDef::Other(
            json!({"type": "web_search_20250305", "name": "web_search"}),
        )]),
        tool_choice: Some(json!({"type": "tool", "name": "web_search"})),
        provider_extras: None,
        ..strict_request()
    }
}

fn forwarded_request() -> ChatRequest {
    let mut req = strict_request();
    req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        "client-token".to_string(),
    ));
    req
}

#[tokio::test]
async fn only_a_fully_delivered_natural_end_admits_stream_evidence() {
    for row in rows() {
        // Act
        let out = Box::pin(walk(&row)).await;

        // Assert
        let name = row.name;
        assert_eq!(out.carried.is_some(), row.carries, "{name}: carrier");
        if let Some(carrier) = &out.carried {
            assert_eq!(
                carrier.ended_naturally(),
                row.natural_end,
                "{name}: natural end"
            );
        }
        assert_eq!(
            out.verified_entries, row.observations,
            "{name}: verified entries"
        );
        assert_eq!(
            out.ledger_rows, row.observations as u64,
            "{name}: ledger rows"
        );
        assert_eq!(out.counted, row.counted, "{name}: counter");
    }
}

/// The forced-search row's zero is the verified-only policy, not a reply that
/// carries nothing: the same reply read with suspects admitted, as a complete
/// response is, does reach the registry.
#[tokio::test]
async fn the_forced_search_reply_is_a_suspect_the_stream_policy_refuses() {
    // Arrange
    let provider: Arc<dyn Provider> = Arc::new(Scripted {
        chunks: vec![text("no search was run"), stop("stop")],
        tail: Tail::End,
        gate: None,
        untranslated: None,
    });
    let router = router_over(OPENAI_P1, &provider);
    let dispatched = router
        .stream_with_options(forced_web_search_request(), RouterOptions::default())
        .await;
    let mut stream = dispatched.result.expect("dispatch");
    while stream.next().await.is_some() {}
    let carrier = dispatched.meta.stream_evidence.expect("carrier");

    // Act
    carrier.settle(
        &router,
        routectl_router::router::EvidencePolicy {
            admit_suspect: true,
        },
        &mut Vec::new(),
    );

    // Assert
    assert_eq!(router.learned_registry().snapshot().len(), 1);
}

// --- parity with the complete path -----------------------------------------

/// One reply, written once as a complete body and once as the equivalent
/// stream: structured output that satisfies the schema plus a prompt-cache
/// read, so two detectors fire.
fn parity_complete_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "created": 1_700_000_000_i64,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"name\":\"ok\"}"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 40, "completion_tokens": 5, "total_tokens": 45,
            "prompt_tokens_details": {"cached_tokens": 32}
        }
    })
}

fn parity_stream_body() -> String {
    let frame = |v: Value| format!("data: {v}\n\n");
    let delta = |content: Value, finish: Value| {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4o",
            "created": 1_700_000_000_i64,
            "choices": [{"index": 0, "delta": content, "finish_reason": finish}]
        })
    };
    let mut body = String::new();
    body.push_str(&frame(delta(
        json!({"role": "assistant", "content": "{\"na"}),
        Value::Null,
    )));
    body.push_str(&frame(delta(
        json!({"content": "me\":\"ok\"}"}),
        Value::Null,
    )));
    body.push_str(&frame(delta(json!({}), json!("stop"))));
    body.push_str(&frame(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4o",
        "created": 1_700_000_000_i64, "choices": [],
        "usage": {
            "prompt_tokens": 40, "completion_tokens": 5, "total_tokens": 45,
            "prompt_tokens_details": {"cached_tokens": 32}
        }
    })));
    body.push_str("data: [DONE]\n\n");
    body
}

async fn parity_upstream(stream_body: String) -> wiremock::MockServer {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream_body),
        )
        .with_priority(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(parity_complete_body()))
        .with_priority(2)
        .mount(&upstream)
        .await;
    upstream
}

/// A router over one real openai-compat lane at `base`.
async fn openai_compat_router(base: &str) -> Router {
    use routectl_router::{ModelEntry, ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;

    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat(base, crate::test_secret::file_ref("k")),
    );
    let mut models = BTreeMap::new();
    models.insert("m1".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(
        Arc::new(Config {
            providers,
            models,
            retry,
            ..Default::default()
        }),
        secrets,
    )
    .await
    .expect("build router")
}

/// `(capability, verdict, evidence class)` of every resident entry.
fn resident(router: &Router) -> Vec<(String, Verdict, Option<String>)> {
    let mut cells: Vec<_> = router
        .learned_registry()
        .snapshot()
        .into_iter()
        .map(|e| (e.feature_key, e.verdict, e.evidence_class))
        .collect();
    cells.sort_by(|a, b| a.0.cmp(&b.0));
    cells
}

/// Stream a request through the warm walk against `router`, published as the
/// only router, to its natural end.
async fn stream_through_walk(router: Router, req: ChatRequest) -> Arc<Router> {
    let router = Arc::new(router);
    let live = Arc::new(ArcSwap::from(Arc::clone(&router)));
    let (usage_tx, _usage_rx) = mpsc::channel(1024);
    let capture = UsageCapture::new(
        build_usage_draft("openai", &req, "parity".to_string()),
        routectl_usage::handle_over_channel(usage_tx),
        "openai".to_string(),
        routectl_core::RequestObservation::new(),
    );
    let turn = StreamTurn {
        session_key: None,
        ctx: StreamRequestContext::default(),
        meter: None,
        evidence: StreamEvidenceSlot::new(StreamEvidenceSettler::with_counters(
            live,
            Box::leak(Box::new(StreamEvidenceCounters::default())),
        )),
    };
    let fut: DispatchFut = {
        let router = Arc::clone(&router);
        Box::pin(async move {
            router
                .stream_with_options(req, RouterOptions::default())
                .await
        })
    };
    let (tx, mut rx) = mpsc::channel::<SseEvent>(64);
    let task = warm_render_task(
        fut,
        OpenAiIngress,
        capture,
        tx,
        Arc::clone(&router),
        Arc::new(ConfirmationTracker::new()),
        turn,
    );
    tokio::join!(task, async { while rx.recv().await.is_some() {} });
    router
}

#[tokio::test]
async fn one_reply_yields_the_same_observations_complete_and_streamed() {
    // Arrange
    let upstream = parity_upstream(parity_stream_body()).await;
    let complete_router = openai_compat_router(&upstream.uri()).await;
    let stream_router = openai_compat_router(&upstream.uri()).await;
    let complete_req = ChatRequest {
        stream: None,
        ..strict_request()
    };

    // Act
    let dispatched = complete_router
        .complete_with_options(complete_req, RouterOptions::default())
        .await;
    assert!(dispatched.result.is_ok(), "complete dispatch");
    let stream_router = stream_through_walk(stream_router, strict_request()).await;

    // Assert
    let complete = resident(&complete_router);
    assert_eq!(
        complete.len(),
        2,
        "the reply fires two detectors: {complete:?}"
    );
    assert_eq!(resident(&stream_router), complete);
}

// --- openai-compat truncation ----------------------------------------------

/// A text stream that closes with neither `[DONE]` nor a finish_reason: the
/// provider ends it with an upstream stream error, so it never reaches a
/// natural end and its evidence is never settled.
#[tokio::test]
async fn a_truncated_openai_compat_stream_errors_and_admits_nothing() {
    // Arrange
    let frame = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4o",
        "created": 1_700_000_000_i64,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "{\"name\":\"ok\"}"},
                     "finish_reason": null}]
    });
    let upstream = parity_upstream(format!("data: {frame}\n\n")).await;
    let router = openai_compat_router(&upstream.uri()).await;

    // Act
    let (router, logs) =
        routectl_testkit::with_capture(Box::pin(stream_through_walk(router, strict_request())))
            .await;

    // Assert
    assert!(
        logs.iter().any(|e| e
            .message
            .contains("openai-compat stream closed without a terminal signal")),
        "the provider warned about the truncation"
    );
    assert!(
        logs.iter()
            .any(|e| e.message.contains("upstream stream error")),
        "the stream ended with an upstream stream error"
    );
    assert!(router.learned_registry().snapshot().is_empty());
    // Positive control: the same lane and request with a terminated reply
    // does verify, so the zero above is the truncation.
    let upstream = parity_upstream(parity_stream_body()).await;
    let control = stream_through_walk(
        openai_compat_router(&upstream.uri()).await,
        strict_request(),
    )
    .await;
    assert_eq!(verified_entries(&control), 2);
}
