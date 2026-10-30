//! Every lane whose wire closes a turn with an explicit terminal event counts
//! a stream that ends without it exactly once, on the request's observation
//! slot and on the process total, through the real `stream()` against a mock
//! upstream. A terminated stream, a stream its consumer drops before the end,
//! and an openai-compat stream cut mid tool-call count nothing.
//!
//! The unit-test total is per thread and these `current_thread` tests poll
//! every stream on the test's own thread, so each delta is exactly what the
//! test caused.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aws_smithy_types::event_stream::{Header, HeaderValue, Message as FrameMessage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, Error, Provider, RequestObservation, Result, StaticToken, TokenSource,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, CloakConfig};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};
use crate::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use crate::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::stream_terminal_metrics::missing_terminal_total;

const PROVIDER_ID: &str = "terminal-count-test";

type Opened = Pin<Box<dyn Future<Output = BoxStream<'static, Result<ChatChunk>>> + Send>>;

/// One lane: its name, and a way to open its real stream against a mock
/// upstream serving a body that either carries the terminal event or is cut
/// before it.
struct Lane {
    name: &'static str,
    open: fn(ChatRequest, bool) -> Opened,
}

const LANES: &[Lane] = &[
    Lane {
        name: "anthropic-api",
        open: |req, terminated| Box::pin(open_anthropic(req, terminated)),
    },
    Lane {
        name: "openai-responses",
        open: |req, terminated| Box::pin(open_responses(req, terminated)),
    },
    Lane {
        name: "bedrock-invoke",
        open: |req, terminated| Box::pin(open_bedrock(BedrockApiShape::Invoke, req, terminated)),
    },
    Lane {
        name: "bedrock-converse",
        open: |req, terminated| Box::pin(open_bedrock(BedrockApiShape::Converse, req, terminated)),
    },
    Lane {
        name: "openai-compat",
        open: |req, terminated| Box::pin(open_compat(req, terminated)),
    },
];

fn request(slot: &RequestObservation) -> ChatRequest {
    let mut req = ChatRequest {
        model: "us.anthropic.claude-opus-5-5".into(),
        messages: vec![routectl_core::test_utils::user_msg("hi")].into(),
        max_tokens: Some(64),
        stream: Some(true),
        ..Default::default()
    };
    req.routectl_internal.observation = Some(slot.clone());
    req
}

/// Keep the mock alive for as long as the stream it answers is polled.
fn holding(
    server: MockServer,
    stream: BoxStream<'static, Result<ChatChunk>>,
) -> BoxStream<'static, Result<ChatChunk>> {
    let server = Arc::new(server);
    stream
        .map(move |item| {
            let _alive = &server;
            item
        })
        .boxed()
}

async fn serve(route: &str, body: Vec<u8>, content_type: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
        .mount(&server)
        .await;
    server
}

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn anthropic_events(terminated: bool) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
    ];
    if terminated {
        events.extend([
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ]);
    }
    events
}

async fn open_anthropic(
    req: ChatRequest,
    terminated: bool,
) -> BoxStream<'static, Result<ChatChunk>> {
    let server = serve(
        "/v1/messages",
        sse(&anthropic_events(terminated)),
        "text/event-stream",
    )
    .await;
    let provider = AnthropicApiProvider::new(AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: server.uri(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: crate::anthropic_api::AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        mantle: None,
    });
    let stream = provider.stream(req).await.expect("stream opens");
    holding(server, stream)
}

fn responses_events(terminated: bool) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "m1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
    ];
    if terminated {
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": "r", "status": "completed", "model": "m",
                "output": [{"type": "message", "id": "m1", "role": "assistant",
                            "content": [{"type": "output_text", "text": "hi"}]}],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        }));
    }
    events
}

async fn responses_provider(terminated: bool) -> (MockServer, OpenAiResponsesProvider) {
    let server = serve(
        "/responses",
        sse(&responses_events(terminated)),
        "text/event-stream",
    )
    .await;
    let provider = OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: PROVIDER_ID.into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: server.uri(),
        auth_kind: crate::openai_responses::AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        mantle: None,
    });
    (server, provider)
}

async fn open_responses(
    req: ChatRequest,
    terminated: bool,
) -> BoxStream<'static, Result<ChatChunk>> {
    let (server, provider) = responses_provider(terminated).await;
    let stream = provider.stream(req).await.expect("stream opens");
    holding(server, stream)
}

fn frame(event_type: &str, payload: &str) -> Vec<u8> {
    let message = FrameMessage::new(Bytes::from(payload.to_string().into_bytes()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ));
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut buf).expect("encode frame");
    buf
}

