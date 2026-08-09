//! Tests for the Converse-stream eventstream decoder.
//!
//! Lives in a sibling file so `eventstream.rs` stays under the
//! project's 800-line ceiling. Imported via
//! `#[path = "eventstream_tests.rs"] mod tests;` from `eventstream.rs`,
//! which means `super::*` here resolves to the parent module's
//! private items (`handle_converse_frame`, `ConverseStreamState`).

use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message as AwsMessage};
use futures::StreamExt;
use futures::stream as fstream;

fn make_frame(event_type: &str, payload_json: &str) -> AwsMessage {
    AwsMessage::new(Bytes::from(payload_json.to_string().into_bytes()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".to_string().into()),
        ))
        .add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ))
}

/// A frame in the shape AWS actually uses for a modeled exception:
/// `:message-type: "exception"` with the member name in `:exception-type`
/// and NO `:event-type` header.
fn make_exception_frame(exception_type: &str, payload_json: &str) -> AwsMessage {
    AwsMessage::new(Bytes::from(payload_json.to_string().into_bytes()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("exception".to_string().into()),
        ))
        .add_header(Header::new(
            ":exception-type",
            HeaderValue::String(exception_type.to_string().into()),
        ))
}

/// Encode a single AWS eventstream frame to its on-the-wire bytes.
/// Used by the EOF-flush test below to drive the public `stream()`
/// async function with a synthetic byte stream.
fn encode_frame(event_type: &str, payload_json: &str) -> Bytes {
    let frame = make_frame(event_type, payload_json);
    let mut buf = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
        .expect("encode eventstream frame");
    Bytes::from(buf)
}

fn run(event_type: &str, payload: &str, state: &mut ConverseStreamState) -> Vec<ChatChunk> {
    handle_converse_frame("test", make_frame(event_type, payload), state).unwrap()
}

#[test]
fn bedrock_converse_stream_opens_with_role_chunk() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = run("messageStart", r#"{"role":"assistant"}"#, &mut state);

    // Assert: a single opening role chunk in first position, no content.
    assert_eq!(chunks.len(), 1);
    let delta = &chunks[0].choices[0].delta;
    assert!(matches!(delta.role, Some(Role::Assistant)));
    assert!(delta.content.is_none());
    assert!(chunks[0].usage.is_none());
    assert!(chunks[0].choices[0].finish_reason.is_none());
}

/// A malformed upstream repeating `messageStart` must not emit a second
/// role chunk -- the opening role chunk fires exactly once per stream.
#[test]
fn bedrock_converse_role_chunk_emitted_once() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let first = run("messageStart", r#"{"role":"assistant"}"#, &mut state);
    let second = run("messageStart", r#"{"role":"assistant"}"#, &mut state);

    // Assert
    assert_eq!(first.len(), 1);
    assert!(second.is_empty());
}

#[test]
fn text_block_lifecycle_yields_text_deltas() {
    // Arrange: AWS sends NO contentBlockStart for a text block
    // (contentBlockStart is tool-use only), so the sequence is deltas
    // then stop.
    let mut state = ConverseStreamState::default();

    // Act
    let c1 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"hello "}}"#,
        &mut state,
    );
    let c2 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"world"}}"#,
        &mut state,
    );
    let stop = run("contentBlockStop", r#"{"contentBlockIndex":0}"#, &mut state);

    // Assert
    assert_eq!(c1.len(), 1);
    assert_eq!(c1[0].choices[0].delta.content.as_deref(), Some("hello "));
    assert_eq!(c2.len(), 1);
    assert_eq!(c2[0].choices[0].delta.content.as_deref(), Some("world"));
    assert!(stop.is_empty());
}

/// Regression guard: on the documented AWS wire a text block arrives
/// with NO `contentBlockStart` frame (`start` is required and its union
/// has no text member). The delta must open the block itself rather
/// than be dropped.
#[test]
fn text_delta_without_content_block_start_yields_text() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: the AWS worked example's order -- messageStart then deltas.
    let _ = run("messageStart", r#"{"role":"assistant"}"#, &mut state);
    let chunks = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
}

