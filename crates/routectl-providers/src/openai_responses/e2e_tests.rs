//! End-to-end tests (wiremock-driven complete + stream paths).

use super::*;
use futures::StreamExt;
use routectl_core::{ChatRequest, MessageContent, ProbeOutcome};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// NOTE: tracing-test's `#[traced_test]` installs a GLOBAL default
// subscriber; a future test in this crate that calls
// `set_global_default` (instead of the thread-local `with_default`)
// would pre-empt these `logs_contain` / `logs_assert` checks into
// false-passes. Keep new log-asserting tests on `#[traced_test]`.
use tracing_test::traced_test;

fn make_provider(base_url: &str) -> OpenAiResponsesProvider {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:test".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
    };
    OpenAiResponsesProvider::new(cfg)
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[tokio::test]
async fn complete_post_returns_chat_response() {
    // Arrange: complete() forces stream=true and drains SSE until
    // `response.completed`. The mock must return a proper SSE stream
    // with that terminal event (not a plain JSON body).
    let server = MockServer::start().await;
    let completed_body = serde_json::json!({
        "id": "resp_01",
        "object": "response",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    // Wrap in a `response.completed` SSE event (the only one we need).
    let event_body = format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let resp = provider.complete(base_req()).await.expect("complete");

    // Assert
    assert_eq!(resp.id, "resp_01");
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "pong"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        resp.routectl_provider.as_deref(),
        Some("openai-responses:test")
    );
}

/// Pin: when the model hits `max_output_tokens` the Responses API
/// emits `response.incomplete` (status "incomplete",
/// incomplete_details.reason "max_output_tokens") as the terminal
/// SSE event. `complete()` must treat it as a successful
/// truncated completion -- return Ok(ChatResponse) with
/// finish_reason="length" and usage populated -- NOT an
/// "stream ended without a terminal event" error. Mirrors the
/// streaming `stream()` path (handle_incomplete -> handle_completed).
#[tokio::test]
async fn complete_response_incomplete_returns_length_finish_reason() {
    // Arrange
    let server = MockServer::start().await;
    let incomplete_body = serde_json::json!({
        "id": "resp_inc",
        "object": "response",
        "status": "incomplete",
        "model": "gpt-5-codex",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "partial"}]
        }],
        "usage": {"input_tokens": 5, "output_tokens": 64, "total_tokens": 69}
    });
    let event_body = format!(
        "data: {{\"type\":\"response.incomplete\",\"response\":{}}}\n\n",
        serde_json::to_string(&incomplete_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let resp = provider
        .complete(base_req())
        .await
        .expect("incomplete must yield Ok, not a terminal-event error");

    // Assert: truncation maps to finish_reason="length" and usage
    // survives.
    assert_eq!(resp.id, "resp_inc");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "partial"),
        other => panic!("expected Text, got {other:?}"),
    }
    let usage = resp.usage.expect("usage present on incomplete response");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 64);
}

/// Pin: `response.incomplete` whose terminal body carries an empty
/// `output` array backfills from accumulated `output_item.done`
/// events, same as the `response.completed` path -- so a truncated
/// streamed turn still surfaces its content.
#[tokio::test]
async fn complete_incomplete_backfills_output_from_item_done_events() {
    // Arrange
    let server = MockServer::start().await;
    let item_done = serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "streamed"}]
        }
    });
    // Terminal incomplete event with an EMPTY output array (the
    // chatgpt-oauth backend pattern).
    let incomplete_body = serde_json::json!({
        "id": "resp_inc2",
        "object": "response",
        "status": "incomplete",
        "model": "gpt-5-codex",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [],
        "usage": {"input_tokens": 3, "output_tokens": 32, "total_tokens": 35}
    });
    let event_body = format!(
        "data: {}\n\ndata: {{\"type\":\"response.incomplete\",\"response\":{}}}\n\n",
        serde_json::to_string(&item_done).unwrap(),
        serde_json::to_string(&incomplete_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let resp = provider.complete(base_req()).await.expect("complete");

    // Assert: the backfilled item content surfaces; finish is length.
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "streamed"),
        other => panic!("expected backfilled Text, got {other:?}"),
    }
}

