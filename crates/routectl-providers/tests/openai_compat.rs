//! Integration tests for the openai-compat provider.
//!
//! Unit tests for request/response/SSE normalization live inside each
//! submodule's own #[cfg(test)] blocks. Here we exercise:
//!   - wiremock-based complete() and stream() end-to-end paths.
//!   - DeepSeek multi-turn: reasoning_content stripped from outgoing history.
//!   - Cross-crate smoke: provider id is returned on ChatResponse.

#![cfg(feature = "openai-compat")]

use futures::StreamExt;
use routectl_core::{Message, MessageContent, Provider, ReasoningDetailKind, Role};
use routectl_providers::openai_compat::{
    OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_testkit::redirect_pin::CrossHostRedirect;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_provider(base_url: &str, dialect: ReasoningDialect) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        reasoning_dialect: dialect,
        ..OpenAiCompatConfig::new("test-provider", base_url, "test-key")
    })
}

fn user_request(model: &str) -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: model.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("What is 2+2?".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        temperature: Some(0.5),
        max_tokens: Some(128),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// extra_headers reserved-name guard (parity with anthropic_api / bedrock)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_headers_reserved_name_does_not_override_authorization() {
    // TOML-supplied `extra_headers = { "authorization" = "..." }` must not
    // bypass the provider's Bearer auth. HeaderMap::insert replaces by
    // name, so without the is_auth_header skip path in apply_header_extras
    // this would silently override the auth header and ship the
    // user-supplied value upstream.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer real-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "ok",
            "model": "test",
            "created": 1,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        // Attempt to override with a different value -- must be ignored.
        header_extras: vec![("authorization".into(), "Bearer attacker-token".into())],
        ..OpenAiCompatConfig::new("test-provider", server.uri(), "real-token")
    });

    // If the guard is missing, the wiremock matcher above won't find
    // "Bearer real-token" (the override would have replaced it) and the
    // mock server returns 404, surfacing here as an upstream error.
    let resp = provider.complete(user_request("test")).await;
    assert!(
        resp.is_ok(),
        "guard must keep the real Bearer token: {resp:?}"
    );
}

// ---------------------------------------------------------------------------
// complete() integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_openai_dialect_returns_normalized_response() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-001",
        "model": "gpt-4o",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Four."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "total_tokens": 12
        }
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let req = user_request("gpt-4o");
    let resp = provider.complete(req).await.unwrap();

    assert_eq!(resp.id, "chatcmpl-001");
    assert_eq!(resp.routectl_provider.as_deref(), Some("test-provider"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "Four."),
        _ => panic!("expected text content"),
    }
}

#[tokio::test]
async fn complete_deepseek_lifts_reasoning_content() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-ds-001",
        "model": "deepseek-reasoner",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 4.",
                "reasoning": "2 plus 2 equals 4."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 30,
            "total_tokens": 40
        }
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);
    let req = user_request("deepseek-reasoner");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
    assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
    assert_eq!(details[0].payload["text"], "2 plus 2 equals 4.");
}

#[tokio::test]
async fn complete_raw_think_tag_strips_and_lifts() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-think-001",
        "model": "qwq-32b",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "<think>inner reasoning</think>Outer answer."
            },
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);
    let req = user_request("qwq-32b");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("raw-think-tag-v1"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t.trim(), "Outer answer."),
        _ => panic!("expected text"),
    }
}

#[tokio::test]
async fn complete_upstream_error_surfaces_as_error_upstream() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 429);
            assert!(body.contains("rate limited"));
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

#[tokio::test]
async fn complete_429_populates_upstream_type_and_code() {
    // Arrange
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "rate_limit_exceeded",
                "code": "rate_limited",
                "message": "rate limited"
            }
        })))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    // Act
    let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

    // Assert: the upstream classifier is lifted into upstream_type/code.
    match err {
        routectl_core::Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
            assert_eq!(upstream_code.as_deref(), Some("rate_limited"));
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// stream() integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_openai_dialect_collects_chunks() {
    let server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chunk-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let mut stream = provider.stream(user_request("gpt-4o")).await.unwrap();

    let mut content_parts = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                content_parts.push(c.clone());
            }
        }
    }

    assert_eq!(content_parts.join(""), "Hello");
}

#[tokio::test]
async fn stream_deepseek_lifts_reasoning_content_in_chunks() {
    let server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"chain of\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" thought\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"ds-1\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);
    let mut stream = provider
        .stream(user_request("deepseek-reasoner"))
        .await
        .unwrap();

    let mut reasoning_chunks: Vec<String> = Vec::new();
    let mut content_chunks: Vec<String> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            for detail in &choice.delta.reasoning_details {
                if let Some(t) = detail.payload["text"].as_str() {
                    reasoning_chunks.push(t.into());
                }
            }
            if let Some(c) = &choice.delta.content {
                content_chunks.push(c.clone());
            }
        }
    }

    assert_eq!(reasoning_chunks.join(""), "chain of thought");
    assert_eq!(content_chunks.join(""), "answer");
}

#[tokio::test]
async fn stream_raw_think_tag_state_machine_across_chunks() {
    let server = MockServer::start().await;

    // The <think> tag opens in chunk 1 and closes in chunk 2 -- classic split.
    let sse_body = concat!(
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"before<think>partial\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" thought</think>after\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);
    let mut stream = provider.stream(user_request("qwq-32b")).await.unwrap();

    let mut outside = String::new();
    let mut reasoning = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                outside.push_str(c);
            }
            if let Some(r) = &choice.delta.reasoning {
                reasoning.push_str(r);
            }
        }
    }

    assert_eq!(outside, "beforeafter");
    assert_eq!(reasoning, "partial thought");
}