fn bedrock_body(shape: BedrockApiShape, terminated: bool) -> Vec<u8> {
    match shape {
        BedrockApiShape::Invoke => anthropic_events(terminated)
            .iter()
            .flat_map(|event| {
                let bytes = B64.encode(event.to_string().as_bytes());
                frame("chunk", &json!({ "bytes": bytes }).to_string())
            })
            .collect(),
        BedrockApiShape::Converse => {
            let mut frames = vec![
                ("messageStart", r#"{"role":"assistant"}"#),
                (
                    "contentBlockDelta",
                    r#"{"contentBlockIndex":0,"delta":{"text":"hello"}}"#,
                ),
                ("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            ];
            if terminated {
                frames.push(("messageStop", r#"{"stopReason":"end_turn"}"#));
            }
            frames
                .iter()
                .flat_map(|(event_type, payload)| frame(event_type, payload))
                .collect()
        }
    }
}

async fn open_bedrock(
    shape: BedrockApiShape,
    req: ChatRequest,
    terminated: bool,
) -> BoxStream<'static, Result<ChatChunk>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            bedrock_body(shape, terminated),
            "application/vnd.amazon.eventstream",
        ))
        .mount(&server)
        .await;
    let creds = BedrockCreds::BearerKey {
        key: "terminal-count-key".into(),
    };
    let cfg = BedrockConfig {
        id: PROVIDER_ID.into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    };
    let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(&server.uri());
    let stream = provider.stream(req).await.expect("stream opens");
    holding(server, stream)
}

fn compat_chunk(delta: &Value, finish_reason: Option<&str>) -> Value {
    json!({"id": "c1", "model": "m", "created": 1,
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]})
}

async fn open_compat_body(
    req: ChatRequest,
    body: Vec<u8>,
) -> BoxStream<'static, Result<ChatChunk>> {
    let server = serve("/chat/completions", body, "text/event-stream").await;
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: PROVIDER_ID.into(),
        base_url: server.uri(),
        api_key: "test-key".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        mantle: None,
    });
    let stream = provider.stream(req).await.expect("stream opens");
    holding(server, stream)
}

async fn open_compat(req: ChatRequest, terminated: bool) -> BoxStream<'static, Result<ChatChunk>> {
    let mut events = vec![compat_chunk(
        &json!({"role": "assistant", "content": "hello"}),
        None,
    )];
    if terminated {
        events.push(compat_chunk(&json!({}), Some("stop")));
    }
    open_compat_body(req, sse(&events)).await
}

async fn drain(stream: BoxStream<'static, Result<ChatChunk>>) -> Vec<Result<ChatChunk>> {
    stream.collect().await
}

fn is_missing_terminal(item: &Result<ChatChunk>) -> bool {
    matches!(
        item,
        Err(Error::Upstream { status: 0, body, .. })
            if body.ends_with(": stream ended without a terminal event")
    )
}

/// What one drained stream did to the slot and to the process total.
struct Counted {
    slot: u64,
    global: u64,
    items: Vec<Result<ChatChunk>>,
}

async fn count(lane: &Lane, terminated: bool) -> Counted {
    let slot = RequestObservation::new();
    let before = missing_terminal_total();

    let items = drain((lane.open)(request(&slot), terminated).await).await;

    Counted {
        slot: slot.snapshot().missing_terminals,
        global: missing_terminal_total() - before,
        items,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_cut_stream_counts_once_on_the_slot_and_the_total() {
    for lane in LANES {
        let counted = count(lane, false).await;

        assert!(
            counted.items.last().is_some_and(is_missing_terminal),
            "{}: the cut stream ends with the missing-terminal error: {:?}",
            lane.name,
            counted.items
        );
        assert_eq!(counted.slot, 1, "{} slot", lane.name);
        assert_eq!(counted.global, 1, "{} total", lane.name);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_terminated_stream_counts_nothing() {
    for lane in LANES {
        let counted = count(lane, true).await;

        assert!(
            counted.items.iter().all(Result::is_ok),
            "{}: a terminated stream is error-free: {:?}",
            lane.name,
            counted.items
        );
        assert_eq!(counted.slot, 0, "{} slot", lane.name);
        assert_eq!(counted.global, 0, "{} total", lane.name);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_cut_stream_dropped_by_its_consumer_before_the_end_counts_nothing() {
    for lane in LANES {
        let slot = RequestObservation::new();
        let before = missing_terminal_total();
        let mut stream = (lane.open)(request(&slot), false).await;

        let first = stream.next().await;
        drop(stream);

        assert!(
            first.as_ref().is_some_and(Result::is_ok),
            "{}: the cut body still yields content first: {first:?}",
            lane.name
        );
        assert_eq!(slot.snapshot().missing_terminals, 0, "{} slot", lane.name);
        assert_eq!(missing_terminal_total() - before, 0, "{} total", lane.name);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_responses_complete_on_a_cut_stream_counts_once() {
    for (terminated, expected) in [(false, 1), (true, 0)] {
        let slot = RequestObservation::new();
        let before = missing_terminal_total();
        let (_server, provider) = responses_provider(terminated).await;

        let result = provider.complete(request(&slot)).await;

        assert_eq!(result.is_err(), !terminated, "terminated={terminated}");
        assert_eq!(
            slot.snapshot().missing_terminals,
            expected,
            "terminated={terminated} slot"
        );
        assert_eq!(
            missing_terminal_total() - before,
            expected,
            "terminated={terminated} total"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_openai_compat_stream_cut_mid_tool_call_is_not_a_counted_missing_terminal() {
    let tool_delta = json!({"role": "assistant", "tool_calls": [{
        "index": 0, "id": "call_1", "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"city\":"}
    }]});
    let slot = RequestObservation::new();
    let before = missing_terminal_total();

    let items =
        drain(open_compat_body(request(&slot), sse(&[compat_chunk(&tool_delta, None)])).await)
            .await;

    match items.last() {
        Some(Err(Error::Streaming(message))) => assert!(
            message.contains("tool call is likely truncated"),
            "unexpected message: {message}"
        ),
        other => panic!("expected the truncated tool-call error, got {other:?}"),
    }
    assert_eq!(slot.snapshot().missing_terminals, 0, "slot");
    assert_eq!(missing_terminal_total() - before, 0, "total");
}