/// Regression guard: a start-less reasoning block must survive with its
/// signature paired onto the terminal aggregated detail.
#[test]
fn reasoning_deltas_without_content_block_start_pair_text_and_signature() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run("messageStart", r#"{"role":"assistant"}"#, &mut state);
    let thinking = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"text":"step 1"}}}"#,
        &mut state,
    );
    let sig = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"signature":"sig"}}}"#,
        &mut state,
    );
    let stop = run("contentBlockStop", r#"{"contentBlockIndex":0}"#, &mut state);

    // Assert
    assert_eq!(thinking.len(), 1);
    assert_eq!(
        thinking[0].choices[0].delta.reasoning.as_deref(),
        Some("step 1")
    );
    assert!(sig.is_empty(), "signature delta is buffered, not emitted");
    assert_eq!(stop.len(), 1);
    let detail = &stop[0].choices[0].delta.reasoning_details[0];
    assert_eq!(detail.payload["text"], "step 1");
    assert_eq!(detail.payload["signature"], "sig");
    assert_eq!(detail.index, Some(0));
}

/// Regression guard: a start-less redacted-reasoning delta emits its
/// encrypted detail immediately.
#[test]
fn redacted_reasoning_delta_without_content_block_start_emits_detail() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"redactedContent":"AAECAwQF"}}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(chunks.len(), 1);
    let detail = &chunks[0].choices[0].delta.reasoning_details[0];
    assert_eq!(detail.payload["data"], "AAECAwQF");
}

/// Lazy block creation stays index-keyed: two start-less blocks at
/// distinct indices must not share state, and each reasoning block
/// gets its own detail index.
#[test]
fn concurrent_start_less_blocks_keep_independent_state_per_index() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: interleave a text block at 0 with a reasoning block at 1.
    let text = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"visible"}}"#,
        &mut state,
    );
    let think_a = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"a"}}}"#,
        &mut state,
    );
    let think_b = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":2,"delta":{"reasoningContent":{"text":"b"}}}"#,
        &mut state,
    );
    let stop1 = run("contentBlockStop", r#"{"contentBlockIndex":1}"#, &mut state);
    let stop2 = run("contentBlockStop", r#"{"contentBlockIndex":2}"#, &mut state);
    let stop0 = run("contentBlockStop", r#"{"contentBlockIndex":0}"#, &mut state);

    // Assert: text unaffected by the reasoning blocks, and each
    // reasoning block accumulated only its own delta under its own
    // detail index.
    assert_eq!(text[0].choices[0].delta.content.as_deref(), Some("visible"));
    assert_eq!(think_a.len(), 1);
    assert_eq!(think_b.len(), 1);
    let d1 = &stop1[0].choices[0].delta.reasoning_details[0];
    let d2 = &stop2[0].choices[0].delta.reasoning_details[0];
    assert_eq!(d1.payload["text"], "a");
    assert_eq!(d2.payload["text"], "b");
    assert_eq!(d1.index, Some(0));
    assert_eq!(d2.index, Some(1));
    assert!(
        stop0.is_empty(),
        "a text block's stop emits no reasoning detail"
    );
}

/// A text or reasoning delta landing on an index already opened as a
/// tool_use block is a wire violation -- still skipped, so the tool
/// accumulator cannot be corrupted.
#[test]
fn text_and_reasoning_deltas_on_a_tool_use_block_are_skipped() {
    // Arrange: a typed tool-use start (the only documented start shape).
    let mut state = ConverseStreamState::default();
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":3,
            "start":{"toolUse":{"toolUseId":"tu_9","name":"calc"}}}"#,
        &mut state,
    );

    // Act
    let text = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":3,"delta":{"text":"nope"}}"#,
        &mut state,
    );
    let reasoning = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":3,"delta":{"reasoningContent":{"text":"nope"}}}"#,
        &mut state,
    );

    // Assert
    assert!(text.is_empty(), "text delta on a tool_use block must skip");
    assert!(
        reasoning.is_empty(),
        "reasoning delta on a tool_use block must skip"
    );
}