/// Pin: when a RawThinkTag stream ends (`[DONE]`) while the accumulator
/// is still holding back bytes that are a prefix of `<think>` (e.g.
/// `<thi`), those bytes are real visible content and must reach the
/// client. Before the flush they were silently dropped at the `[DONE]`
/// exit. Upstream emits `hello<thi` then `[DONE]` and never completes
/// the tag; the client must receive the full `hello<thi`.
#[tokio::test]
async fn stream_raw_think_tag_flushes_pending_on_done() {
    let server = MockServer::start().await;

    // One chunk whose content ends in a partial `<think>` prefix, then
    // a bare `[DONE]` -- the tag never completes.
    let sse_body = concat!(
        "data: {\"id\":\"t1\",\"model\":\"qwq-32b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello<thi\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);
    let mut stream = provider.stream(user_request("qwq-32b")).await.unwrap();

    let mut outside = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if let Some(c) = &choice.delta.content {
                outside.push_str(c);
            }
        }
    }

    // All bytes survive: the held-back `<thi` is flushed at [DONE].
    assert_eq!(outside, "hello<thi");
}

async fn stream_items(
    sse_body: &'static str,
    dialect: ReasoningDialect,
) -> Vec<routectl_core::Result<routectl_core::ChatChunk>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), dialect);
    provider
        .stream(user_request("m"))
        .await
        .unwrap()
        .collect()
        .await
}

fn streamed_content(items: &[routectl_core::Result<routectl_core::ChatChunk>]) -> String {
    items
        .iter()
        .filter_map(|i| i.as_ref().ok())
        .flat_map(|c| &c.choices)
        .filter_map(|choice| choice.delta.content.as_deref())
        .collect()
}

fn assert_ends_with_one_missing_terminal(
    items: &[routectl_core::Result<routectl_core::ChatChunk>],
) {
    let errs = items.iter().filter(|i| i.is_err()).count();
    assert_eq!(errs, 1, "exactly one error; items: {items:?}");
    match items.last() {
        Some(Err(routectl_core::Error::Upstream {
            provider,
            status,
            body,
            ..
        })) => {
            assert_eq!(provider, "test-provider");
            assert_eq!(*status, 0);
            assert_eq!(body, "openai-compat: stream ended without a terminal event");
        }
        other => panic!("stream must end with the missing-terminal error; got {other:?}"),
    }
}

/// A text stream that closes with neither `[DONE]` nor a finish_reason
/// emits its content, then ends with exactly one missing-terminal error.
#[tokio::test]
async fn stream_closed_without_terminal_signal_emits_content_then_one_error() {
    let total_before = routectl_providers::stream_terminal_metrics::missing_terminal_total();

    let items = stream_items(
        concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
        ),
        ReasoningDialect::OpenAi,
    )
    .await;

    assert_eq!(streamed_content(&items), "Hello");
    assert_ends_with_one_missing_terminal(&items);
    // The process total is shared with this binary's other tests, so only a
    // lower bound on the delta is stable.
    assert!(
        routectl_providers::stream_terminal_metrics::missing_terminal_total() > total_before,
        "the cut stream advances the process-wide missing-terminal total"
    );
}

/// An empty body (no event at all) is the same missing-terminal error.
#[tokio::test]
async fn stream_closed_before_any_event_is_one_error() {
    let items = stream_items("", ReasoningDialect::OpenAi).await;

    assert_eq!(items.len(), 1, "items: {items:?}");
    assert_ends_with_one_missing_terminal(&items);
}

/// A finish_reason without `[DONE]` still terminates the stream cleanly.
#[tokio::test]
async fn stream_closed_after_finish_reason_without_done_ends_cleanly() {
    let items = stream_items(
        concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        ReasoningDialect::OpenAi,
    )
    .await;

    assert_eq!(streamed_content(&items), "Hello");
    assert!(items.iter().all(Result::is_ok), "items: {items:?}");
}

macro_rules! malformed {
    () => {
        "data: {\"id\":\"c1\",\"choices\":[\n\n"
    };
}

macro_rules! in_band_error {
    () => {
        "data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n"
    };
}

fn assert_event_error(name: &str, item: &routectl_core::Result<routectl_core::ChatChunk>) {
    match item {
        Err(routectl_core::Error::Upstream { body, .. })
            if body == "openai-compat: stream ended without a terminal event" =>
        {
            panic!("{name}: the event's own error, not the missing-terminal one");
        }
        Err(_) => {}
        Ok(chunk) => panic!("{name}: expected an error, got {chunk:?}"),
    }
}

/// An event the stream cannot use (malformed JSON, or an in-band upstream
/// error envelope) ends the stream with that one error: nothing after it is
/// emitted, neither a later valid event nor the missing-terminal error.
#[tokio::test]
async fn stream_error_event_is_the_last_item() {
    macro_rules! content_then_done {
        () => {
            concat!(
                "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n",
                "data: [DONE]\n\n",
            )
        };
    }
    let rows: [(&str, &'static str); 4] = [
        ("malformed then EOF", malformed!()),
        (
            "malformed, valid content, [DONE]",
            concat!(malformed!(), content_then_done!()),
        ),
        ("in-band error then EOF", in_band_error!()),
        (
            "in-band error, valid content, [DONE]",
            concat!(in_band_error!(), content_then_done!()),
        ),
    ];

    for (name, body) in rows {
        let items = stream_items(body, ReasoningDialect::OpenAi).await;

        assert_eq!(items.len(), 1, "{name}: exactly one item; items: {items:?}");
        assert_eq!(streamed_content(&items), "", "{name}");
        assert_event_error(name, &items[0]);
    }
}

/// On a RawThinkTag stream, bytes held back as a possible `<think>` prefix
/// are real content: an unusable event flushes them, then ends the stream
/// with its own error.
#[tokio::test]
async fn raw_think_tag_error_event_flushes_pending_then_ends() {
    macro_rules! partial_tag {
        () => {
            "data: {\"id\":\"t1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello<thi\"},\"finish_reason\":null}]}\n\n"
        };
    }
    let rows: [(&str, &'static str); 2] = [
        (
            "partial tag then malformed",
            concat!(partial_tag!(), malformed!()),
        ),
        (
            "partial tag then in-band error",
            concat!(partial_tag!(), in_band_error!()),
        ),
    ];

    for (name, body) in rows {
        let items = stream_items(body, ReasoningDialect::RawThinkTag).await;

        assert_eq!(items.len(), 3, "{name}: items: {items:?}");
        let content = |i: usize| {
            items[i]
                .as_ref()
                .ok()
                .and_then(|c| c.choices.first())
                .and_then(|choice| choice.delta.content.clone())
        };
        assert_eq!(content(0).as_deref(), Some("hello"), "{name}");
        assert_eq!(content(1).as_deref(), Some("<thi"), "{name}");
        assert_event_error(name, &items[2]);
    }
}

/// RawThinkTag bytes held back as a possible `<think>` prefix are real
/// content: a cut stream flushes them before its missing-terminal error.
#[tokio::test]
async fn raw_think_tag_stream_cut_flushes_pending_before_the_error() {
    let items = stream_items(
        "data: {\"id\":\"t1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello<thi\"},\"finish_reason\":null}]}\n\n",
        ReasoningDialect::RawThinkTag,
    )
    .await;

    assert_eq!(streamed_content(&items), "hello<thi");
    assert_ends_with_one_missing_terminal(&items);
}

/// A loopback upstream that answers one request with a 200 SSE head whose
/// `Content-Length` promises more than `partial_body`, writes the body, then
/// closes: the client sees the event, then a transport error on the body.
async fn upstream_cut_after(partial_body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
            partial_body.len() + 4096
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(partial_body.as_bytes()).await;
        let _ = socket.flush().await;
        drop(socket);
    });
    base
}