/// Pin: `complete()` caps the `output_item.done` accumulator at
/// `sse::MAX_OUTPUT_BLOCKS`, mirroring the stream path's bounded-
/// growth guard. An upstream that ships more done-items than the cap
/// (adversarial or extreme) must NOT error -- the call truncates the
/// overflow with a debug log and returns Ok, so large-but-legit
/// responses below the cap still surface.
#[traced_test]
#[tokio::test]
async fn complete_caps_accumulated_output_items_and_logs() {
    // Arrange: build an SSE body programmatically with one more
    // done-item than the cap, followed by a terminal response.completed
    // carrying an empty output array (the chatgpt-oauth backfill
    // pattern). The accumulator must stop at MAX_OUTPUT_BLOCKS.
    let server = MockServer::start().await;
    let overflow = super::sse::MAX_OUTPUT_BLOCKS + 3;
    let mut sse = String::new();
    for i in 0..overflow {
        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "id": format!("msg_{i}"),
                "role": "assistant",
                "content": [{"type": "output_text", "text": format!("t{i}")}]
            }
        });
        sse.push_str(&format!(
            "data: {}\n\n",
            serde_json::to_string(&item_done).unwrap()
        ));
    }
    let completed_body = serde_json::json!({
        "id": "resp_cap",
        "object": "response",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    sse.push_str(&format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed_body).unwrap()
    ));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act: overflow must truncate, not error.
    let resp = provider
        .complete(base_req())
        .await
        .expect("overflow must yield Ok, not an error");

    // Assert: finishes normally and the cap debug log fired.
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    assert!(
        logs_contain("output_item.done beyond cap"),
        "the accumulator cap debug log must fire on overflow"
    );
    // Direct boundary check: the cap log must fire EXACTLY
    // overflow - MAX_OUTPUT_BLOCKS (== 3) times -- one per skipped
    // item past the cap. This pins the `>=` guard at exactly
    // MAX_OUTPUT_BLOCKS: flipping it to `>` keeps one extra item, so
    // only 2 items overflow and the count drops to 2 (RED).
    let expected_skips = overflow - super::sse::MAX_OUTPUT_BLOCKS;
    logs_assert(|lines: &[&str]| {
        let skips = lines
            .iter()
            .filter(|l| l.contains("output_item.done beyond cap"))
            .count();
        if skips == expected_skips {
            Ok(())
        } else {
            Err(format!(
                "cap log fired {skips} times; expected exactly {expected_skips} \
                 (accumulator must cap at exactly MAX_OUTPUT_BLOCKS)"
            ))
        }
    });
}

