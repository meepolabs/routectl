//! The response leg end to end: a request through `ingress_handle`, a real
//! egress against a mock upstream, and the CLI renderer's output compared
//! with what the upstream sent. Each FIDELITY dialect, complete and stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, header};
use routectl_router::wire_check_state::{CheckOutcome, WireCheckSnapshot};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::ingress::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, ResponseEdits, SseEvent,
    StreamErrorClass, StreamRequestContext,
};
use crate::server::AppState;
use routectl_core::wire_check::{EditKind, EditSite};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse};

const CHECK_EVERY: (u32, u32) = (8, 1);
const CHECK_NONE: (u32, u32) = (0, 1000);

#[derive(Clone, Copy, Debug)]
enum Dialect {
    Anthropic,
    Chat,
    Responses,
}

const DIALECTS: [Dialect; 3] = [Dialect::Anthropic, Dialect::Chat, Dialect::Responses];

impl Dialect {
    const fn lane(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic->anthropic-api",
            Self::Chat => "chat->openai-compat",
            Self::Responses => "responses->openai-responses",
        }
    }

    fn provider(self, base: &str) -> ProviderEntry {
        let key = crate::test_secret::file_ref("k");
        match self {
            Self::Anthropic => ProviderEntry::anthropic_api(key).with_base_url(base),
            Self::Chat => ProviderEntry::openai_compat(base, key),
            Self::Responses => ProviderEntry::openai_responses(key)
                .with_openai_responses_auth_kind(
                    routectl_providers::openai_responses::AuthKind::ApiKey,
                )
                .with_openai_responses_base_url(base),
        }
    }

    const fn upstream_path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::Chat => "/chat/completions",
            Self::Responses => "/responses",
        }
    }

    fn request_body(self, stream: bool) -> Value {
        match self {
            Self::Anthropic => json!({
                "model": "a", "max_tokens": 16, "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::Chat => json!({
                "model": "a", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            Self::Responses => json!({
                "model": "a", "stream": stream,
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}]
            }),
        }
    }

    /// A complete upstream body carrying `extra` as a top-level member the
    /// renderer has no slot for.
    fn complete_body(self, extra: Option<(&str, Value)>) -> Value {
        let mut body = match self {
            Self::Anthropic => json!({
                "id": "msg_1", "type": "message", "role": "assistant", "model": "upstream",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }),
            Self::Chat => json!({
                "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                "model": "upstream", "system_fingerprint": null,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            }),
            Self::Responses => responses_body(),
        };
        if let (Some((key, value)), Some(object)) = (extra, body.as_object_mut()) {
            object.insert(key.to_string(), value);
        }
        body
    }

    /// The upstream's stream for the same answer.
    fn stream_body(self, extra: Option<(&str, Value)>) -> String {
        let frames: Vec<(Option<&str>, Value)> = match self {
            Self::Anthropic => {
                let mut start = json!({
                    "id": "msg_1", "type": "message", "role": "assistant", "model": "upstream",
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 3, "output_tokens": 0}
                });
                if let (Some((key, value)), Some(object)) = (extra, start.as_object_mut()) {
                    object.insert(key.to_string(), value);
                }
                vec![
                    (
                        Some("message_start"),
                        json!({"type": "message_start", "message": start}),
                    ),
                    (
                        Some("content_block_start"),
                        json!({"type": "content_block_start", "index": 0,
                        "content_block": {"type": "text", "text": ""}}),
                    ),
                    (
                        Some("content_block_delta"),
                        json!({"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": "ok"}}),
                    ),
                    (
                        Some("content_block_stop"),
                        json!({"type": "content_block_stop", "index": 0}),
                    ),
                    (
                        Some("message_delta"),
                        json!({"type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"input_tokens": 3, "output_tokens": 1}}),
                    ),
                    (Some("message_stop"), json!({"type": "message_stop"})),
                ]
            }
            Self::Chat => {
                let mut first = json!({"id": "c1", "object": "chat.completion.chunk",
                    "created": 1_700_000_000_i64, "model": "upstream",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}}]});
                if let (Some((key, value)), Some(object)) = (extra, first.as_object_mut()) {
                    object.insert(key.to_string(), value);
                }
                vec![
                    (None, first),
                    (
                        None,
                        json!({"id": "c1", "object": "chat.completion.chunk",
                        "created": 1_700_000_000_i64, "model": "upstream",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                    ),
                ]
            }
            Self::Responses => {
                let mut done = responses_body();
                if let (Some((key, value)), Some(object)) = (extra, done.as_object_mut()) {
                    object.insert(key.to_string(), value);
                }
                let item = json!({"type": "message", "role": "assistant",
                                  "status": "completed", "content": []});
                let mut created = responses_body();
                created["status"] = json!("in_progress");
                created["output"] = json!([]);
                vec![
                    (
                        Some("response.created"),
                        json!({"type": "response.created", "response": created}),
                    ),
                    (
                        Some("response.output_item.added"),
                        json!({
                        "type": "response.output_item.added", "output_index": 0, "item": item}),
                    ),
                    (
                        Some("response.content_part.added"),
                        json!({
                        "type": "response.content_part.added", "output_index": 0,
                        "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}}),
                    ),
                    (
                        Some("response.output_text.delta"),
                        json!({
                        "type": "response.output_text.delta", "output_index": 0,
                        "content_index": 0, "delta": "ok"}),
                    ),
                    (
                        Some("response.completed"),
                        json!({"type": "response.completed", "response": done}),
                    ),
                ]
            }
        };
        let mut wire = String::new();
        for (event, data) in frames {
            if let Some(event) = event {
                wire.push_str(&format!("event: {event}\n"));
            }
            wire.push_str(&format!("data: {data}\n\n"));
        }
        if matches!(self, Self::Chat) {
            wire.push_str("data: [DONE]\n\n");
        }
        wire
    }
}

fn responses_body() -> Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": 0,
        "status": "completed", "model": "upstream",
        "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
}

async fn upstream(dialect: Dialect, stream: bool, extra: Option<(&str, Value)>) -> MockServer {
    let server = MockServer::start().await;
    let template = if stream {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(dialect.stream_body(extra))
    } else if matches!(dialect, Dialect::Responses) {
        // The Responses egress reads every answer as a stream; its complete
        // render echoes a nonzero upstream `created_at`, which the stream
        // render has no slot for.
        let extra = extra.or(Some(("created_at", json!(1_700_000_000_i64))));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(dialect.stream_body(extra))
    } else {
        ResponseTemplate::new(200).set_body_json(dialect.complete_body(extra))
    };
    Mock::given(method("POST"))
        .and(path(dialect.upstream_path()))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

async fn router(dialect: Dialect, base: &str, (first_n, sample_one_in): (u32, u32)) -> Router {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers: BTreeMap::from([("p".to_string(), dialect.provider(base))]),
        models: BTreeMap::from([("m".to_string(), ModelEntry::new("p", "upstream"))]),
        aliases: BTreeMap::from([("a".to_string(), AliasValue::Single("m".to_string()))]),
        retry,
        ..Default::default()
    };
    config.wire_check.first_n = first_n;
    config.wire_check.sample_one_in =
        std::num::NonZeroU32::new(sample_one_in).expect("nonzero sample");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router")
}

/// What the client-facing renderer does to the response in a test run.
#[derive(Clone, Copy)]
enum Render {
    /// The production renderer, unchanged.
    AsShipped,
    /// The production renderer, then one upstream member removed.
    Dropping,
    /// As `Dropping`, with the removal declared.
    DroppingDeclared,
}

/// Send one request through the ingress and return the bytes the client
/// received, after the response is over.
async fn send(
    dialect: Dialect,
    swap: &Arc<arc_swap::ArcSwap<Router>>,
    stream: bool,
    render: Render,
) -> Vec<u8> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    let body = Bytes::from(serde_json::to_vec(&dialect.request_body(stream)).expect("json"));
    let state = AppState::for_test(Arc::clone(swap));
    let resp = match dialect {
        Dialect::Anthropic => {
            let adapter = DroppingAdapter::new(AnthropicIngress, dialect, render);
            ingress_handle(state, headers, None, Ok(body), adapter).await
        }
        Dialect::Chat => {
            let adapter = DroppingAdapter::new(OpenAiIngress, dialect, render);
            ingress_handle(state, headers, None, Ok(body), adapter).await
        }
        Dialect::Responses => {
            let adapter = DroppingAdapter::new(ResponsesIngress, dialect, render);
            ingress_handle(state, headers, None, Ok(body), adapter).await
        }
    };
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

/// Run one request on `dialect` and return the router's wire-check state
/// once the response is over, with the bytes the client got.
async fn run(
    dialect: Dialect,
    stream: bool,
    extra: Option<(&str, Value)>,
    sampling: (u32, u32),
) -> (WireCheckSnapshot, Vec<u8>) {
    run_rendering(dialect, stream, extra, sampling, Render::AsShipped).await
}

async fn run_rendering(
    dialect: Dialect,
    stream: bool,
    extra: Option<(&str, Value)>,
    sampling: (u32, u32),
    render: Render,
) -> (WireCheckSnapshot, Vec<u8>) {
    let server = upstream(dialect, stream, extra).await;
    let router = router(dialect, &server.uri(), sampling).await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));
    let client = send(dialect, &swap, stream, render).await;
    let snapshot = swap.load().wire_check_snapshot();
    (snapshot, client)
}