/// RawThinkTag bytes held back as a possible `</think>` prefix reach the
/// client before the transport error that cuts the stream mid-reasoning.
#[tokio::test]
async fn transport_error_flushes_held_think_bytes_first() {
    // Arrange: "abc" is reasoning; the trailing "</th" is held as a
    // possible closing tag when the connection drops.
    let base = upstream_cut_after(
        "data: {\"id\":\"t1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>abc</th\"},\"finish_reason\":null}]}\n\n",
    )
    .await;
    let provider = make_provider(&base, ReasoningDialect::RawThinkTag);

    // Act
    let items: Vec<_> = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        provider
            .stream(user_request("m"))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
    })
    .await
    .expect("the cut stream settles within the bound");

    // Assert
    assert_eq!(items.len(), 3, "items: {items:?}");
    let delta = |i: usize| {
        items[i]
            .as_ref()
            .ok()
            .and_then(|c| c.choices.first())
            .map(|choice| choice.delta.clone())
    };
    assert_eq!(
        delta(0).and_then(|d| d.reasoning).as_deref(),
        Some("abc"),
        "the reasoning chunk comes first"
    );
    assert_eq!(
        delta(1).and_then(|d| d.content).as_deref(),
        Some("</th"),
        "the held bytes come before the error"
    );
    match &items[2] {
        Err(routectl_core::Error::Streaming(msg)) => {
            assert!(msg.contains("SSE error"), "transport error, got {msg:?}");
        }
        other => panic!("expected the transport error last, got {other:?}"),
    }
}

/// RawThinkTag bytes held back as a possible `</think>` prefix reach the
/// client before the error that ends the stream on an unpairable tool call.
#[tokio::test]
async fn unpairable_tool_call_error_flushes_held_think_bytes_first() {
    // Arrange: "abc" is reasoning and "</th" is held; the next event's
    // id-less tool call has no `index`, so no stable id can be minted.
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"id\":\"t1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<think>abc</th\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"t1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::RawThinkTag);

    // Act
    let items: Vec<_> = provider
        .stream(user_request("m"))
        .await
        .unwrap()
        .collect()
        .await;

    // Assert
    assert_eq!(items.len(), 3, "items: {items:?}");
    let delta = |i: usize| {
        items[i]
            .as_ref()
            .ok()
            .and_then(|c| c.choices.first())
            .map(|choice| choice.delta.clone())
    };
    assert_eq!(
        delta(0).and_then(|d| d.reasoning).as_deref(),
        Some("abc"),
        "the reasoning chunk comes first"
    );
    assert_eq!(
        delta(1).and_then(|d| d.content).as_deref(),
        Some("</th"),
        "the held bytes come before the error"
    );
    match &items[2] {
        Err(routectl_core::Error::Streaming(msg)) => {
            assert!(msg.contains("tool_call"), "tool-call error, got {msg:?}");
        }
        other => panic!("expected the tool-call error last, got {other:?}"),
    }
}

/// Pin: for `n > 1` the streaming stop-sequence heuristic must track
/// per-choice content, not one shared buffer. Choice 0 streams `fooEND`
/// (ends with the configured stop -> matches); choice 1 streams `bar`
/// (no stop -> must NOT match). A single shared accumulator would bleed
/// choice 0's `END` suffix into choice 1's match, producing a false
/// positive on choice 1.
///
/// Two stop sequences are configured so the single-stop fallback in
/// `detect_matched_stop_sequence` cannot fire: this isolates the
/// per-choice suffix-match contract, not the single-fence fallback.
#[tokio::test]
async fn stream_stop_sequence_heuristic_is_per_choice_for_n_gt_1() {
    let server = MockServer::start().await;

    // Two choices interleaved across content chunks, then a terminal
    // chunk carrying finish_reason="stop" for both. Choice 0 -> "fooEND",
    // choice 1 -> "bar".
    let sse_body = concat!(
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"foo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":1,\"delta\":{\"content\":\"bar\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"END\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"},{\"index\":1,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    let mut req = user_request("m");
    req.n = Some(2);
    req.stop = Some(vec!["END".to_string(), "STOP".to_string()]);

    let mut stream = provider.stream(req).await.unwrap();

    // Collect the matched_stop_sequence per choice index from the
    // terminal chunk (the one carrying finish_reason="stop").
    let mut matched: std::collections::HashMap<u32, Option<String>> =
        std::collections::HashMap::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for choice in &chunk.choices {
            if choice.finish_reason.as_deref() == Some("stop") {
                matched.insert(choice.index, choice.matched_stop_sequence.clone());
            }
        }
    }

    assert_eq!(
        matched.get(&0).cloned().flatten().as_deref(),
        Some("END"),
        "choice 0 streamed 'fooEND' -> must match the stop sequence",
    );
    assert_eq!(
        matched.get(&1).cloned().flatten(),
        None,
        "choice 1 streamed 'bar' -> must NOT match (no per-choice bleed)",
    );
}