#[test]
fn tool_use_lifecycle_emits_tool_call_deltas() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: start with tool_use payload, two arg deltas, stop.
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":1,
            "start":{"toolUse":{"toolUseId":"tu_42","name":"calc"}}}"#,
        &mut state,
    );
    let c1 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"a\":"}}}"#,
        &mut state,
    );
    let c2 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"1}"}}}"#,
        &mut state,
    );

    // Assert: each delta carries an OpenAI-shape tool_calls entry
    // with the same `index` (stable across deltas for one tool).
    assert_eq!(c1.len(), 1);
    let tcs1 = c1[0].choices[0].delta.tool_calls.as_ref().unwrap();
    assert_eq!(tcs1[0]["index"], 0);
    assert_eq!(tcs1[0]["id"], "tu_42");
    assert_eq!(tcs1[0]["function"]["name"], "calc");
    assert_eq!(tcs1[0]["function"]["arguments"], "{\"a\":");
    let tcs2 = c2[0].choices[0].delta.tool_calls.as_ref().unwrap();
    assert_eq!(tcs2[0]["index"], 0);
    assert_eq!(tcs2[0]["function"]["arguments"], "1}");
}

/// Strategy A: per-delta `reasoning` string lands live, structured
/// detail is deferred to contentBlockStop.
#[test]
fn reasoning_text_delta_emits_thinking_chunk() {
    // Arrange: no contentBlockStart -- a reasoning block has none on
    // the documented wire.
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"text":"step 1"}}}"#,
        &mut state,
    );

    // Assert: live string only on the per-delta chunk.
    assert_eq!(chunks.len(), 1);
    let delta = &chunks[0].choices[0].delta;
    assert_eq!(delta.reasoning.as_deref(), Some("step 1"));
    assert!(
        delta.reasoning_details.is_empty(),
        "live thinking chunk must not carry the structured detail (deferred to contentBlockStop)"
    );
}

/// Strategy A: signature_delta records onto state and emits no
/// chunk; the aggregated detail at contentBlockStop carries both
/// text and signature with the same detail_index.
#[test]
fn reasoning_signature_after_text_uses_same_detail_index() {
    let mut state = ConverseStreamState::default();
    let text_chunks = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"text":"thinking"}}}"#,
        &mut state,
    );
    // Signature delta is silent under Strategy A.
    let sig_chunks = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,
            "delta":{"reasoningContent":{"signature":"sig"}}}"#,
        &mut state,
    );
    assert_eq!(text_chunks.len(), 1);
    assert!(
        sig_chunks.is_empty(),
        "signature delta is buffered, not emitted directly"
    );

    // contentBlockStop emits the terminal aggregated detail.
    let stop_chunks = run("contentBlockStop", r#"{"contentBlockIndex":0}"#, &mut state);
    assert_eq!(stop_chunks.len(), 1);
    let detail = &stop_chunks[0].choices[0].delta.reasoning_details[0];
    assert_eq!(detail.payload["text"], "thinking");
    assert_eq!(detail.payload["signature"], "sig");
}

