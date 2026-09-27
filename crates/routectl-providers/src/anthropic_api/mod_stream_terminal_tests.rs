//! A stream that crossed HTTP 200 must still tell the client whether the turn
//! finished. The Messages wire closes every turn with `message_stop`; a body
//! that ends before it is a cut stream and must surface as an error, while
//! every complete stream -- including gateway variants that append a `[DONE]`
//! sentinel or carry benign `error` fields -- must stay error-free.
//!
//! Driven through the real `stream()` against a mock upstream, so the
//! assertions span the HTTP-200 boundary rather than starting after it.

use super::*;
use futures::StreamExt;
use routectl_core::{ChatChunk, ChatRequest, Message, MessageContent, Role};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROVIDER_ID: &str = "stream-terminal-test";

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_t","type":"message","role":"assistant","content":[],"model":"claude-3-opus","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}}"#;
const BLOCK_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const TEXT_DELTA: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
const BLOCK_STOP: &str = r#"{"type":"content_block_stop","index":0}"#;
const MESSAGE_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#;
const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;
const PING: &str = r#"{"type":"ping"}"#;

fn sse(events: &[&str]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn full_turn() -> Vec<&'static str> {
    vec![
        MESSAGE_START,
        BLOCK_START,
        TEXT_DELTA,
        BLOCK_STOP,
        MESSAGE_DELTA,
        MESSAGE_STOP,
    ]
}

fn provider_for(base_url: &str) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn stream_req() -> ChatRequest {
    ChatRequest {
        model: "claude-3-opus".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        stream: Some(true),
        ..Default::default()
    }
}

/// Serve `body` as a 200 SSE response and drain the provider's stream.
async fn drain(body: String) -> Vec<Result<ChatChunk>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = provider_for(&server.uri());
    let mut stream = provider.stream(stream_req()).await.expect("stream opens");
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

fn errors(items: &[Result<ChatChunk>]) -> Vec<&Error> {
    items.iter().filter_map(|r| r.as_ref().err()).collect()
}

fn final_finish_reason(items: &[Result<ChatChunk>]) -> Option<String> {
    items
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter_map(|c| c.choices.first()?.finish_reason.clone())
        .next_back()
}

/// The stream's last item is the missing-terminal error: an upstream-class
/// error (status 0, the network-failure class) naming the cause.
fn assert_ends_with_missing_terminal(items: &[Result<ChatChunk>]) {
    let errs = errors(items);
    assert_eq!(errs.len(), 1, "exactly one error expected, got {errs:?}");
    let last = items.last().expect("stream yielded items");
    match last {
        Err(Error::Upstream {
            provider,
            status,
            body,
            ..
        }) => {
            assert_eq!(provider, PROVIDER_ID);
            assert_eq!(*status, 0);
            assert!(
                body.contains("stream ended without a terminal event"),
                "unexpected body: {body}"
            );
        }
        other => panic!("expected a trailing missing-terminal Upstream error, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_stream_ends_without_error() {
    let items = drain(sse(&full_turn())).await;

    assert!(
        errors(&items).is_empty(),
        "healthy stream errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn done_sentinel_after_message_stop_ends_without_error() {
    let mut events = full_turn();
    events.push("[DONE]");

    let items = drain(sse(&events)).await;

    assert!(
        errors(&items).is_empty(),
        "[DONE] after message_stop errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn benign_error_fields_and_keepalives_do_not_fail_a_complete_stream() {
    let delta_with_null_error = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"},"error":null}"#;
    let message_delta_with_empty_error = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2},"error":{}}"#;
    let events = [
        MESSAGE_START,
        PING,
        BLOCK_START,
        delta_with_null_error,
        BLOCK_STOP,
        PING,
        message_delta_with_empty_error,
        MESSAGE_STOP,
    ];
    let body = format!(": keepalive comment\n\n{}", sse(&events));

    let items = drain(body).await;

    assert!(
        errors(&items).is_empty(),
        "benign fields errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn stream_cut_mid_content_surfaces_missing_terminal_error() {
    let items = drain(sse(&[MESSAGE_START, BLOCK_START, TEXT_DELTA])).await;

    assert!(
        items.iter().any(|r| r
            .as_ref()
            .is_ok_and(|c| c.choices[0].delta.content.as_deref() == Some("hello"))),
        "content before the cut still reaches the client"
    );
    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn stream_cut_after_message_delta_before_message_stop_surfaces_error() {
    let items = drain(sse(&[
        MESSAGE_START,
        BLOCK_START,
        TEXT_DELTA,
        BLOCK_STOP,
        MESSAGE_DELTA,
    ]))
    .await;

    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn done_sentinel_before_message_stop_surfaces_missing_terminal_error() {
    let items = drain(sse(&[MESSAGE_START, BLOCK_START, TEXT_DELTA, "[DONE]"])).await;

    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn empty_success_body_surfaces_missing_terminal_error() {
    let items = drain(String::new()).await;

    assert_eq!(items.len(), 1, "only the error is yielded: {items:?}");
    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn in_band_error_event_surfaces_its_own_error_only() {
    let overloaded =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

    let items = drain(sse(&[MESSAGE_START, BLOCK_START, TEXT_DELTA, overloaded])).await;

    let errs = errors(&items);
    assert_eq!(
        errs.len(),
        1,
        "one error, not a second missing-terminal: {errs:?}"
    );
    match errs[0] {
        Error::Upstream { body, .. } => assert!(
            body.contains("overloaded_error"),
            "the upstream's own error must surface: {body}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}