// ---------------------------------------------------------------------------
// DeepSeek multi-turn: outgoing body must not contain reasoning_content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deepseek_multiturn_strips_reasoning_from_outgoing_body() {
    let server = MockServer::start().await;

    // We capture the request body to inspect it.
    let response_body = json!({
        "id": "chatcmpl-mt-001",
        "model": "deepseek-reasoner",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    // Build a request whose history contains an assistant message with
    // reasoning_details and reasoning fields populated.
    let mut req = user_request("deepseek-reasoner");
    let mut msgs = req.messages.to_vec();
    msgs.push(Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("Prior answer".into()),
        reasoning: Some("prior chain of thought".into()),
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extras: serde_json::Map::new(),
    });
    msgs.push(Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text("Follow-up question".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extras: serde_json::Map::new(),
    });
    req.messages = msgs.into();

    let provider = make_provider(&server.uri(), ReasoningDialect::DeepSeek);

    // Inspect the normalized request before it goes out.
    let normalized = provider.normalize_request(&req).unwrap();
    let messages = normalized["messages"].as_array().unwrap();
    for msg in messages {
        assert!(
            msg.get("reasoning_content").is_none(),
            "reasoning_content must not be in outgoing body: {msg}"
        );
        assert!(
            msg.get("reasoning").is_none(),
            "reasoning must not be in outgoing body: {msg}"
        );
        assert!(
            msg.get("reasoning_details").is_none(),
            "reasoning_details must not be in outgoing body: {msg}"
        );
    }

    // Also verify the round-trip through complete() succeeds.
    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "chatcmpl-mt-001");
}

// ---------------------------------------------------------------------------
// strict_translation: lossy seam policy
// ---------------------------------------------------------------------------

#[test]
fn strict_translation_off_warns_and_allows_request() {
    // Default mode: cache_control on a user text block is silently dropped
    // (warn-only), and the request body still serializes. Pin that the
    // request reaches the upstream wire shape without erroring.
    use routectl_core::{
        ChatRequest, KnownContentPart, cache_control::CacheControl, content_part::ContentPart,
    };
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig::new(
        "openai-compat:test",
        "http://localhost",
        "k",
    ));
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "hi".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_5m()),
                extras: serde_json::Map::new(),
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        ..Default::default()
    };
    let body = provider
        .normalize_request(&req)
        .expect("default warns, returns Ok");
    assert!(
        body.get("cache_control").is_none(),
        "wire body must not carry cache_control on the openai-compat seam"
    );
}

#[test]
fn strict_translation_on_rejects_canonical_only_fields() {
    // Strict mode: same lossy seam returns an Error::Validation that
    // names the offending fields. Wired through OpenAiCompatConfig from
    // [server] strict_translation at provider build time.
    use routectl_core::{ChatRequest, cache_control::CacheControl};
    let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
        strict_translation: true,
        ..OpenAiCompatConfig::new("openai-compat:strict", "http://localhost", "k")
    });
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        anthropic_beta: vec!["context-1m-2025-08-07".into()],
        ..Default::default()
    };
    let err = provider
        .normalize_request(&req)
        .expect_err("strict_translation must reject canonical-only fields");
    let msg = format!("{err}");
    assert!(
        msg.contains("strict_translation"),
        "expected strict_translation marker in error: {msg}"
    );
    assert!(
        msg.contains("cache_control") && msg.contains("anthropic_beta"),
        "expected both dropped fields named in error: {msg}"
    );
    // The strict-translation rejection is client-facing (HTTP 400), so it
    // must name the offending feature -- never the internal egress/provider
    // id, which would leak routing topology to a remote caller.
    assert!(
        !msg.contains("openai-compat:strict"),
        "strict_translation error must not echo the provider id: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Dialect format tag correctness
// ---------------------------------------------------------------------------

#[test]
fn dialect_format_tags_are_correct() {
    use routectl_providers::openai_compat::ReasoningDialect;
    assert_eq!(ReasoningDialect::OpenAi.format_tag(), "openai-responses-v1");
    assert_eq!(ReasoningDialect::DeepSeek.format_tag(), "deepseek-v1");
    assert_eq!(ReasoningDialect::Vllm.format_tag(), "vllm-reasoning-v1");
    assert_eq!(
        ReasoningDialect::RawThinkTag.format_tag(),
        "raw-think-tag-v1"
    );
    assert_eq!(
        ReasoningDialect::OpenRouter.format_tag(),
        "openrouter-passthrough-v1"
    );
    assert_eq!(ReasoningDialect::Passthrough.format_tag(), "passthrough-v1");
}

// ---------------------------------------------------------------------------
// Passthrough and OpenRouter: no mutation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_passthrough_no_mutation() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-pt-001",
        "model": "some-model",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pass"},
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::Passthrough);
    let resp = provider.complete(user_request("some-model")).await.unwrap();
    assert_eq!(resp.choices[0].message.reasoning_details.len(), 0);
}

#[tokio::test]
async fn complete_vllm_lifts_reasoning_content() {
    let server = MockServer::start().await;

    let response_body = json!({
        "id": "chatcmpl-vllm-001",
        "model": "qwen3-30b",
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "result",
                "reasoning": "vllm trace"
            },
            "finish_reason": "stop"
        }]
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::Vllm);
    let req = user_request("qwen3-30b");
    let resp = provider.complete(req).await.unwrap();

    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
    assert_eq!(details[0].payload["text"], "vllm trace");
}

// ---------------------------------------------------------------------------
// probe(): free reachability against /models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_200_models_list_is_reachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1) // AT MOST ONE upstream request: no retry.
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    assert_eq!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Reachable
    );
}