/// Streamed redacted reasoning must carry a monotonic detail
/// index drawn from the same counter the text-reasoning path uses, so
/// sort-by-index preserves wire order. A redacted block after a text-
/// reasoning block at index N>0 gets index Some(>N).
#[test]
fn redacted_reasoning_detail_carries_monotonic_index_after_text_reasoning() {
    let mut state = ConverseStreamState::default();

    // Two text-reasoning blocks so the counter advances past 0: block 0
    // -> detail_index 0, block 1 -> detail_index 1 (this is N). No
    // contentBlockStart frames -- reasoning blocks get none.
    let _ = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"a"}}}"#,
        &mut state,
    );
    let stop0 = run("contentBlockStop", r#"{"contentBlockIndex":0}"#, &mut state);

    let _ = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"b"}}}"#,
        &mut state,
    );
    let stop1 = run("contentBlockStop", r#"{"contentBlockIndex":1}"#, &mut state);

    let n = stop1[0].choices[0].delta.reasoning_details[0]
        .index
        .expect("text reasoning detail must carry an index");
    assert!(n > 0, "second text-reasoning block must land at index N>0");
    let _ = stop0; // first block establishes the counter at 0.

    // A redacted block on a third index.
    let redacted = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":2,"delta":{"reasoningContent":{"redactedContent":"AAECAwQF"}}}"#,
        &mut state,
    );

    // Assert: redacted detail carries Some(index) strictly > N.
    let redacted_detail = &redacted[0].choices[0].delta.reasoning_details[0];
    let r_index = redacted_detail
        .index
        .expect("redacted reasoning detail must carry an index");
    assert!(
        r_index > n,
        "redacted detail index {r_index} must be strictly greater than text index {n}"
    );

    // Sort-by-index preserves wire order: text (N) before redacted (>N).
    let mut indices = vec![n, r_index];
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![n, r_index],
        "sort-by-index must preserve wire order (text before redacted)"
    );
}

#[test]
fn message_stop_capture_then_metadata_emits_closing_chunk_with_finish_and_usage() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: AWS event order is messageStop -> metadata. messageStop
    // alone shouldn't yield a chunk (we hold the stop_reason).
    let mid = run("messageStop", r#"{"stopReason":"end_turn"}"#, &mut state);
    assert!(mid.is_empty());
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15},
            "metrics":{"latencyMs":42}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(closing.len(), 1);
    assert_eq!(closing[0].choices[0].finish_reason.as_deref(), Some("stop"));
    let u = closing[0].usage.as_ref().unwrap();
    assert_eq!(u.prompt_tokens, Some(10));
    assert_eq!(u.completion_tokens, Some(5));
    assert_eq!(u.total_tokens, Some(15));
}

#[test]
fn metadata_cache_details_translate_to_per_ttl_breakdown_on_closing_chunk() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: messageStop holds the stop_reason; the metadata frame
    // carries a ConverseUsage with a per-TTL cacheDetails breakdown.
    let _ = run("messageStop", r#"{"stopReason":"end_turn"}"#, &mut state);
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":0,"outputTokens":5,
            "cacheWriteInputTokens":175,
            "cacheDetails":[
                {"inputTokens":75,"ttl":"5m"},
                {"inputTokens":100,"ttl":"1h"}
            ]}}"#,
        &mut state,
    );

    // Assert: the streaming closing chunk must carry the same per-TTL
    // split the non-streaming path produces, not a flattened None.
    assert_eq!(closing.len(), 1);
    let u = closing[0].usage.as_ref().unwrap();
    assert_eq!(u.cache_creation_input_tokens, Some(175));
    let cc = u.cache_creation.as_ref().unwrap();
    assert_eq!(cc.ephemeral_5m_input_tokens, Some(75));
    assert_eq!(cc.ephemeral_1h_input_tokens, Some(100));
}

#[test]
fn metadata_unknown_ttl_bucket_dropped_from_per_ttl_split_on_closing_chunk() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: a future TTL bucket (e.g. "24h") AWS hasn't shipped yet.
    let _ = run("messageStop", r#"{"stopReason":"end_turn"}"#, &mut state);
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":0,"outputTokens":5,
            "cacheWriteInputTokens":125,
            "cacheDetails":[
                {"inputTokens":50,"ttl":"24h"},
                {"inputTokens":75,"ttl":"5m"}
            ]}}"#,
        &mut state,
    );

    // Assert: the unknown bucket is dropped from the per-TTL object
    // (not coerced into the wrong bucket) but still counts toward the
    // aggregate, matching the shared translate_cache_details contract.
    assert_eq!(closing.len(), 1);
    let u = closing[0].usage.as_ref().unwrap();
    assert_eq!(u.cache_creation_input_tokens, Some(125));
    let cc = u.cache_creation.as_ref().unwrap();
    assert_eq!(cc.ephemeral_5m_input_tokens, Some(75));
    assert_eq!(cc.ephemeral_1h_input_tokens, None);
}

