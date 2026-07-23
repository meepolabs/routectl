//! wiremock integration tests for the complete and stream paths.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn integration_complete() {
    let mock_server = MockServer::start().await;

    let response_body = json!({
        "id": "msg_int01",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20},
        "content": [{"type": "text", "text": "Integration test response."}]
    });

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("Hi from integration test")]);

    let resp = provider.complete(req).await.unwrap();
    assert_eq!(resp.id, "msg_int01");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "Integration test response."),
        other => panic!("expected Text content, got {other:?}"),
    }
    assert_eq!(resp.routectl_provider.as_deref(), Some("test-anthropic"));
}

#[tokio::test]
async fn integration_complete_upstream_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "invalid api key"}
        })))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    let err = provider.complete(req).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("401") || msg.contains("invalid api key"),
        "unexpected: {msg}"
    );
}

/// A body larger than the 16 MiB non-stream cap. Honest wiremock
/// Content-Length lets the provider's fast-reject fire without the
/// socket ever streaming the body.
fn over_cap_body() -> String {
    "a".repeat(16 * 1024 * 1024 + 1)
}

#[tokio::test]
async fn integration_complete_success_body_over_cap_maps_to_502() {
    // An unreadable 2xx body is an invalid upstream protocol result: it
    // must classify as a 502 ServerError (not leak the original 200 nor
    // echo any bytes), so the breaker/retry machinery treats it like any
    // other upstream failure.
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(over_cap_body()))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    let err = provider.complete(req).await.unwrap_err();
    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 502, "over-cap 2xx must map to 502, got {status}");
            assert!(
                body.contains("cap"),
                "message must be the bounded cap-exceeded text: {body:?}"
            );
            assert!(
                !body.contains("aaaa"),
                "cap message must not echo the raw body: {body:?}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn integration_complete_error_body_over_cap_preserves_status_bounded() {
    // An over-cap >=400 body preserves the original upstream status while
    // collapsing the (truncated, untrustworthy) body to the fixed
    // cap-exceeded message -- no raw echo.
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string(over_cap_body()))
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hi")]);

    let err = provider.complete(req).await.unwrap_err();
    match err {
        routectl_core::Error::Upstream { status, body, .. } => {
            assert_eq!(status, 429, "original upstream status must be preserved");
            assert!(
                body.contains("cap"),
                "message must be the bounded cap-exceeded text: {body:?}"
            );
            assert!(
                !body.contains("aaaa"),
                "capped error body must not be echoed: {body:?}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn integration_stream() {
    let mock_server = MockServer::start().await;

    // Build a minimal valid SSE body.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_st01\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
    req.stream = Some(true);

    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut text_chunks: Vec<String> = Vec::new();
    let mut finish_reasons: Vec<String> = Vec::new();

    while let Some(result) = stream.next().await {
        let chunk = result.unwrap();
        for choice in &chunk.choices {
            if let Some(ref text) = choice.delta.content {
                text_chunks.push(text.clone());
            }
            if let Some(ref fr) = choice.finish_reason {
                finish_reasons.push(fr.clone());
            }
        }
    }

    assert!(
        text_chunks.contains(&"Hi!".to_string()),
        "expected 'Hi!' in {text_chunks:?}"
    );
    assert!(
        finish_reasons.contains(&"stop".to_string()),
        "expected 'stop' in {finish_reasons:?}"
    );
}

/// OpenRouter's `/v1/messages` endpoint appends an OpenAI-style
/// `data: [DONE]` sentinel after the Anthropic `message_stop`
/// event. Real api.anthropic.com does not emit this. Pre-fix
/// (Bug G), the SSE parser would try to JSON-decode `[DONE]`
/// and fail with `bad sse json: expected value at line 1
/// column 2`, yielding an `Err(Streaming(..))` chunk and
/// causing the egress wrapper to synthesize
/// `finish_reason="truncated"`. Pin that the stream now ends
/// cleanly: no error yielded, observed finish_reason still
/// `"stop"`.
#[tokio::test]
async fn integration_stream_handles_trailing_done_sentinel() {
    let mock_server = MockServer::start().await;

    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_st02\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        // OpenRouter trailer -- not a valid Anthropic event.
        "event: data\n",
        "data: [DONE]\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("stream test")]);
    req.stream = Some(true);

    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut text_chunks: Vec<String> = Vec::new();
    let mut finish_reasons: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(chunk) => {
                for choice in &chunk.choices {
                    if let Some(ref text) = choice.delta.content {
                        text_chunks.push(text.clone());
                    }
                    if let Some(ref fr) = choice.finish_reason {
                        finish_reasons.push(fr.clone());
                    }
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
    }

    assert!(
        errors.is_empty(),
        "trailing [DONE] must not produce stream errors: {errors:?}"
    );
    assert!(
        text_chunks.contains(&"Hi!".to_string()),
        "expected 'Hi!' in {text_chunks:?}"
    );
    assert!(
        finish_reasons.contains(&"stop".to_string()),
        "expected 'stop' in {finish_reasons:?}"
    );
}

/// Drive an SSE stream that errors mid-flight (malformed JSON in a
/// `content_block_start` payload, after a clean `message_start`)
/// and confirm the stream surfaces an `Err` chunk to the caller.
/// Pins the contract: a parse-time mid-stream failure terminates
/// the stream with a streaming Err rather than silently swallowing
/// the event. The accompanying DEBUG log on the error path is
/// emitted for triage but not asserted here (no tracing-test
/// dev-dep on this crate for log capture).
#[tokio::test]
async fn integration_stream_yields_err_on_midstream_parse_error() {
    let mock_server = MockServer::start().await;

    // Valid message_start, then a malformed content_block_start whose
    // `data` is not valid JSON. The SSE state machine bails on the
    // parse error and yields Err(Streaming(..)) to the caller.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_err01\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {not valid json at all\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let provider = make_provider(&mock_server.uri());
    let mut req = base_req("claude-3-opus", vec![user_msg("trigger parse error")]);
    req.stream = Some(true);

    use futures::StreamExt;
    let mut stream = provider.stream(req).await.unwrap();
    let mut saw_err = false;
    while let Some(result) = stream.next().await {
        if result.is_err() {
            saw_err = true;
            // Once the error fires the stream contract is to
            // terminate; break so we don't keep polling a closed
            // generator.
            break;
        }
    }
    assert!(
        saw_err,
        "expected the malformed mid-stream event to surface as Err to the caller"
    );
}