#[tokio::test]
async fn probe_401_is_auth_failed_without_leaking_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    match provider.probe().await {
        routectl_core::ProbeOutcome::AuthFailed(reason) => {
            assert!(!reason.contains("test-key"), "reason leaked the api key");
            assert!(!reason.contains(&server.uri()), "reason leaked the url");
        }
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_403_is_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::AuthFailed(_)
    ));
}

#[tokio::test]
async fn probe_connection_refused_is_unreachable() {
    // A closed loopback port (nothing binds 127.0.0.1:1) deterministically
    // refuses the connect. Stands in for DNS / connect / TLS transport
    // failures, which all fold into Unreachable.
    let provider = make_provider("http://127.0.0.1:1", ReasoningDialect::OpenAi);
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Unreachable(_)
    ));
}

/// The `Retry-After` hint reaches the error only on a rate-limit status: a
/// stray header on a 400 must not park the provider.
#[tokio::test]
async fn complete_retry_after_is_gated_on_rate_limit_status() {
    let cases: [(&str, u16, Option<std::time::Duration>); 2] = [
        (
            "429 carries the hint",
            429,
            Some(std::time::Duration::from_secs(30)),
        ),
        ("400 drops the hint", 400, None),
    ];
    for (name, status, want) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({"error": {"type": "t", "message": "m"}})),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

        let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

        match err {
            routectl_core::Error::Upstream { retry_after, .. } => {
                assert_eq!(retry_after, want, "{name}");
            }
            other => panic!("{name}: expected Upstream, got: {other:?}"),
        }
    }
}

/// A flat mantle body with no `__type` takes its exception type from the
/// `x-amzn-errortype` header (the coral URL tail is dropped).
#[tokio::test]
async fn complete_flat_body_lifts_type_from_aws_error_type_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header(
                    "x-amzn-errortype",
                    "ValidationException:urn:example:ignored-suffix",
                )
                .set_body_json(json!({"message": "invalid model identifier"})),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    let err = provider.complete(user_request("gpt-4o")).await.unwrap_err();

    match err {
        routectl_core::Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => {
            assert_eq!(status, 400);
            assert_eq!(upstream_type.as_deref(), Some("ValidationException"));
            assert_eq!(upstream_code, None);
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Response-body cap adoption (the DoS response-body cap). One byte over the 16 MiB ceiling
// exercised end-to-end; wiremock advertises an honest Content-Length, so the
// fast-reject guard trips without transferring the whole body.
// ---------------------------------------------------------------------------

/// One byte past the shared 16 MiB response-body cap.
const OVER_CAP_BODY_LEN: usize = 16 * 1024 * 1024 + 1;

#[tokio::test]
async fn complete_success_body_over_cap_maps_to_502() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    let (result, events) =
        routectl_testkit::with_capture(provider.complete(user_request("gpt-4o"))).await;
    let err = result.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 502, "an unreadable 2xx body must classify as 502");
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }

    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("path").is_some())
        .collect();
    assert_eq!(cap_warns.len(), 1, "exactly one cap-trip WARN per trip");
    let w = cap_warns[0];
    assert_eq!(w.field("provider"), Some("test-provider"));
    assert_eq!(w.field("status"), Some("200"));
    assert_eq!(
        w.field("body_cap_bytes"),
        Some((16 * 1024 * 1024).to_string().as_str())
    );
    assert_eq!(
        w.field("content_length"),
        Some(format!("Some({OVER_CAP_BODY_LEN})").as_str())
    );
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("complete_success_body"));
}

#[tokio::test]
async fn complete_error_body_over_cap_preserves_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    let (result, events) =
        routectl_testkit::with_capture(provider.complete(user_request("gpt-4o"))).await;
    let err = result.unwrap_err();

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(
                status, 429,
                "the original upstream status must be preserved"
            );
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }

    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message == "upstream response body exceeded cap; truncated"
        })
        .collect();
    assert_eq!(cap_warns.len(), 1, "exactly one cap-trip WARN per trip");
    let w = cap_warns[0];
    assert_eq!(w.field("provider"), Some("test-provider"));
    assert_eq!(w.field("status"), Some("429"));
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(w.field("path"), Some("error_body"));
}

#[tokio::test]
async fn stream_error_body_over_cap_preserves_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_bytes(vec![b'a'; OVER_CAP_BODY_LEN]))
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

    // `stream()` yields a `BoxStream` that is not `Debug`, so `unwrap_err`
    // is unavailable; match the `Result` directly.
    let err = match provider.stream(user_request("gpt-4o")).await {
        Ok(_) => panic!("expected an upstream error, got a stream"),
        Err(e) => e,
    };

    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(
                status, 503,
                "the original upstream status must be preserved"
            );
            // Exact match proves the body is the bounded fixed message, not
            // an echo of the 16 MiB raw upstream body.
            assert_eq!(body, "response body exceeded 16777216-byte cap");
        }
        other => panic!("expected Upstream, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Redirect policy: the shared no-redirect client hands back the 3xx itself
// (not a followed response), so the egress must map it to an upstream
// server fault rather than let it fall into the success/parse path.
// ---------------------------------------------------------------------------

/// Two-server cross-host pin for the first-party lane, mirroring the
/// anthropic-api lane's `first_party_lane_does_not_follow_cross_host_redirect`.
/// The redirect target answers 200, so a followed hop would look like a
/// successful call -- only this lane's own no-redirect client keeps the
/// target untouched and the `Authorization: Bearer` on the configured host.
#[tokio::test]
async fn first_party_lane_does_not_follow_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;

    let provider = make_provider(&pin.origin_uri(), ReasoningDialect::OpenAi);
    let err = provider
        .complete(user_request("test-model"))
        .await
        .unwrap_err();

    pin.assert_not_followed(&err, "openai-compat").await;
}