impl Dialect {
    /// The member a dropping renderer removes: its pointer in a complete
    /// body, its pointer in the stream frame that carries it, and the path
    /// the assembled document names it by.
    const fn dropped(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Anthropic => ("/role", "/message/role", "/role"),
            Self::Chat => (
                "/choices/0/finish_reason",
                "/choices/0/finish_reason",
                "/choices/0/finish_reason",
            ),
            Self::Responses => ("/status", "/response/status", "/status"),
        }
    }

    const fn dropped_pattern(self) -> &'static str {
        match self {
            Self::Anthropic => "/role",
            Self::Chat => "/choices/*/finish_reason",
            Self::Responses => "/status",
        }
    }
}

/// A production adapter whose rendered output loses one member when the
/// render is `Dropping`, as a renderer with a fidelity bug would.
struct DroppingAdapter<A> {
    inner: A,
    dialect: Dialect,
    render: Render,
}

impl<A> DroppingAdapter<A> {
    const fn new(inner: A, dialect: Dialect, render: Render) -> Self {
        Self {
            inner,
            dialect,
            render,
        }
    }

    fn declare(&self, edits: &mut ResponseEdits) {
        if matches!(self.render, Render::DroppingDeclared) {
            edits.declare(
                EditSite::StreamStragglerDrop,
                self.dialect.dropped_pattern(),
                EditKind::Remove,
            );
        }
    }

