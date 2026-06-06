//! Tests for the Converse-stream eventstream decoder.
//!
//! Lives in a sibling file so `eventstream.rs` stays under the
//! project's 800-line ceiling. Imported via
//! `#[path = "eventstream_tests.rs"] mod tests;` from `eventstream.rs`,
//! which means `super::*` here resolves to the parent module's
//! private items (`handle_converse_frame`, `ConverseStreamState`).

use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message as AwsMessage};
use futures::stream as fstream;
use futures::StreamExt;

fn make_frame(event_type: &str, payload_json: &str) -> AwsMessage {
    AwsMessage::new(Bytes::from(payload_json.to_string().into_bytes())).add_header(Header::new(
        ":event-type",
        HeaderValue::String(event_type.to_string().into()),
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
fn message_start_emits_no_chunk() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let chunks = run("messageStart", r#"{"role":"assistant"}"#, &mut state);

    // Assert
    assert!(chunks.is_empty());
}

#[test]
fn text_block_lifecycle_yields_text_deltas() {
    // Arrange: start, two deltas, stop.
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":0}"#,
        &mut state,
    );
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
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":0}"#,
        &mut state,
    );
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
    let _ = run(
        "contentBlockStart",
        r#"{"contentBlockIndex":0}"#,
        &mut state,
    );
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

#[test]
fn model_stream_error_exception_surfaces_as_upstream_500() {
    // Arrange
    let mut state = ConverseStreamState::default();

    // Act
    let res = handle_converse_frame(
        "test",
        make_frame("modelStreamErrorException", r#"{"message":"glitch"}"#),
        &mut state,
    );

    // Assert: maps to 500 (default for non-throttling/validation
    // exceptions). The caller (router) re-classifies based on
    // status; we only need to make sure it's an Upstream variant
    // with the body propagated.
    let err = res.unwrap_err();
    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 500);
            assert!(body.contains("glitch"), "body: {body}");
        }
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
            "contentBlockStart",
            r#"{"contentBlockIndex":0}"#,
        )),
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
    let any_err = chunks.iter().any(|c| c.is_err());
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
            "contentBlockStart",
            r#"{"contentBlockIndex":0}"#,
        )),
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