#[test]
fn unknown_stop_reason_passes_through_on_closing_chunk() {
    // Arrange: Converse-only stop_reason.
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run(
        "messageStop",
        r#"{"stopReason":"guardrail_intervened"}"#,
        &mut state,
    );
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(
        closing[0].choices[0].finish_reason.as_deref(),
        Some("guardrail_intervened")
    );
}

/// AWS documents `modelStreamErrorException` with HTTP Status Code 424
/// ("A streaming error occurred. Retry your request.") in the
/// ConverseStream response elements -- NOT 500. Preserving the documented
/// status keeps the router's retry/breaker classification aligned with
/// what AWS said went wrong.
#[test]
fn model_stream_error_exception_surfaces_as_upstream_424() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_frame("modelStreamErrorException", r#"{"message":"glitch"}"#),
        &mut state,
    );

    // Assert
    let err = res.unwrap_err();
    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 424, "AWS documents this member as 424");
            assert!(body.contains("glitch"), "body: {body}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// `modelTimeoutException` is documented as 408, and arrives on the
/// protocol-shaped path (`:message-type: exception`).
#[test]
fn model_timeout_exception_surfaces_as_upstream_408() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_exception_frame("modelTimeoutException", r#"{"message":"too slow"}"#),
        &mut state,
    );

    // Assert
    let err = res.unwrap_err();
    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 408, "AWS documents this member as 408");
            assert!(body.contains("too slow"), "body: {body}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// A protocol-shaped `modelStreamErrorException` (the real wire framing)
/// must reach the same 424, not just the `:event-type` fallback path.
#[test]
fn protocol_shaped_model_stream_error_surfaces_as_upstream_424() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_exception_frame("modelStreamErrorException", r#"{"message":"glitch"}"#),
        &mut state,
    );

    // Assert
    let err = res.unwrap_err();
    match err {
        Error::Upstream { status, .. } => assert_eq!(status, 424),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn validation_exception_surfaces_as_upstream_400() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_frame("validationException", r#"{"message":"bad model"}"#),
        &mut state,
    );

    // Assert
    match res.unwrap_err() {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(body.contains("bad model"));
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn throttling_exception_surfaces_as_upstream_429() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_frame("throttlingException", r#"{"message":"slow down"}"#),
        &mut state,
    );

    // Assert
    match res.unwrap_err() {
        Error::Upstream { status, .. } => assert_eq!(status, 429),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn unknown_event_type_is_skipped_not_errored() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = run("someBrandNewConverseEvent", "{}", &mut state);

    // Assert
    assert!(chunks.is_empty());
}

/// The wire shape AWS actually sends when it throttles mid-stream:
/// `:message-type: exception` + `:exception-type: throttlingException`, no
/// `:event-type`. Classifying on `:event-type` alone made this frame
/// unrecognized, so the response truncated silently and the breaker
/// recorded the throttled seat healthy.
#[test]
fn protocol_shaped_throttling_exception_frame_surfaces_as_upstream_429() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_exception_frame("throttlingException", r#"{"message":"slow down"}"#),
        &mut state,
    );

    // Assert
    match res.unwrap_err() {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 429);
            assert!(body.contains("slow down"), "body: {body}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// A protocol-typed exception whose member name we do not know must still
/// end the stream -- failing closed as a 500 rather than being skipped.
#[test]
fn unknown_exception_type_fails_closed_as_500() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_exception_frame("someFutureAwsException", r#"{"message":"nope"}"#),
        &mut state,
    );

    // Assert
    match res.unwrap_err() {
        Error::Upstream { status, .. } => assert_eq!(status, 500),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

/// A frame naming neither `:message-type` nor `:event-type` carries nothing
/// decodable and nothing marking it a failure -- skip it.
#[test]
fn frame_with_no_type_headers_is_skipped() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = handle_converse_frame(
        "test",
        AwsMessage::new(Bytes::from_static(b"{}")),
        &mut state,
    )
    .unwrap();

    // Assert
    assert!(chunks.is_empty());
}

/// End-to-end through the real wire encoding: a mid-stream protocol-shaped
/// throttle must terminate the stream with a 429 rather than closing
/// cleanly on a truncated response.
#[tokio::test]
async fn mid_stream_exception_frame_terminates_converse_stream_with_error() {
    // Arrange
    let exception = {
        let frame = make_exception_frame("throttlingException", r#"{"message":"slow down"}"#);
        let mut buf = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
            .expect("encode eventstream frame");
        Bytes::from(buf)
    };
    let byte_stream = fstream::iter(vec![
        Ok(encode_frame("messageStart", r#"{"role":"assistant"}"#)),
        Ok(exception),
    ]);

    // Act
    let mut chunks = stream("test".to_string(), byte_stream);
    let mut last_err = None;
    while let Some(item) = chunks.next().await {
        if let Err(e) = item {
            last_err = Some(e);
        }
    }

    // Assert
    match last_err {
        Some(Error::Upstream { status, body, .. }) => {
            assert_eq!(status, 429);
            assert!(body.contains("slow down"), "body: {body}");
        }
        other => panic!("mid-stream throttle must terminate the stream with a 429; got {other:?}"),
    }
}

#[test]
fn tool_call_index_is_stable_across_two_tool_blocks() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act: two tool_use blocks at indices 1 and 2 should get
    // call_index 0 and 1 in canonical tool_calls.
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":1,
            "start":{"toolUse":{"toolUseId":"a","name":"f1"}}}"#,
        &mut state,
    );
    let c1 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"x"}}}"#,
        &mut state,
    );
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":2,
            "start":{"toolUse":{"toolUseId":"b","name":"f2"}}}"#,
        &mut state,
    );
    let c2 = run(
        "contentBlockDelta",
        r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"y"}}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(
        c1[0].choices[0].delta.tool_calls.as_ref().unwrap()[0]["index"],
        0
    );
    assert_eq!(
        c2[0].choices[0].delta.tool_calls.as_ref().unwrap()[0]["index"],
        1
    );
}