    fn drop_from(&self, mut value: Value, pointer: &str) -> Value {
        if matches!(self.render, Render::AsShipped) {
            return value;
        }
        if let Some((parent, member)) = pointer.rsplit_once('/')
            && let Some(object) = value.pointer_mut(parent).and_then(Value::as_object_mut)
        {
            object.remove(member);
        }
        value
    }

    fn drop_from_events(&self, events: Vec<SseEvent>) -> Vec<SseEvent> {
        let (_, frame_pointer, _) = self.dialect.dropped();
        events
            .into_iter()
            .map(|event| match serde_json::from_str::<Value>(&event.data) {
                Ok(data) => SseEvent {
                    data: self.drop_from(data, frame_pointer).to_string(),
                    ..event
                },
                Err(_) => event,
            })
            .collect()
    }
}

impl<A: IngressAdapter> IngressAdapter for DroppingAdapter<A> {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn dialect(&self) -> routectl_core::wire_check::IngressDialect {
        self.inner.dialect()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        self.inner.error_envelope_shape()
    }
    fn parse_request(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> routectl_core::Result<ChatRequest> {
        self.inner.parse_request(headers, body)
    }
    fn render_response(&self, resp: ChatResponse) -> routectl_core::Result<bytes::Bytes> {
        self.render_response_declared(resp, &mut ResponseEdits::default())
    }
    fn render_response_declared(
        &self,
        resp: ChatResponse,
        edits: &mut ResponseEdits,
    ) -> routectl_core::Result<bytes::Bytes> {
        let body = self.inner.render_response_declared(resp, edits)?;
        self.declare(edits);
        let value: Value = serde_json::from_slice(&body).expect("rendered json");
        let (body_pointer, _, _) = self.dialect.dropped();
        Ok(serde_json::to_vec(&self.drop_from(value, body_pointer))
            .expect("json")
            .into())
    }
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        self.inner.new_stream_state(ctx)
    }
    fn early_frame(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        self.drop_from_events(self.inner.early_frame(state))
    }
    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        self.render_chunk_declared(chunk, state, &mut ResponseEdits::default())
    }
    fn render_chunk_declared(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
        edits: &mut ResponseEdits,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        let events = self.inner.render_chunk_declared(chunk, state, edits)?;
        self.declare(edits);
        Ok(self.drop_from_events(events))
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        self.drop_from_events(self.inner.render_eos(state))
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        self.inner.render_error_eos(state, error, class)
    }
}