// ---------------------------------------------------------------------------
// Bedrock mantle lane wire behavior: SigV4/bearer-signed egress, no
// first-party Bearer, a no-redirect client, and the deterministic 501
// count_tokens capability signal. These pin the runtime lane against a mock
// upstream; the credential-scope and URL-builder units live in
// `routectl-providers/src/mantle.rs`.
// ---------------------------------------------------------------------------

#[cfg(feature = "bedrock")]
mod mantle {
    use super::*;
    use routectl_core::Error;
    use routectl_providers::anthropic_api::MantleAuth;
    use routectl_providers::bedrock::BedrockCreds;
    use routectl_providers::bedrock::auth::resolve;

    use serde_json::Value;

    /// A mantle-lane compat provider posting to `base_url` with a resolved
    /// credential. `base_url` points at wiremock (the factory derives the
    /// real host from the region; here we still sign under the region
    /// scope). The `api_key` is empty, mirroring the config-validation
    /// invariant on the lane.
    async fn mantle_provider(base_url: &str, creds: BedrockCreds) -> OpenAiCompatProvider {
        let resolved = resolve(&creds, "us-west-2").await.unwrap();
        OpenAiCompatProvider::new(OpenAiCompatConfig {
            mantle: Some(MantleAuth {
                region: "us-west-2".into(),
                creds: resolved,
            }),
            ..OpenAiCompatConfig::new("mantle-compat", base_url, "")
        })
    }

    fn bearer_creds() -> BedrockCreds {
        BedrockCreds::BearerKey {
            key: "mantle-bearer-key".into(),
        }
    }

    fn sigv4_creds() -> BedrockCreds {
        BedrockCreds::Static {
            access_key: "AKIAmantlewire000000".into(),
            secret_key: "mantle-wire-secret-key".into(),
            session_token: None,
        }
    }

    fn ok_completion() -> serde_json::Value {
        json!({
            "id": "chatcmpl-mantle",
            "model": "anthropic.claude-haiku-4-5",
            "created": 1700000000_i64,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    }

    /// complete() on the mantle lane with bearer creds signs the request as
    /// `Authorization: Bearer <mantle-key>` and never attaches a stray
    /// first-party Bearer (the empty `api_key` is not stamped).
    #[tokio::test]
    async fn complete_bearer_lane_is_signed_with_no_first_party_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let auth = received[0]
            .headers
            .get("authorization")
            .expect("mantle lane must attach Authorization")
            .to_str()
            .unwrap();
        assert_eq!(
            auth, "Bearer mantle-bearer-key",
            "bearer creds must sign as the mantle key, never an empty first-party Bearer"
        );
        // The signed body is real JSON bytes (SigV4 requires a hashable body).
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(body.get("model").is_some(), "signed body reached the wire");
    }

    /// stream() on the mantle lane with bearer creds is signed and carries
    /// no first-party Bearer. The request is sent (and thus signed) by the
    /// time `stream()` returns, so the wire assertion holds without draining.
    #[tokio::test]
    async fn stream_bearer_lane_is_signed_with_no_first_party_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let _stream = provider
            .stream(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer mantle-bearer-key"),
            "mantle stream() must be signed with the mantle key"
        );
    }