/// Pin: when the SSE stream's terminal event is `response.failed`,
/// `complete()` must return `Err::Upstream` with the body's
/// `error.message` -- NOT a 200 ChatResponse with finish_reason="error".
#[tokio::test]
async fn complete_response_failed_returns_upstream_error() {
    let server = MockServer::start().await;
    let failed_body = serde_json::json!({
        "id": "resp_failed",
        "object": "response",
        "status": "failed",
        "model": "gpt-5-codex",
        "error": {"code": "rate_limited", "message": "rate limit exceeded"},
        "output": []
    });
    let event_body = format!(
        "data: {{\"type\":\"response.failed\",\"response\":{}}}\n\n",
        serde_json::to_string(&failed_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let err = provider.complete(base_req()).await.unwrap_err();
    match err {
        Error::Upstream { body, .. } => {
            assert!(
                body.contains("rate limit exceeded"),
                "expected error.message, got body: {body}"
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// Pin: `response.cancelled` also surfaces as Err::Upstream so
/// callers can distinguish from a clean completion (and route
/// retries appropriately).
#[tokio::test]
async fn complete_response_cancelled_returns_upstream_error() {
    let server = MockServer::start().await;
    let cancelled_body = serde_json::json!({
        "id": "resp_cancelled",
        "object": "response",
        "status": "cancelled",
        "model": "gpt-5-codex",
        "output": []
    });
    let event_body = format!(
        "data: {{\"type\":\"response.cancelled\",\"response\":{}}}\n\n",
        serde_json::to_string(&cancelled_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let err = provider.complete(base_req()).await.unwrap_err();
    match err {
        Error::Upstream { .. } => {}
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_non_2xx_returns_upstream_error_with_body_excerpt() {
    // Arrange
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("{\"error\":{\"message\":\"oops\"}}"),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let err = provider
        .complete(base_req())
        .await
        .expect_err("expected upstream err");

    // Assert
    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 500);
            assert!(body.contains("oops"), "body: {body}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// An over-cap error body must never reach the client: the original
/// upstream status and the header `Retry-After` are preserved, the
/// client sees only the fixed cap-exceeded message (no raw echo), and
/// exactly one cap-trip WARN fires carrying `path="error_body"`.
#[tokio::test]
async fn error_body_over_cap_preserves_status_and_hides_body() {
    // A body one byte over the production cap with an honest
    // Content-Length: the fast-reject guard trips before any body byte
    // is buffered. `b'Z'` is the sentinel that must not survive into
    // the client-facing message.
    let oversized = vec![b'Z'; crate::http_client::MAX_RESPONSE_BODY_BYTES + 1];
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_bytes(oversized),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (result, events) = routectl_testkit::with_capture(provider.complete(base_req())).await;
    let err = result.expect_err("expected upstream err");

    match err {
        Error::Upstream {
            status,
            retry_after,
            body,
            ..
        } => {
            assert_eq!(status, 429, "original upstream status must be preserved");
            assert_eq!(
                retry_after,
                Some(std::time::Duration::from_secs(30)),
                "header Retry-After must survive the cap trip"
            );
            let expected = format!(
                "response body exceeded {}-byte cap",
                crate::http_client::MAX_RESPONSE_BODY_BYTES
            );
            assert_eq!(body, expected, "client must see only the cap message");
            assert!(
                !body.contains('Z'),
                "capped body must not echo raw upstream bytes: {body}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }

    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("path") == Some("error_body"))
        .collect();
    assert_eq!(
        cap_warns.len(),
        1,
        "exactly one cap-trip WARN per error body"
    );
    let w = cap_warns[0];
    assert_eq!(w.field("status"), Some("429"));
    assert_eq!(w.field("body_truncated"), Some("true"));
    assert_eq!(
        w.field("body_cap_bytes"),
        Some(
            crate::http_client::MAX_RESPONSE_BODY_BYTES
                .to_string()
                .as_str()
        )
    );
}

/// A normal (under-cap) error body is unregressed: the upstream
/// `error.message` still reaches the client and NO cap-trip WARN fires.
#[tokio::test]
async fn error_body_under_cap_unregressed_no_cap_warn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":{"message":"bad request detail"}}"#),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    let (result, events) = routectl_testkit::with_capture(provider.complete(base_req())).await;
    let err = result.expect_err("expected upstream err");

    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(
                body.contains("bad request detail"),
                "under-cap message must reach client: {body}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
    assert!(
        events.iter().all(|e| e.field("path") != Some("error_body")),
        "no cap-trip WARN may fire under cap",
    );
}

#[tokio::test]
async fn stream_yields_error_on_truncated_sse() {
    // Arrange: a wiremock body that opens an SSE event but never
    // terminates it (no final `\n\n` framing, no `[DONE]`). The
    // stream loop should either yield a Streaming Err or simply
    // exhaust without panicking; what it MUST NOT do is loop
    // forever or unwrap a partial event.
    let server = MockServer::start().await;
    // Open `data: ` but no terminating blank line + no JSON body.
    // The eventsource decoder will treat this as a parse error or
    // as no event emitted; in both cases the stream must terminate
    // cleanly without panicking.
    let truncated = "data: {\"type\":\"response.created\",\"resp";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(truncated)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let mut s = provider.stream(base_req()).await.expect("stream");
    let mut chunks: Vec<Result<ChatChunk, Error>> = Vec::new();
    while let Some(item) = s.next().await {
        chunks.push(item);
        // Bound the loop defensively so a regression doesn't hang
        // the test forever.
        if chunks.len() >= 16 {
            break;
        }
    }

    // Assert: stream terminated (didn't panic) and no chunks
    // beyond what could be parsed (an Err is acceptable too).
    let oks = chunks.iter().filter(|r| r.is_ok()).count();
    let errs = chunks.iter().filter(|r| r.is_err()).count();
    // Either we got 0 successful chunks + an Err, or we got
    // nothing at all (parser ate the partial line). Both are
    // acceptable; what we're guarding against is panic / hang.
    assert!(
        errs >= 1 || (oks == 0 && errs == 0),
        "expected truncated stream to yield either an Err or empty; got {oks} oks + {errs} errs"
    );
}

#[tokio::test]
async fn stream_yields_chat_chunks_for_full_session() {
    // Arrange
    let server = MockServer::start().await;
    // Construct an SSE body with `data: <json>\n\n` framing.
    let events = [
        serde_json::json!({"type": "response.created", "response": {"id":"r","model":"m"}}),
        serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "id":"m1", "role":"assistant", "content":[]}
        }),
        serde_json::json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id":"r", "status":"completed", "model":"m",
                "output":[{"type":"message","id":"m1","role":"assistant",
                            "content":[{"type":"output_text","text":"hi"}]}],
                "usage": {"input_tokens":1, "output_tokens":1, "total_tokens":2}
            }
        }),
    ];
    let sse_body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = make_provider(&server.uri());

    // Act
    let mut s = provider.stream(base_req()).await.expect("stream");
    let mut chunks: Vec<ChatChunk> = Vec::new();
    while let Some(item) = s.next().await {
        chunks.push(item.expect("chunk ok"));
    }

    // Assert: created (role) + text delta + final = 3 chunks.
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("hi"));
    let final_c = chunks.last().unwrap();
    assert_eq!(final_c.choices[0].finish_reason.as_deref(), Some("stop"));
}

// -----------------------------------------------------------------------
// probe(): free reachability against /models (ApiKey lane only)
// -----------------------------------------------------------------------

/// A `TokenSource` that counts `token()` calls so the oauth-guard test
/// can prove the probe never resolves (never refreshes) a credential.
#[derive(Default)]
struct CountingTokenSource {
    token_calls: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for CountingTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingTokenSource").finish()
    }
}

#[async_trait]
impl TokenSource for CountingTokenSource {
    async fn token(&self) -> Result<String> {
        self.token_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("api-key".into())
    }
}

fn api_key_provider(base_url: &str) -> OpenAiResponsesProvider {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:probe".into(),
        auth: Arc::new(StaticToken::new("test-key")) as Arc<dyn TokenSource>,
        account_id: None,
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
    };
    OpenAiResponsesProvider::new(cfg)
}

#[tokio::test]
async fn probe_api_key_200_models_list_is_reachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] })))
        .expect(1) // AT MOST ONE upstream request: no retry.
        .mount(&server)
        .await;

    let provider = api_key_provider(&server.uri());
    assert_eq!(provider.probe().await, ProbeOutcome::Reachable);
}