fn outcomes(snapshot: &WireCheckSnapshot) -> Vec<(&'static str, CheckOutcome, u64)> {
    snapshot
        .checks
        .iter()
        .map(|c| (c.lane.label(), c.outcome, c.reading.count))
        .collect()
}

fn response_findings(snapshot: &WireCheckSnapshot) -> Vec<(String, &'static str)> {
    snapshot
        .findings
        .iter()
        .filter(|f| f.leg == routectl_core::wire_check::Leg::Response)
        .map(|f| {
            (
                f.path.as_str().to_string(),
                f.class.map_or("-", |c| c.label()),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_faithful_response_records_clean_on_both_legs() {
    for dialect in DIALECTS {
        for stream in [false, true] {
            let (snapshot, client) = run(dialect, stream, None, CHECK_EVERY).await;

            assert_eq!(
                response_findings(&snapshot),
                Vec::<(String, &str)>::new(),
                "{dialect:?} stream={stream}: {}",
                String::from_utf8_lossy(&client)
            );
            assert_eq!(
                outcomes(&snapshot),
                [(dialect.lane(), CheckOutcome::Clean, 2)],
                "{dialect:?} stream={stream}"
            );
        }
    }
}

#[tokio::test]
async fn a_renderer_that_drops_an_upstream_key_is_counted_on_the_response_leg_by_path() {
    for dialect in DIALECTS {
        for stream in [false, true] {
            let (snapshot, _) =
                run_rendering(dialect, stream, None, CHECK_EVERY, Render::Dropping).await;

            let (_, _, path) = dialect.dropped();
            assert_eq!(
                response_findings(&snapshot),
                [(path.to_string(), "missing")],
                "{dialect:?} stream={stream}"
            );
            let loss: Vec<_> = snapshot
                .loss
                .iter()
                .map(|c| (c.lane.label(), c.path_class.label(), c.reading.count))
                .collect();
            assert_eq!(
                loss,
                [(dialect.lane(), "missing", 1)],
                "{dialect:?} stream={stream}"
            );
        }
    }
}

#[tokio::test]
async fn a_drop_the_renderer_declares_records_clean() {
    for dialect in DIALECTS {
        for stream in [false, true] {
            let (snapshot, _) =
                run_rendering(dialect, stream, None, CHECK_EVERY, Render::DroppingDeclared).await;

            assert!(
                response_findings(&snapshot).is_empty(),
                "{dialect:?} stream={stream}: {:?}",
                response_findings(&snapshot)
            );
            assert_eq!(
                outcomes(&snapshot),
                [(dialect.lane(), CheckOutcome::Clean, 2)],
                "{dialect:?} stream={stream}"
            );
        }
    }
}

#[tokio::test]
async fn an_unadmitted_success_records_no_response_check() {
    for dialect in DIALECTS {
        for stream in [false, true] {
            let (snapshot, client) =
                run_rendering(dialect, stream, None, CHECK_NONE, Render::Dropping).await;

            assert!(
                !client.is_empty(),
                "{dialect:?} stream={stream}: the client was served"
            );
            assert_eq!(outcomes(&snapshot), [], "{dialect:?} stream={stream}");
            assert!(snapshot.findings.is_empty(), "{dialect:?} stream={stream}");
        }
    }
}

/// The Responses stream renderer has no upstream timestamp to echo (the
/// canonical chunk carries none), so a nonzero upstream `created_at` is
/// lost. Pinned as a detection: the check names it on the response leg.
#[tokio::test]
async fn the_responses_stream_render_loses_the_upstream_created_at() {
    let (snapshot, _) = run(
        Dialect::Responses,
        true,
        Some(("created_at", json!(1_700_000_000_i64))),
        CHECK_EVERY,
    )
    .await;

    assert_eq!(
        response_findings(&snapshot),
        [("/created_at".to_string(), "changed")]
    );
}

/// One `message_start` and a text block, then nothing: a stream the
/// upstream never finished.
fn anthropic_unfinished_stream() -> String {
    let frames = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "upstream",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "ok"}}),
        ),
    ];
    frames
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

