//! Full-pipeline integration tests for the Anthropic SSE forward-compat
//! opaque-events fix.
//!
//! Each scenario hand-crafts an Anthropic SSE wire-byte fixture, drives
//! it through `egress -> canonical -> ingress`, and asserts properties
//! of the emitted SSE bytes the client would see. The egress side uses
//! the wiremock pattern from
//! `routectl-providers/tests/contract_stream_egress.rs`; the ingress
//! side reuses the same `IngressAdapter::render_chunk + render_eos`
//! drive loop as `live_matrix.rs::anthropic_ingress_streaming_subset`.
//! No HTTP server is involved -- the data path that matters for this
//! fix is canonical-shape only.
//!
//! Coverage:
//!   - Test 1: in-budget round-trip preserves opaque blocks (start +
//!     delta + stop) and the canonical text block round-trips
//!     normally.
//!   - Test 2: a stream with NO unknown variants emits zero opaque
//!     events at every chunk and the downstream SSE is unchanged from
//!     the pre-fix shape.
//!   - Test 3: a stream whose unknown-block payload exceeds the
//!     256 KB byte cap degrades cleanly: capture stops at the cap,
//!     downstream opaque events are fewer than input deltas, the
//!     canonical text block following the unknown block still reaches
//!     the client, and the egress emits a degrade WARN.
//!
//! The router-level "chain doesn't walk" test lives in
//! `routectl-router/tests/router.rs` -- it pins router-circuit
//! behavior on opaque-event chunks.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::{IngressAdapter, SseEvent};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use serde_json::Value;
use tracing::field::{Field, Visit};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------
// Provider + request builders
// ---------------------------------------------------------------------
//
// Mirrors `contract_stream_egress.rs`: keep the two-line constructor
// local rather than carving out a `pub` helper for one more caller.

fn anthropic_api_provider(base_url: &str) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-test".into(),
        auth: Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
    })
}

fn stream_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(64),
        stream: Some(true),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// Pipeline driver
// ---------------------------------------------------------------------

/// Drive the Anthropic egress through wiremock, pump every canonical
/// `ChatChunk` through the Anthropic ingress's `render_chunk`, and
/// drain `render_eos`. Returns the flat SSE event list the client
/// would see. Surfaces upstream `Err` as a panic so test assertions
/// only see the success path.
async fn drive_pipeline(server_uri: &str, model: &str) -> Vec<SseEvent> {
    let provider = anthropic_api_provider(server_uri);
    let mut upstream = provider
        .stream(stream_request(model))
        .await
        .expect("egress stream open");
    let ingress = AnthropicIngress;
    let mut state = ingress.new_stream_state();
    let mut out: Vec<SseEvent> = Vec::new();
    while let Some(item) = upstream.next().await {
        let chunk = item.expect("upstream chunk decoded without error");
        out.extend(
            ingress
                .render_chunk(chunk, state.as_mut())
                .expect("render_chunk"),
        );
    }
    out.extend(ingress.render_eos(state.as_mut()));
    out
}

/// Mount a wiremock handler returning `body` as an SSE response on
/// `POST /v1/messages` and return the running mock server. Accepts both
/// `'static` literals (the round-trip and no-opaque cases) and owned
/// `String` bodies (the byte-overflow case, whose body is too large to
/// keep as a `'static str`).
async fn mount_anthropic_sse(body: impl Into<String>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.into()),
        )
        .mount(&server)
        .await;
    server
}

// ---------------------------------------------------------------------
// SSE event helpers
// ---------------------------------------------------------------------

/// Parse the `data:` JSON payload of one event. Panics on parse
/// failure so the test surfaces a clear error rather than a silent
/// pass.
fn parse_data(ev: &SseEvent) -> Value {
    serde_json::from_str(&ev.data).expect("event data is valid JSON")
}

/// Find the first event whose name is `name` AND whose parsed data
/// satisfies `pred`. Returns `None` if no such event exists.
fn find_event<'a>(
    events: &'a [SseEvent],
    name: &str,
    pred: impl Fn(&Value) -> bool,
) -> Option<&'a SseEvent> {
    events.iter().find(|e| {
        e.event.as_deref() == Some(name) && {
            let v = parse_data(e);
            pred(&v)
        }
    })
}

/// Count events whose name is `name`.
fn count_named(events: &[SseEvent], name: &str) -> usize {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some(name))
        .count()
}