#[tokio::test]
async fn probe_api_key_401_is_auth_failed_without_leaking_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = api_key_provider(&server.uri());
    match provider.probe().await {
        ProbeOutcome::AuthFailed(reason) => {
            assert!(!reason.contains("test-key"), "reason leaked the api key");
            assert!(!reason.contains(&server.uri()), "reason leaked the url");
        }
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_api_key_403_is_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let provider = api_key_provider(&server.uri());
    assert!(matches!(
        provider.probe().await,
        ProbeOutcome::AuthFailed(_)
    ));
}

#[tokio::test]
async fn probe_api_key_connection_refused_is_unreachable() {
    // A closed loopback port (nothing binds 127.0.0.1:1)
    // deterministically refuses the connect.
    let provider = api_key_provider("http://127.0.0.1:1");
    assert!(matches!(
        provider.probe().await,
        ProbeOutcome::Unreachable(_)
    ));
}

/// BINDING read-only guard: a ChatgptOauth provider reports
/// `UnsupportedFreeProbe` and makes ZERO token-source calls -- the
/// token path is never touched, and no upstream request is issued.
#[tokio::test]
async fn probe_chatgpt_oauth_is_unsupported_with_zero_token_calls() {
    let source = Arc::new(CountingTokenSource::default());
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:oauth-probe".into(),
        auth: source.clone(),
        account_id: Some("acct-uuid".into()),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
    };
    let provider = OpenAiResponsesProvider::new(cfg);

    assert_eq!(provider.probe().await, ProbeOutcome::UnsupportedFreeProbe);
    assert_eq!(
        source.token_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the oauth probe guard must never resolve a token",
    );
}