#[tokio::test]
async fn an_upstream_stream_cut_mid_response_records_incomplete_never_clean() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(anthropic_unfinished_stream()),
        )
        .mount(&server)
        .await;
    let router = router(Dialect::Anthropic, &server.uri(), CHECK_EVERY).await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));

    let (client, events) = routectl_testkit::with_capture(Box::pin(send(
        Dialect::Anthropic,
        &swap,
        true,
        Render::AsShipped,
    )))
    .await;

    let client = String::from_utf8_lossy(&client);
    assert_eq!(
        incomplete_reasons(&events),
        [("upstream".to_string(), "upstream_error".to_string())]
    );
    assert!(
        client.contains("event: error"),
        "the client saw the failure: {client}"
    );
    let snapshot = swap.load().wire_check_snapshot();
    assert_eq!(
        outcomes(&snapshot),
        [
            (Dialect::Anthropic.lane(), CheckOutcome::Clean, 1),
            (Dialect::Anthropic.lane(), CheckOutcome::Incomplete, 1),
        ],
        "the request leg is clean; the response leg is incomplete"
    );
    assert!(snapshot.findings.is_empty());
}

/// An upstream that sends the first frames of a stream and then holds the
/// connection open, so the client can leave mid-response.
struct HangingUpstream {
    base: String,
}

impl HangingUpstream {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 64 * 1024];
                    let _ = socket.read(&mut buf).await;
                    let body = anthropic_unfinished_stream();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         transfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                });
            }
        });
        Self { base }
    }
}