    /// complete() on the SigV4 lane signs with an `AWS4-HMAC-SHA256`
    /// Authorization scoped to `.../us-west-2/bedrock-mantle/aws4_request`,
    /// stamps `x-amz-date`, and carries the bare model id on the wire.
    #[tokio::test]
    async fn sigv4_lane_signs_wire_with_mantle_service_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), sigv4_creds()).await;
        provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let auth = received[0]
            .headers
            .get("authorization")
            .expect("SigV4 lane must attach Authorization")
            .to_str()
            .unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 "),
            "SigV4 lane must sign with AWS4-HMAC-SHA256; got {auth}"
        );
        assert!(
            auth.contains("/us-west-2/bedrock-mantle/aws4_request"),
            "credential scope must name the mantle service under the lane region; got {auth}"
        );
        assert!(
            received[0].headers.get("x-amz-date").is_some(),
            "SigV4 lane must stamp x-amz-date"
        );
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("anthropic.claude-haiku-4-5"),
            "the bare model id must reach the wire body verbatim"
        );
    }

    /// The mantle lane uses a no-redirect client: a 3xx is surfaced as an
    /// upstream failure and NEVER followed to its `Location` target.
    #[tokio::test]
    async fn mantle_lane_does_not_follow_redirects() {
        let server = MockServer::start().await;
        let redirect_target = format!("{}/redirected", server.uri());
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", redirect_target.as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();
        // A 302 is surfaced as a mapped upstream server fault (not the
        // bare redirect status), and not chased cross-host.
        match &err {
            Error::Upstream { status, .. } => assert_eq!(*status, 502),
            other => panic!("expected Upstream, got: {other:?}"),
        }
        assert_eq!(
            routectl_core::failure_class::classify(&err, Some("openai-compat")).class,
            routectl_core::failure_class::FailureClass::ServerError,
            "an unfollowed redirect must classify like a server fault"
        );

        let received = server.received_requests().await.unwrap();
        let followed = received
            .iter()
            .filter(|r| r.url.path() == "/redirected")
            .count();
        assert_eq!(
            followed, 0,
            "no-redirect client must not follow the 302 to its Location target"
        );
    }

    /// count_tokens on a mantle-configured compat provider stays the
    /// deterministic trait-default 501 (`Error::NotImplemented`): the router
    /// never walks compat for token counting, so the lane must not dial the
    /// signed endpoint for it.
    #[tokio::test]
    async fn count_tokens_is_not_implemented_on_mantle_lane() {
        let provider = mantle_provider("https://unused.invalid", bearer_creds()).await;
        let err = provider
            .count_tokens(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::NotImplemented(_, _)),
            "count_tokens on the mantle compat lane must stay a deterministic 501; got {err:?}"
        );
    }

    /// A bearer mantle credential is a static secret, so the credential
    /// probe reports `Reachable` without dialing the inference host.
    #[tokio::test]
    async fn probe_resolves_credential_without_dialing() {
        let provider = mantle_provider("https://unused.invalid", bearer_creds()).await;
        assert!(
            matches!(
                provider.probe().await,
                routectl_core::ProbeOutcome::Reachable
            ),
            "a bearer mantle credential must probe Reachable without a network dial"
        );
    }

    /// End-to-end AWS 403 on the wire: the ARN-laden AccessDenied body lifts
    /// the AWS exception token, scrubs the client body to the IAM action only
    /// (no principal ARN / account id), and classifies as `FailureClass::Auth`.
    /// The reader units live in `openai_compat/mod.rs`; this pins the full
    /// runtime lane against a mock upstream.
    #[tokio::test]
    async fn aws_403_lifts_token_scrubs_body_and_classifies_auth() {
        let server = MockServer::start().await;
        let body = r#"{"__type":"com.amazonaws.bedrock#AccessDeniedException","message":"User: arn:aws:iam::123456789012:role/App is not authorized to perform: bedrock-runtime:InvokeModel on resource: arn:aws:bedrock:us-west-2::foundation-model/anthropic.claude-haiku-4-5"}"#;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(403).set_body_string(body))
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();

        match &err {
            Error::Upstream {
                status,
                upstream_type,
                body,
                ..
            } => {
                assert_eq!(*status, 403);
                assert_eq!(upstream_type.as_deref(), Some("AccessDeniedException"));
                assert!(!body.contains("arn:aws:"), "client body leaked ARN: {body}");
                assert!(
                    !body.contains("123456789012"),
                    "client body leaked account id: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
        assert_eq!(
            routectl_core::failure_class::classify(&err, Some("openai-compat")).class,
            routectl_core::failure_class::FailureClass::Auth,
            "a mantle 403 must classify as Auth"
        );
    }

    /// End-to-end AWS 429 on the wire: the `Retry-After` reset hint is
    /// preserved on the canonical error and the bare AWS throttling `code`
    /// token is lifted.
    #[tokio::test]
    async fn aws_429_preserves_retry_after_and_lifts_code() {
        let server = MockServer::start().await;
        let body = r#"{"code":"ThrottlingException","Message":"Too many requests"}"#;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let err = provider
            .complete(user_request("anthropic.claude-haiku-4-5"))
            .await
            .unwrap_err();

        match err {
            Error::Upstream {
                status,
                retry_after,
                upstream_code,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(
                    retry_after,
                    Some(std::time::Duration::from_secs(30)),
                    "the Retry-After reset hint must be preserved"
                );
                assert_eq!(upstream_code.as_deref(), Some("ThrottlingException"));
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Forwarded media URLs: Mantle fetches `image_url.url` and other media
    // URLs server-side under the operator's AWS identity, so only https and
    // data URLs may leave on this lane.
    // -----------------------------------------------------------------------

    use routectl_core::{ChatRequest, ContentPart, KnownContentPart};

    const PLANTED_HOST: &str = "planted-bucket-zq7";
    const PLANTED_PATH: &str = "secret-key-path-k3";

    fn planted(scheme: &str) -> String {
        format!("{scheme}://{PLANTED_HOST}/{PLANTED_PATH}.png")
    }

    fn message(role: Role, content: MessageContent) -> Message {
        Message {
            refusal: None,
            role,
            content,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }
    }

    fn text_part(text: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::Text {
            text: text.into(),
            citations: None,
            cache_control: None,
            extras: serde_json::Map::new(),
        })
    }

    fn chat(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: messages.into(),
            ..Default::default()
        }
    }

    fn user_parts_req(part: ContentPart) -> ChatRequest {
        chat(vec![message(
            Role::User,
            MessageContent::Parts(vec![text_part("look"), part]),
        )])
    }

    fn image_url_part(url: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::ImageUrl {
            image_url: json!({"url": url}),
            cache_control: None,
            extras: serde_json::Map::new(),
        })
    }

    fn native_image(url: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::Image {
            source: json!({"type": "url", "url": url}),
            cache_control: None,
            extras: serde_json::Map::new(),
        })
    }

    fn file_url_part(url: &str) -> ContentPart {
        ContentPart::Known(KnownContentPart::File {
            file: json!({"file_url": url}),
            cache_control: None,
            extras: serde_json::Map::new(),
        })
    }

    /// An unmodeled block the content lift forwards verbatim, carrying an
    /// Anthropic url `source`.
    fn verbatim_url_source_block(url: &str) -> ContentPart {
        let mut extras = serde_json::Map::new();
        extras.insert("source".into(), json!({"type": "url", "url": url}));
        ContentPart::Other {
            type_tag: "image_reference".into(),
            cache_control: None,
            extras,
        }
    }

    /// A user turn whose Anthropic `tool_result` carries a url image; the
    /// lift hoists that image into a user message after the tool message.
    fn tool_result_image_req(url: &str) -> ChatRequest {
        let tool_use = ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_1".into(),
            name: "screenshot".into(),
            input: json!({}),
            cache_control: None,
            extras: serde_json::Map::new(),
        });
        let tool_result = ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: json!([{"type": "image", "source": {"type": "url", "url": url}}]),
            is_error: None,
            cache_control: None,
            extras: serde_json::Map::new(),
        });
        chat(vec![
            message(Role::User, MessageContent::Text("take one".into())),
            message(Role::Assistant, MessageContent::Parts(vec![tool_use])),
            message(Role::User, MessageContent::Parts(vec![tool_result])),
        ])
    }

    /// The rows Mantle must refuse, with the field and position the refusal
    /// names.
    fn refused_rows() -> Vec<(&'static str, ChatRequest, &'static str)> {
        vec![
            (
                "user image_url s3",
                user_parts_req(image_url_part(&planted("s3"))),
                "`image_url` at messages[0].content[1] ",
            ),
            (
                "tool result image s3",
                tool_result_image_req(&planted("s3")),
                "`image_url` at messages[3].content[0] ",
            ),
            (
                "translated anthropic image gs",
                user_parts_req(native_image(&planted("gs"))),
                "`image_url` at messages[0].content[1] ",
            ),
            (
                "verbatim url source gs",
                user_parts_req(verbatim_url_source_block(&planted("gs"))),
                "`source` at messages[0].content[1] ",
            ),
            (
                "file part file_url s3",
                user_parts_req(file_url_part(&planted("s3"))),
                "`file_url` at messages[0].content[1].file ",
            ),
            (
                "empty image_url",
                user_parts_req(image_url_part("")),
                "`image_url` at messages[0].content[1] ",
            ),
        ]
    }

    async fn completion_upstream() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion()))
            .mount(&server)
            .await;
        server
    }

    fn assert_media_refusal<T: std::fmt::Debug>(
        result: Result<T, Error>,
        row: &str,
        field_at: &str,
    ) {
        match result {
            Err(Error::NormalizeRequest(_, msg, routectl_core::RefusalReason::Unrepresentable)) => {
                assert!(msg.contains(field_at), "{row}: names the field: {msg}");
                assert!(
                    msg.contains("https or data"),
                    "{row}: names the class: {msg}"
                );
                assert!(
                    !msg.contains(PLANTED_HOST) && !msg.contains(PLANTED_PATH),
                    "{row}: refusal must not echo the URL: {msg}"
                );
            }
            other => panic!("{row}: expected a local unrepresentable refusal, got {other:?}"),
        }
    }

    /// Every media URL Mantle would fetch is refused locally on complete()
    /// and stream() unless it is https or data, whether the client sent it
    /// in OpenAI shape, the lift built it from an Anthropic block or a tool
    /// result, or an unmodeled block carries it verbatim; the upstream is
    /// never called.
    #[tokio::test]
    async fn mantle_lane_refuses_unforwardable_media_urls() {
        for (row, req, field_at) in refused_rows() {
            let server = completion_upstream().await;
            let provider = mantle_provider(&server.uri(), bearer_creds()).await;

            let complete = provider.complete(req.clone()).await;
            let stream = provider.stream(req).await.map(|_| ());

            assert_media_refusal(complete, &format!("{row} (complete)"), field_at);
            assert_media_refusal(stream, &format!("{row} (stream)"), field_at);
            let received = server.received_requests().await.unwrap();
            assert!(received.is_empty(), "{row}: no upstream call");
        }
    }

    /// https and data media URLs reach the Mantle upstream.
    #[tokio::test]
    async fn mantle_lane_forwards_https_and_data_media_urls() {
        let rows = [
            (
                "user image_url https",
                user_parts_req(image_url_part("https://example.com/a.png")),
            ),
            (
                "user image_url data",
                user_parts_req(image_url_part("data:image/png;base64,AAAA")),
            ),
            (
                "tool result image https",
                tool_result_image_req("https://example.com/b.png"),
            ),
            (
                "verbatim url source https",
                user_parts_req(verbatim_url_source_block("https://example.com/c.png")),
            ),
        ];

        for (row, req) in rows {
            let server = completion_upstream().await;
            let provider = mantle_provider(&server.uri(), bearer_creds()).await;

            provider
                .complete(req)
                .await
                .unwrap_or_else(|e| panic!("{row}: must reach the upstream, got {e:?}"));

            let received = server.received_requests().await.unwrap();
            assert_eq!(received.len(), 1, "{row}");
        }
    }

    /// Off Mantle the check does not run: every row Mantle refuses reaches a
    /// first-party openai-compat upstream with its URL intact.
    #[tokio::test]
    async fn first_party_lane_forwards_media_urls_unchanged() {
        for (row, req, _) in refused_rows() {
            let server = completion_upstream().await;
            let provider = make_provider(&server.uri(), ReasoningDialect::OpenAi);

            provider
                .complete(req.clone())
                .await
                .unwrap_or_else(|e| panic!("{row}: first-party lane must forward, got {e:?}"));

            let received = server.received_requests().await.unwrap();
            assert_eq!(received.len(), 1, "{row}");
            let body = String::from_utf8_lossy(&received[0].body);
            let carried = req
                .messages
                .iter()
                .any(|m| serde_json::to_string(m).is_ok_and(|s| s.contains(PLANTED_HOST)));
            assert_eq!(body.contains(PLANTED_HOST), carried, "{row}: {body}");
        }
    }

    /// Tool-call arguments are not media: an OpenAI `tool_calls` entry and an
    /// Anthropic `tool_use` input naming an s3 URL in a media-shaped member
    /// reach the Mantle upstream verbatim.
    #[tokio::test]
    async fn mantle_lane_does_not_inspect_tool_call_arguments() {
        let server = completion_upstream().await;
        let provider = mantle_provider(&server.uri(), bearer_creds()).await;
        let arguments = json!({"image_url": planted("s3")}).to_string();
        let mut openai_call = message(Role::Assistant, MessageContent::Text(String::new()));
        openai_call.tool_calls = Some(vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "fetch", "arguments": arguments}
        })]);
        let mut openai_result = message(Role::Tool, MessageContent::Text("done".into()));
        openai_result.tool_call_id = Some("call_1".into());
        let tool_use = ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_2".into(),
            name: "fetch".into(),
            input: json!({"image_url": planted("s3")}),
            cache_control: None,
            extras: serde_json::Map::new(),
        });
        let tool_result = ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_2".into(),
            content: json!("done"),
            is_error: None,
            cache_control: None,
            extras: serde_json::Map::new(),
        });
        let req = chat(vec![
            message(Role::User, MessageContent::Text("fetch it".into())),
            openai_call,
            openai_result,
            message(Role::Assistant, MessageContent::Parts(vec![tool_use])),
            message(Role::User, MessageContent::Parts(vec![tool_result])),
        ]);

        provider
            .complete(req)
            .await
            .expect("tool-call arguments naming s3 are not media URLs");

        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        let shipped: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["tool_calls"][0]["function"]["arguments"].as_str())
            .collect();
        assert_eq!(shipped.len(), 2, "{body}");
        assert!(shipped.iter().all(|a| a.contains(&planted("s3"))), "{body}");
    }
}