#[tokio::test]
async fn stream_eof_after_message_stop_without_metadata_emits_closing_chunk() {
    // Arrange: synthesize the full happy-path frame sequence MINUS
    // the metadata frame -- a real-world failure mode where AWS
    // middleware truncates the connection after messageStop. Pre-fix
    // the decoder held the captured stop_reason in state and never
    // flushed it, so finish_reason silently vanished from the wire
    // and clients saw a stream that just stopped.
    let frames: Vec<std::result::Result<Bytes, reqwest::Error>> = vec![
        Ok(encode_frame("messageStart", r#"{"role":"assistant"}"#)),
        Ok(encode_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"hello"}}"#,
        )),
        Ok(encode_frame(
            "contentBlockStop",
            r#"{"contentBlockIndex":0}"#,
        )),
        Ok(encode_frame("messageStop", r#"{"stopReason":"end_turn"}"#)),
        // No metadata frame -- EOF here.
    ];
    let byte_stream = fstream::iter(frames);

    // Act
    let chunks: Vec<_> = stream("test".to_string(), byte_stream)
        .collect::<Vec<_>>()
        .await;

    // Assert: the final chunk carries the captured stop_reason as
    // finish_reason and an absent usage delta (we never saw metadata).
    let last = chunks
        .last()
        .expect("expected at least one chunk")
        .as_ref()
        .expect("EOF flush yielded an Err");
    assert_eq!(last.choices[0].finish_reason.as_deref(), Some("stop"));
    assert!(
        last.usage.is_none(),
        "EOF flush should emit empty usage when metadata never arrived"
    );
    // And no error chunk was yielded -- the EOF is graceful when
    // buffer is empty + no prelude pending.
    let any_err = chunks.iter().any(std::result::Result::is_err);
    assert!(!any_err, "EOF after messageStop should not error");
}

#[test]
fn matched_stop_sequence_lifts_onto_closing_chunk_when_stop_reason_is_stop_sequence() {
    // Arrange: AWS streams messageStop with the lifted
    // additionalModelResponseFields["stop_sequence"] then metadata.
    // The closing chunk emitted on metadata must carry the matched
    // sequence onto canonical `matched_stop_sequence`, identical to
    // the non-streaming Converse path.
    let mut state = ConverseStreamState::default();

    // Act
    let mid = run(
        "messageStop",
        r#"{"stopReason":"stop_sequence","additionalModelResponseFields":{"stop_sequence":"STOP"}}"#,
        &mut state,
    );
    assert!(mid.is_empty());
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(closing.len(), 1);
    assert_eq!(closing[0].choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        closing[0].choices[0].matched_stop_sequence.as_deref(),
        Some("STOP")
    );
}

#[test]
fn matched_stop_sequence_gated_off_when_stop_reason_is_end_turn() {
    // Arrange: a stray stop_sequence value paired with a different
    // stop_reason must NOT be lifted -- mirrors the non-streaming
    // gate so the canonical layer never mis-signals
    // `matched_stop_sequence` on a normal end_turn.
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run(
        "messageStop",
        r#"{"stopReason":"end_turn","additionalModelResponseFields":{"stop_sequence":"STOP"}}"#,
        &mut state,
    );
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
        &mut state,
    );

    // Assert
    assert!(closing[0].choices[0].matched_stop_sequence.is_none());
}