/// The (side, reason) of every response check that found nothing complete
/// to compare.
fn incomplete_reasons(events: &[routectl_testkit::CapturedEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|e| e.message == "wire check had no complete response to compare")
        .map(|e| {
            (
                e.field("side").unwrap_or_default().to_string(),
                e.field("reason").unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_client_leaving_mid_stream_records_incomplete_never_clean() {
    let (recorded, events) =
        routectl_testkit::with_capture(Box::pin(client_leaves_mid_stream())).await;

    assert_eq!(
        recorded,
        [
            (Dialect::Anthropic.lane(), CheckOutcome::Clean, 1),
            (Dialect::Anthropic.lane(), CheckOutcome::Incomplete, 1),
        ]
    );
    assert_eq!(
        incomplete_reasons(&events),
        [("upstream".to_string(), "disconnect".to_string())]
    );
}

async fn client_leaves_mid_stream() -> Vec<(&'static str, CheckOutcome, u64)> {
    use futures::StreamExt;

    let upstream = HangingUpstream::start().await;
    let router = router(Dialect::Anthropic, &upstream.base, CHECK_EVERY).await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let body =
        Bytes::from(serde_json::to_vec(&Dialect::Anthropic.request_body(true)).expect("json"));
    let resp = ingress_handle(
        AppState::for_test(Arc::clone(&swap)),
        headers,
        None,
        Ok(body),
        AnthropicIngress,
    )
    .await;
    let mut frames = resp.into_body().into_data_stream();
    let first = frames
        .next()
        .await
        .expect("a first frame")
        .expect("frame bytes");
    assert!(!first.is_empty());

    drop(frames);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let outcomes = outcomes(&swap.load().wire_check_snapshot());
            if outcomes.len() > 1 {
                return outcomes;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the response leg records once the client leaves")
}

/// A Chat upstream answer whose stop value or usage makes one renderer
/// exception fire.
fn chat_complete_with(finish_reason: &str, usage: Value) -> Value {
    json!({
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
        "model": "upstream", "system_fingerprint": null,
        "choices": [{"index": 0, "finish_reason": finish_reason,
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": usage
    })
}

fn chat_stream_with(chunk_extra: Option<(&str, Value)>, finish_reason: &str) -> String {
    let mut first = json!({"id": "c1", "object": "chat.completion.chunk",
        "created": 1_700_000_000_i64, "model": "upstream",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}}]});
    if let (Some((key, value)), Some(object)) = (chunk_extra, first.as_object_mut()) {
        object.insert(key.to_string(), value);
    }
    let last = json!({"id": "c1", "object": "chat.completion.chunk",
        "created": 1_700_000_000_i64, "model": "upstream",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]});
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

async fn chat_run(template: ResponseTemplate, stream: bool) -> WireCheckSnapshot {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(&server)
        .await;
    let router = router(Dialect::Chat, &server.uri(), CHECK_EVERY).await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(router));
    send(Dialect::Chat, &swap, stream, Render::AsShipped).await;
    swap.load().wire_check_snapshot()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// Each Chat renderer exception, single-sourced: the row where it fires
/// records clean only because its site declared the edit (the deletion
/// mutation of that one declare call turns exactly that row's findings
/// non-empty), and the row where it does not fire stays clean with nothing
/// to declare.
#[tokio::test]
async fn each_declared_chat_exception_explains_only_its_own_change() {
    let usage = json!({"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4});
    let cached = json!({"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4,
                        "cache_read_input_tokens": 2});
    let rows: Vec<(&str, ResponseTemplate, bool)> = vec![
        (
            "finish_reason rewrite fires (complete)",
            ResponseTemplate::new(200).set_body_json(chat_complete_with("refusal", usage.clone())),
            false,
        ),
        (
            "finish_reason rewrite fires (stream)",
            sse(chat_stream_with(None, "refusal")),
            true,
        ),
        (
            "finish_reason already valid",
            ResponseTemplate::new(200).set_body_json(chat_complete_with("stop", usage.clone())),
            false,
        ),
        (
            "cached_tokens added",
            ResponseTemplate::new(200).set_body_json(chat_complete_with("stop", cached)),
            false,
        ),
        (
            "complete envelope stamped over upstream values",
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "", "created": 0, "model": "upstream",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": usage.clone()
            })),
            false,
        ),
        (
            "stream envelope stamped over upstream values",
            sse(chat_stream_with(Some(("created", json!(1))), "stop")),
            true,
        ),
    ];

    for (name, template, stream) in rows {
        let snapshot = chat_run(template, stream).await;

        assert_eq!(
            response_findings(&snapshot),
            Vec::<(String, &str)>::new(),
            "{name}"
        );
        assert_eq!(
            outcomes(&snapshot),
            [(Dialect::Chat.lane(), CheckOutcome::Clean, 2)],
            "{name}"
        );
    }
}

/// The canonical chunk has no slot for an upstream `system_fingerprint`, so
/// the stream render stamps `null` where the upstream named one. Pinned as
/// a detection: the stamp is declared only where the upstream sent nothing.
#[tokio::test]
async fn the_chat_stream_render_loses_an_upstream_system_fingerprint() {
    let snapshot = chat_run(
        sse(chat_stream_with(
            Some(("system_fingerprint", json!("fp_1"))),
            "stop",
        )),
        true,
    )
    .await;

    assert_eq!(
        response_findings(&snapshot),
        [("/system_fingerprint".to_string(), "missing")]
    );
}