// ---------------------------------------------------------------------
// Tracing capture for the overflow test
// ---------------------------------------------------------------------
//
// Minimal in-process subscriber that captures every event into a
// `Vec<CapturedEvent>` with its level, target, message, and string-
// shaped fields. Scoped via `tracing::subscriber::with_default` so
// concurrent tests do not leak captured state.

#[derive(Debug, Clone)]
#[allow(dead_code)] // target/message read via {captured:?} Debug output on test failure
struct CapturedEvent {
    level: tracing::Level,
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push((field.name().into(), value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push((field.name().into(), value.to_string()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push((field.name().into(), value.to_string()));
    }
}

#[derive(Default)]
struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(captured);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Run `fut` with the capture subscriber installed as the thread-local
/// default. Returns the captured events alongside the future's output.
/// `#[tokio::test]` defaults to a current_thread runtime so the
/// subscriber guard remains active for the whole future.
async fn with_capture<F, T>(fut: F) -> (T, Vec<CapturedEvent>)
where
    F: std::future::Future<Output = T>,
{
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: captured.clone(),
    };
    // NOTE: set_default installs a thread-local subscriber. Correct only
    // because #[tokio::test] defaults to current_thread; worker threads
    // in a multi_thread runtime would not see this subscriber. Do NOT
    // add flavor = "multi_thread" to tests that call with_capture.
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = fut.await;
    let events = captured.lock().expect("capture lock poisoned").clone();
    (out, events)
}

// =====================================================================
// Test 1: in-budget round-trip preserves opaque blocks
// =====================================================================
//
// Realistic web-search beta SSE response: a server_tool_use block at
// upstream index 0, a web_search_tool_result block at index 1 carrying
// a citations_delta, and a final text block at index 2. The pipeline
// must emit:
//   - content_block_start for `server_tool_use` carrying the original
//     id + name + input
//   - content_block_start for `web_search_tool_result` carrying the
//     original tool_use_id + content array
//   - content_block_delta for `citations_delta` carrying the original
//     citation payload
//   - canonical content_block_start/delta/stop for the final text
//   - terminal message_delta + message_stop with usage.output_tokens=42

const WEB_SEARCH_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_01\",\"name\":\"web_search\",\"input\":{\"query\":\"rust serde\"}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv_01\",\"content\":[{\"url\":\"https://serde.rs\",\"title\":\"Serde\"}]}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"citations_delta\",\"citation\":{\"type\":\"web_search_result_location\",\"cited_text\":\"Serde is a framework\",\"url\":\"https://serde.rs\",\"title\":\"Serde\"}}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"Serde is a Rust serialization framework.\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":42}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn web_search_round_trip_preserves_opaque_blocks_and_text() {
    // Arrange
    let server = mount_anthropic_sse(WEB_SEARCH_SSE).await;

    // Act
    let events = drive_pipeline(&server.uri(), "claude-opus-4-7").await;

    // Assert -- pipe completed (no panics in drive_pipeline). Now
    // walk the emitted SSE for the wire-shape pins.

    // The opaque server_tool_use block must surface as a
    // content_block_start whose `content_block` carries the original
    // type/id/name/input fields verbatim. Index re-allocation by the
    // ingress is allowed (the BTreeMap upstream->ingress mapping); we
    // pin the content shape, not the index value.
    let srv = find_event(&events, "content_block_start", |v| {
        v.pointer("/content_block/type").and_then(Value::as_str) == Some("server_tool_use")
    })
    .expect("server_tool_use content_block_start must be re-emitted verbatim");
    let srv_data = parse_data(srv);
    assert_eq!(
        srv_data
            .pointer("/content_block/id")
            .and_then(Value::as_str),
        Some("srv_01"),
        "server_tool_use id must round-trip; data: {srv_data}",
    );
    assert_eq!(
        srv_data
            .pointer("/content_block/name")
            .and_then(Value::as_str),
        Some("web_search"),
        "server_tool_use name must round-trip; data: {srv_data}",
    );
    assert_eq!(
        srv_data
            .pointer("/content_block/input/query")
            .and_then(Value::as_str),
        Some("rust serde"),
        "server_tool_use input.query must round-trip; data: {srv_data}",
    );

    // web_search_tool_result must surface with original tool_use_id +
    // content array.
    let res = find_event(&events, "content_block_start", |v| {
        v.pointer("/content_block/type").and_then(Value::as_str) == Some("web_search_tool_result")
    })
    .expect("web_search_tool_result content_block_start must be re-emitted verbatim");
    let res_data = parse_data(res);
    assert_eq!(
        res_data
            .pointer("/content_block/tool_use_id")
            .and_then(Value::as_str),
        Some("srv_01"),
    );
    assert!(
        res_data
            .pointer("/content_block/content")
            .map(Value::is_array)
            .unwrap_or(false),
        "web_search_tool_result.content array must round-trip; data: {res_data}",
    );

    // citations_delta must surface as a content_block_delta whose
    // delta.type matches AND whose nested citation payload survives.
    let cite = find_event(&events, "content_block_delta", |v| {
        v.pointer("/delta/type").and_then(Value::as_str) == Some("citations_delta")
    })
    .expect("citations_delta content_block_delta must be re-emitted");
    let cite_data = parse_data(cite);
    assert_eq!(
        cite_data
            .pointer("/delta/citation/url")
            .and_then(Value::as_str),
        Some("https://serde.rs"),
    );
    assert_eq!(
        cite_data
            .pointer("/delta/citation/cited_text")
            .and_then(Value::as_str),
        Some("Serde is a framework"),
    );

    // The canonical text block: a text_delta carrying the full string.
    let text = find_event(&events, "content_block_delta", |v| {
        v.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
    })
    .expect("text_delta must reach the client");
    assert_eq!(
        parse_data(text)
            .pointer("/delta/text")
            .and_then(Value::as_str),
        Some("Serde is a Rust serialization framework."),
    );

    // Terminal message_delta MUST carry stop_reason=end_turn AND
    // usage.output_tokens=42 in the same event (Bug B class invariant).
    let term = find_event(&events, "message_delta", |_| true)
        .expect("message_delta must close the stream");
    let term_data = parse_data(term);
    assert_eq!(
        term_data
            .pointer("/delta/stop_reason")
            .and_then(Value::as_str),
        Some("end_turn"),
    );
    assert_eq!(
        term_data
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64),
        Some(42),
    );
    // Exactly one message_delta + one message_stop.
    assert_eq!(count_named(&events, "message_delta"), 1);
    assert_eq!(count_named(&events, "message_stop"), 1);
}