#[test]
fn matched_stop_sequence_absent_field_yields_none_no_error() {
    // Arrange: stop_reason=stop_sequence but the response-field bag
    // is missing entirely (provider quirk or schema drift). The
    // closing chunk must carry None and the stream must not error;
    // a debug-level diagnostic fires for operator visibility but is
    // not asserted (no tracing_test wired).
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run(
        "messageStop",
        r#"{"stopReason":"stop_sequence"}"#,
        &mut state,
    );
    let closing = run(
        "metadata",
        r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
        &mut state,
    );

    // Assert
    assert_eq!(closing.len(), 1);
    assert!(closing[0].choices[0].matched_stop_sequence.is_none());
    assert_eq!(closing[0].choices[0].finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn matched_stop_sequence_lifts_on_eof_flush_path() {
    // Arrange: messageStop with a lifted stop_sequence, then EOF
    // before metadata arrives -- the synthetic closing chunk emitted
    // by the stream() flush must still carry the matched sequence.
    let frames: Vec<std::result::Result<Bytes, reqwest::Error>> = vec![
        Ok(encode_frame("messageStart", r#"{"role":"assistant"}"#)),
        Ok(encode_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"hi STOP"}}"#,
        )),
        Ok(encode_frame(
            "contentBlockStop",
            r#"{"contentBlockIndex":0}"#,
        )),
        Ok(encode_frame(
            "messageStop",
            r#"{"stopReason":"stop_sequence","additionalModelResponseFields":{"stop_sequence":"STOP"}}"#,
        )),
        // No metadata frame -- EOF flush path.
    ];
    let byte_stream = fstream::iter(frames);

    // Act
    let chunks: Vec<_> = stream("test".to_string(), byte_stream)
        .collect::<Vec<_>>()
        .await;

    // Assert
    let last = chunks
        .last()
        .expect("expected at least one chunk")
        .as_ref()
        .expect("EOF flush yielded an Err");
    assert_eq!(last.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        last.choices[0].matched_stop_sequence.as_deref(),
        Some("STOP")
    );
}

include!("eventstream_history_compat_tests.rs");