// =====================================================================
// Test 2: no-opaque case unchanged (regression guard)
// =====================================================================
//
// A normal Anthropic stream with NO unknown variants. The fix must be
// invisible on this path: zero opaque events on the canonical chunks,
// downstream SSE matches the pre-Wave-1 shape.

const NO_OPAQUE_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_42\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn no_opaque_blocks_regression_unchanged() {
    // Arrange -- collect raw chunks from the egress AND the rendered
    // SSE from the ingress so we can pin both halves.
    let server = mount_anthropic_sse(NO_OPAQUE_SSE).await;
    let provider = anthropic_api_provider(&server.uri());
    let mut upstream = provider
        .stream(stream_request("claude-opus-4-7"))
        .await
        .expect("egress stream open");
    let ingress = AnthropicIngress;
    let mut state = ingress.new_stream_state();
    let mut sse: Vec<SseEvent> = Vec::new();

    // Act -- drain canonical chunks; pin opaque_events.is_empty() on
    // EVERY chunk before forwarding to the ingress.
    while let Some(item) = upstream.next().await {
        let chunk = item.expect("chunk decoded without error");
        assert!(
            chunk.opaque_events.is_empty(),
            "no-opaque path must surface zero opaque events on every chunk; got: {:?}",
            chunk.opaque_events,
        );
        sse.extend(
            ingress
                .render_chunk(chunk, state.as_mut())
                .expect("render_chunk"),
        );
    }
    sse.extend(ingress.render_eos(state.as_mut()));

    // Assert -- downstream SSE shape matches the pre-fix Bug-B-class
    // contract: content_block_start (text) + 2 deltas + stop +
    // single message_delta + message_stop (plus the leading
    // message_start the ingress always emits).
    let names: Vec<&str> = sse.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "no-opaque path must emit the canonical event sequence; got: {names:?}",
    );
    // Text reassembles in order.
    let first = parse_data(&sse[2]);
    let second = parse_data(&sse[3]);
    assert_eq!(
        first.pointer("/delta/text").and_then(Value::as_str),
        Some("Hello")
    );
    assert_eq!(
        second.pointer("/delta/text").and_then(Value::as_str),
        Some(" world")
    );
    // Terminal usage round-trips.
    let term = parse_data(&sse[5]);
    assert_eq!(
        term.pointer("/usage/output_tokens").and_then(Value::as_u64),
        Some(7),
    );
}

// =====================================================================
// Test 3: bounded-capture downgrade on byte-overflow
// =====================================================================
//
// An unknown block whose delta payload sums to > 256 KB. Each delta is
// ~1 KB raw, fed 300 times. Capture saturates near the cap and the
// rest are sink-drained without emission. The trailing canonical text
// block must still reach the client; the egress must log a
// `reason="byte_overflow"` WARN.

/// Build an SSE body with `n` ~1 KB unknown deltas, followed by a
/// normal text block + terminal events. Each delta carries a
/// `citations_delta` shape with a `blob` field of `delta_kb_size` KB
/// of `x` characters so the byte budget tips quickly.
fn overflow_sse_body(n: usize, delta_kb_size: usize) -> String {
    let blob: String = "x".repeat(delta_kb_size * 1024);
    let mut out = String::new();
    out.push_str("event: message_start\n");
    out.push_str("data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ov\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n");
    out.push_str("event: content_block_start\n");
    out.push_str("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv_ov\",\"content\":[]}}\n\n");
    for i in 0..n {
        out.push_str("event: content_block_delta\n");
        out.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"citations_delta\",\"seq\":{i},\"blob\":\"{blob}\"}}}}\n\n",
        ));
    }
    out.push_str("event: content_block_stop\n");
    out.push_str("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    // Trailing canonical text -- must still reach the client.
    out.push_str("event: content_block_start\n");
    out.push_str("data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    out.push_str("event: content_block_delta\n");
    out.push_str("data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"after-overflow\"}}\n\n");
    out.push_str("event: content_block_stop\n");
    out.push_str("data: {\"type\":\"content_block_stop\",\"index\":1}\n\n");
    out.push_str("event: message_delta\n");
    out.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n");
    out.push_str("event: message_stop\n");
    out.push_str("data: {\"type\":\"message_stop\"}\n\n");
    out
}

#[tokio::test]
async fn byte_overflow_degrades_capture_but_stream_continues() {
    // Arrange -- 300 deltas of ~1 KB each: ~300 KB > 256 KB cap.
    const N: usize = 300;
    let body = overflow_sse_body(N, 1);
    let server = mount_anthropic_sse(body).await;

    // Act -- drive the pipeline AND capture tracing events emitted
    // during egress + ingress decoding.
    let server_uri = server.uri();
    let (events, captured) =
        with_capture(async move { drive_pipeline(&server_uri, "claude-opus-4-7").await }).await;

    // Assert 1: the post-overflow canonical text block still reaches
    // the client. Silent overflow that swallowed the rest of the
    // stream would lose this.
    let text = find_event(&events, "content_block_delta", |v| {
        v.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
    })
    .expect("post-overflow text_delta must still reach the client");
    assert_eq!(
        parse_data(text)
            .pointer("/delta/text")
            .and_then(Value::as_str),
        Some("after-overflow"),
    );

    // Assert 2: terminal message_delta + message_stop fire exactly
    // once -- the stream must complete cleanly past the overflow.
    assert_eq!(
        count_named(&events, "message_delta"),
        1,
        "stream must terminate with one message_delta after overflow",
    );
    assert_eq!(count_named(&events, "message_stop"), 1);

    // Assert 3: the count of opaque content_block_delta events the
    // ingress re-emitted is STRICTLY LESS than the input N. Some are
    // captured (under the cap) and re-emitted; the rest are dropped.
    // The ingress only sees the captured subset because the egress
    // sink-drains the post-cap deltas before they reach the canonical
    // chunk. Filter for citations_delta specifically: the trailing
    // text block also emits a content_block_delta we must exclude.
    let opaque_delta_count = events
        .iter()
        .filter(|e| {
            e.event.as_deref() == Some("content_block_delta")
                && parse_data(e).pointer("/delta/type").and_then(Value::as_str)
                    == Some("citations_delta")
        })
        .count();
    assert!(
        opaque_delta_count > 0,
        "some citations_delta events must have been captured pre-cap; got 0",
    );
    assert!(
        opaque_delta_count < N,
        "post-cap citations_delta events must be sink-drained; got {opaque_delta_count} of {N}",
    );

    // Assert 4: the egress logged a WARN naming the byte-overflow
    // reason. Pin the reason field rather than the message string so
    // a future log-message tweak doesn't flake the test.
    let degrade_warn = captured.iter().find(|e| {
        e.level == tracing::Level::WARN
            && e.fields
                .iter()
                .any(|(k, v)| k == "reason" && v == "byte_overflow")
    });
    assert!(
        degrade_warn.is_some(),
        "byte_overflow degrade WARN must fire; captured: {captured:?}",
    );
}
