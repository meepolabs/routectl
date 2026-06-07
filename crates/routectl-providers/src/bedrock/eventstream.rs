//! Bedrock streaming response decoder.
//!
//! Bedrock streams responses in `application/vnd.amazon.eventstream`
//! framing -- a binary format with per-frame headers (`:event-type`,
//! `:content-type`, etc.) and a JSON payload. This module turns the
//! byte stream from `reqwest` into a `Stream<Item = Result<ChatChunk>>`.
//!
//! ## InvokeModel-stream frame shapes
//!
//! - `chunk` frames carry the actual model output. Their JSON payload
//!   is `{"bytes": "<base64>"}` where the base64-decoded bytes are an
//!   inner JSON object representing one Anthropic Messages API SSE
//!   event (`message_start`, `content_block_delta`, `message_stop`,
//!   etc.). We delegate that inner parsing to `anthropic_api::sse::SseState`.
//! - exception frames (`internalServerException`, `modelStreamErrorException`,
//!   `validationException`, `throttlingException`,
//!   `serviceUnavailableException`) carry an error JSON we map to
//!   `Error::Upstream`.
//!
//! ## Converse-stream frame shapes
//!
//! AWS-shaped event types (`messageStart`, `contentBlockStart`,
//! `contentBlockDelta`, `contentBlockStop`, `messageStop`, `metadata`).
//! Translation to ChatChunk lives in `converse::eventstream`; this
//! module handles only the framing layer shared by both shapes.

use aws_smithy_types::event_stream::Message;
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream};
use serde_json::Value;

use routectl_core::{ChatChunk, Error, Result};

use super::frame::{self, FrameHandler, FrameLabel};
use crate::anthropic_api::sse::SseState;

/// Per-frame handler for the Anthropic-shape Invoke stream. Holds the
/// `SseState` that accumulates across `chunk` frames; the framing layer
/// owns everything up to the decoded `Message`.
struct InvokeFrameHandler {
    sse_state: SseState,
}

impl FrameHandler for InvokeFrameHandler {
    fn on_frame(&mut self, provider_id: &str, message: Message) -> Result<Vec<ChatChunk>> {
        handle_invoke_frame(provider_id, message, &mut self.sse_state)
            .map(|maybe| maybe.into_iter().collect())
    }
}

/// Decode Bedrock InvokeModel-stream frames into routectl `ChatChunk`s.
///
/// `byte_stream` is the body of a `/invoke-with-response-stream` HTTP
/// response from `reqwest`. The shared `frame::decode_frames` driver
/// handles the AWS-eventstream framing; this function supplies only the
/// Anthropic-SSE payload interpretation.
pub fn invoke_stream<S>(
    provider_id: String,
    byte_stream: S,
) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let handler = InvokeFrameHandler {
        sse_state: SseState::default(),
    };
    frame::decode_frames(provider_id, byte_stream, handler, FrameLabel::Invoke)
}

/// Decode Bedrock ConverseStream frames into routectl `ChatChunk`s.
/// Delegates to `super::converse::eventstream_stream` for the
/// Converse-specific frame routing; this wrapper exists so the
/// dispatch site in `super::mod` can call a stable name regardless of
/// which sub-module owns the impl.
pub fn converse_stream<S>(
    provider_id: String,
    byte_stream: S,
) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    super::converse::eventstream_stream(provider_id, byte_stream)
}

/// Map a single decoded eventstream frame into an optional ChatChunk.
/// Returns `Ok(None)` for non-content frames (no-op) and `Err` for
/// upstream exception frames.
fn handle_invoke_frame(
    provider_id: &str,
    message: Message,
    sse_state: &mut SseState,
) -> Result<Option<ChatChunk>> {
    let event_type = frame::header_str(&message, ":event-type")
        .unwrap_or("")
        .to_string();
    let payload_bytes = message.payload();

    match event_type.as_str() {
        "chunk" => {
            // Payload is JSON: { "bytes": "<base64>" } where the
            // base64-decoded bytes is an Anthropic Messages SSE event.
            //
            // A malformed chunk's outer JSON would be stream-fatal.
            // Demote to `Ok(None)` + WARN so a single bad frame
            // doesn't kill an in-flight response. The failed frame's
            // bytes never reach the SSE state machine, and `sse_state`
            // is tolerant to one missing event.
            let outer: Value = match serde_json::from_slice(payload_bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        provider = %provider_id,
                        err = %e,
                        raw_len = payload_bytes.len(),
                        "bedrock chunk payload not JSON; skipping"
                    );
                    return Ok(None);
                }
            };
            let b64 = match outer.get("bytes").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => {
                    // Missing `bytes` field -- treat symmetrically with
                    // the malformed-outer-JSON arm above: WARN and skip
                    // the frame rather than killing the stream. One
                    // malformed frame should not abort an in-flight
                    // response.
                    tracing::warn!(
                        provider = %provider_id,
                        "bedrock chunk payload missing `bytes` field; skipping"
                    );
                    return Ok(None);
                }
            };
            let decoded = B64_STANDARD.decode(b64).map_err(|e| {
                Error::Streaming(format!("bedrock chunk bytes not valid base64: {e}"))
            })?;
            // Use `from_utf8_lossy` rather than strict `from_utf8`.
            // A multi-byte char (emoji, CJK character) split exactly
            // across two SSE chunk boundaries would surface as
            // `Streaming("not valid utf-8")` and kill the stream.
            // Replacement with U+FFFD lets the response finish; the
            // model rarely emits a single replacement char where
            // text was intended.
            let inner_owned = String::from_utf8_lossy(&decoded).into_owned();
            let inner = inner_owned.as_str();
            // Detect Anthropic-shape `error` events injected into an
            // otherwise-200 stream (e.g. `overloaded_error` mid-stream).
            // The default SseState swallows these as `Ok(None)` -- we
            // need to surface them to the client so the stream doesn't
            // end silently in the middle of a generation.
            if let Ok(parsed) = serde_json::from_str::<Value>(inner) {
                if parsed.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let err_type = parsed
                        .pointer("/error/type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("error");
                    let err_msg = parsed
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("upstream signaled error event mid-stream");
                    let status = match err_type {
                        "overloaded_error" => 529,
                        "rate_limit_error" => 429,
                        "invalid_request_error" => 400,
                        "authentication_error" => 401,
                        "permission_error" => 403,
                        "not_found_error" => 404,
                        _ => 502,
                    };
                    if matches!(err_type, "authentication_error" | "permission_error") {
                        tracing::warn!(
                            provider = %provider_id,
                            event_type = err_type,
                            message = %routectl_core::sanitize_for_log(err_msg),
                            "bedrock in-stream auth/permission exception",
                        );
                    }
                    return Err(Error::upstream(
                        provider_id,
                        status,
                        format!("{err_type}: {err_msg}"),
                    ));
                }
            }
            sse_state.parse_event(provider_id, inner)
        }
        "internalServerException"
        | "modelStreamErrorException"
        | "validationException"
        | "throttlingException"
        | "serviceUnavailableException"
        | "accessDeniedException"
        | "unauthorizedException" => {
            let payload: Value = serde_json::from_slice(payload_bytes).unwrap_or(Value::Null);
            let msg = payload
                .pointer("/message")
                .or_else(|| payload.pointer("/Message"))
                .and_then(|v| v.as_str())
                .unwrap_or(event_type.as_str())
                .to_string();
            // Synthesize a status code roughly matching the exception kind.
            let status: u16 = match event_type.as_str() {
                "throttlingException" => 429,
                "validationException" => 400,
                "serviceUnavailableException" => 503,
                "accessDeniedException" => 403,
                "unauthorizedException" => 401,
                _ => 500,
            };
            if matches!(
                event_type.as_str(),
                "accessDeniedException" | "unauthorizedException"
            ) {
                tracing::warn!(
                    provider = %provider_id,
                    event_type = %event_type,
                    message = %routectl_core::sanitize_for_log(&msg),
                    "bedrock in-stream auth/permission exception",
                );
            }
            Err(Error::upstream(provider_id, status, msg))
        }
        // Unknown frame types -- log and skip rather than fail. AWS
        // adds new events occasionally and we don't want clients
        // breaking on extensions.
        other => {
            tracing::debug!(
                provider = provider_id,
                event_type = other,
                "bedrock: skipping unknown eventstream frame"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    use base64::Engine;
    use bytes::Bytes;
    use futures::stream::StreamExt;
    use routectl_core::Error;

    fn make_frame(event_type: &str, payload_json: &str) -> Message {
        Message::new(Bytes::from(payload_json.to_string().into_bytes())).add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ))
    }

    fn handle(event_type: &str, payload: &str) -> Result<Option<ChatChunk>> {
        let mut sse_state = SseState::default();
        handle_invoke_frame(
            "test-bedrock",
            make_frame(event_type, payload),
            &mut sse_state,
        )
    }

    #[test]
    fn throttling_exception_maps_to_429() {
        let payload = r#"{"message":"slow down"}"#;
        let err = handle("throttlingException", payload).unwrap_err();
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 429);
                assert!(body.contains("slow down"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn validation_exception_maps_to_400() {
        let payload = r#"{"Message":"bad model id"}"#;
        let err = handle("validationException", payload).unwrap_err();
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 400);
                assert!(body.contains("bad model id"));
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn service_unavailable_maps_to_503() {
        let err = handle("serviceUnavailableException", "{}").unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn internal_server_exception_maps_to_500() {
        let err = handle("internalServerException", "{}").unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_is_skipped_not_error() {
        let res = handle("someBrandNewEventType", "{}");
        assert!(matches!(res, Ok(None)), "got {res:?}");
    }

    #[test]
    fn chunk_payload_missing_bytes_field_skips_frame() {
        // After the symmetric-skip fix, a chunk frame missing the
        // `bytes` field should produce Ok(None) and a WARN -- not a
        // stream-fatal Err. This mirrors the behavior for malformed
        // outer JSON.
        let payload = r#"{"not_bytes":"oops"}"#;
        let res = handle("chunk", payload);
        match res {
            Ok(None) => {} // expected: frame skipped
            Ok(Some(_)) => panic!("missing-bytes chunk should skip, not yield a chunk"),
            Err(e) => {
                panic!("regression: missing-bytes chunk returned Err instead of Ok(None): {e:?}")
            }
        }
    }

    #[test]
    fn chunk_with_invalid_base64_errors() {
        let payload = r#"{"bytes":"!!!not-base64!!!"}"#;
        let err = handle("chunk", payload).unwrap_err();
        match err {
            Error::Streaming(msg) => assert!(msg.contains("not valid base64"), "got: {msg}"),
            other => panic!("expected Streaming, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_overloaded_error_in_stream_surfaces_as_529() {
        let inner = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner.as_bytes());
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let err = handle("chunk", &payload).unwrap_err();
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 529, "expected 529 for overloaded_error");
                assert!(body.contains("overloaded_error"), "body: {body}");
                assert!(body.contains("slow"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_rate_limit_error_in_stream_surfaces_as_429() {
        let inner = r#"{"type":"error","error":{"type":"rate_limit_error","message":"too fast"}}"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner.as_bytes());
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let err = handle("chunk", &payload).unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 429),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    // The MAX_FRAME_BYTES cap (8 MB) is documented at the const
    // declaration. We don't add a constant-on-constant assertion
    // here because clippy folds it; if the constant ever drifts
    // below 1 MB or above 64 MB, code review (or this comment) is
    // the place to catch that.

    /// Regression: a Bedrock eventstream frame split exactly at
    /// the 12-byte prelude boundary across two HTTP body chunks must
    /// still decode cleanly. Without the drain this surfaces as an
    /// `InvalidUtf8String` error inside smithy because the cursor
    /// position wasn't drained from our buffer when the decoder
    /// returned `Incomplete` -- the second iteration's fresh cursor
    /// re-read the prelude bytes as the headers section.
    ///
    /// A neighboring bug fell out of the same fix: the
    /// advertised-length DoS guard reads `buffer[0..4]` as the next
    /// frame's `total_length`, but only when no prelude has been
    /// previously consumed. The fix tracks this via the
    /// `smithy_has_prelude_buffered` flag so the cap check skips
    /// post-Incomplete iterations -- otherwise the first 4 bytes of
    /// the headers section get misread as a frame size of ~188 MB
    /// and the cap fires spuriously.
    #[tokio::test]
    async fn frame_split_at_prelude_boundary_streaming() {
        // Construct a `chunk` event whose payload is a base64-wrapped
        // Anthropic SSE event. Use `ping` because it's the simplest
        // shape that SseState accepts without further deltas.
        let inner = r#"{"type":"ping"}"#;
        let b64 = B64_STANDARD.encode(inner.as_bytes());
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let frame = make_frame("chunk", &payload);

        // Encode the frame to its on-the-wire bytes.
        let mut buf = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
            .expect("encode eventstream frame");
        assert!(buf.len() > 12, "frame must be larger than its 12B prelude");

        // Split at exactly byte 12 -- the prelude boundary. This is
        // the worst case because smithy reads the prelude into its
        // internal state on the first decode call and returns
        // Incomplete; without our drain, the next call reads the
        // prelude bytes again as headers.
        let (head, tail) = buf.split_at(12);
        let head = Bytes::copy_from_slice(head);
        let tail = Bytes::copy_from_slice(tail);

        let byte_stream = futures::stream::iter(vec![Ok(head), Ok(tail)]);
        let mut chunks = invoke_stream("test-bedrock".to_string(), byte_stream);

        // The `ping` event maps to no ChatChunk (sse_state returns
        // None) but it MUST NOT fail the stream. Without the drain
        // this would yield an Err via the InvalidUtf8String path;
        // with the drain but without the cap-check skip, it would
        // fire the `advertised ... exceeds cap` error.
        while let Some(item) = chunks.next().await {
            match item {
                Ok(_) => {}
                Err(e) => panic!("regression: stream errored on prelude-split frame: {e:?}"),
            }
        }
    }

    /// Second-order test: when an upstream sends just a 12-byte
    /// prelude and then closes the connection, we drain the prelude
    /// (so `buffer.is_empty()` is true) but smithy still has a
    /// partial frame staged. Without the second-fix the EOF check
    /// returned `Ok(())` to the caller, hiding the truncation. Now
    /// the `smithy_has_prelude_buffered` half of the EOF check fires.
    #[tokio::test]
    async fn eof_after_prelude_only_yields_truncation_error() {
        // Build a real frame, then take only its 12-byte prelude.
        let inner = r#"{"type":"ping"}"#;
        let b64 = B64_STANDARD.encode(inner.as_bytes());
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let frame = make_frame("chunk", &payload);
        let mut buf = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf).unwrap();
        let prelude_only = Bytes::copy_from_slice(&buf[..12]);

        // Stream yields the prelude bytes once, then EOF.
        let byte_stream = futures::stream::iter(vec![Ok(prelude_only)]);
        let mut chunks = invoke_stream("test-bedrock".to_string(), byte_stream);

        let mut saw_truncation = false;
        while let Some(item) = chunks.next().await {
            match item {
                Ok(_) => panic!("expected truncation error, got Ok"),
                Err(Error::Streaming(msg)) => {
                    assert!(
                        msg.contains("truncated") && msg.contains("prelude"),
                        "expected prelude-truncation message, got: {msg}"
                    );
                    saw_truncation = true;
                }
                Err(other) => panic!("expected Streaming error, got: {other:?}"),
            }
        }
        assert!(
            saw_truncation,
            "stream closed cleanly after prelude-only EOF -- circuit breaker would record success on an unhealthy upstream"
        );
    }

    /// Multi-frame test: two consecutive frames where frame 1 is
    /// split at byte 12 and frame 2 arrives intact. Verifies the
    /// `smithy_has_prelude_buffered` flag transitions correctly:
    /// false -> true (after frame 1 Incomplete + drain) -> false
    /// (after frame 1 Complete, smithy resets) -> false (frame 2
    /// completes in one step, no Incomplete in between).
    #[tokio::test]
    async fn two_frame_stream_flag_lifecycle() {
        let make_chunk_bytes = |type_str: &str| -> Vec<u8> {
            let inner = format!(r#"{{"type":"{type_str}"}}"#);
            let b64 = B64_STANDARD.encode(inner.as_bytes());
            let payload = format!(r#"{{"bytes":"{b64}"}}"#);
            let frame = make_frame("chunk", &payload);
            let mut buf = Vec::new();
            aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf).unwrap();
            buf
        };

        let frame1 = make_chunk_bytes("ping");
        let frame2 = make_chunk_bytes("ping");

        // Frame 1 split at 12; frame 2 in one chunk on top.
        let (head1, tail1) = frame1.split_at(12);
        let mut combined_tail: Vec<u8> = tail1.to_vec();
        combined_tail.extend_from_slice(&frame2);

        let byte_stream = futures::stream::iter(vec![
            Ok(Bytes::copy_from_slice(head1)),
            Ok(Bytes::from(combined_tail)),
        ]);
        let mut chunks = invoke_stream("test-bedrock".to_string(), byte_stream);

        // Both frames are pings -> sse_state yields no ChatChunks.
        // The test passes if the stream completes without error.
        while let Some(item) = chunks.next().await {
            if let Err(e) = item {
                panic!("two-frame stream errored unexpectedly: {e:?}");
            }
        }
    }

    /// A multi-byte UTF-8 sequence in a chunk payload that happens
    /// to be invalid (split across frames in real life, or just a
    /// bad byte) must NOT kill the stream. Strict
    /// `std::str::from_utf8` would surface as `Streaming("not valid
    /// utf-8")` and terminate. Lossy decoding inserts U+FFFD and
    /// the chunk continues.
    #[tokio::test]
    async fn lossy_utf8_in_chunk_payload_does_not_fail_stream() {
        // Build inner SSE event with a bare 0xFE byte (invalid UTF-8
        // start byte) embedded in a text delta.
        let mut inner = b"{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi"
            .to_vec();
        inner.push(0xFE); // invalid UTF-8 start byte
        inner.extend_from_slice(b"\"}}");

        let b64 = B64_STANDARD.encode(&inner);
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let frame = make_frame("chunk", &payload);
        let mut buf = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf).unwrap();

        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(buf))]);
        let mut chunks = invoke_stream("test-bedrock".to_string(), byte_stream);

        // The lossy decode replaces 0xFE with U+FFFD; the resulting
        // string is valid JSON wrapping a text delta, so the stream
        // yields a chunk (not an error).
        while let Some(item) = chunks.next().await {
            match item {
                Ok(_) => {}
                Err(e) => panic!("regression: stream errored on lossy-utf8 payload: {e:?}"),
            }
        }
    }

    /// A chunk frame whose payload is malformed JSON would be
    /// stream-fatal. Now it emits a per-frame WARN and skips the
    /// frame, returning `Ok(None)` from `handle_invoke_frame`.
    #[test]
    fn malformed_chunk_json_skips_via_handle_invoke_frame() {
        // Direct unit test of handle_invoke_frame -- exercising the
        // chunk arm's skip path.
        let mut sse_state = SseState::default();
        // Payload that's not a JSON object at all.
        let res = handle_invoke_frame(
            "test-bedrock",
            make_frame("chunk", "not even close to json"),
            &mut sse_state,
        );
        match res {
            Ok(None) => {} // expected
            Ok(Some(_)) => panic!("malformed chunk should skip, not yield"),
            Err(e) => {
                panic!("regression: malformed chunk JSON returned Err instead of Ok(None): {e:?}")
            }
        }
    }

    /// A frame with a corrupted `total_length` causes smithy to
    /// return Err. Without the recovery this would kill the stream.
    /// Instead, we emit a WARN with hex dump, skip exactly the
    /// advertised length (or clear buffer if smaller), reset the
    /// decoder, and continue. The next valid frame yields normally.
    #[tokio::test]
    async fn malformed_frame_skip_continues_stream() {
        // Build a valid frame, then corrupt its CRC bytes (the last
        // 4 bytes) so smithy fails its CRC check on decode.
        let inner = r#"{"type":"ping"}"#;
        let b64 = B64_STANDARD.encode(inner.as_bytes());
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let frame_good = make_frame("chunk", &payload);
        let mut buf_good = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame_good, &mut buf_good).unwrap();

        // Corrupt frame: same total_length, but flip the message CRC
        // bytes (last 4 bytes of the frame). Smithy's frame decoder
        // reads the message-CRC at the end and Errors on mismatch,
        // exercising the skip path.
        let mut buf_bad = buf_good.clone();
        let n = buf_bad.len();
        buf_bad[n - 4] ^= 0xFF;
        buf_bad[n - 3] ^= 0xFF;

        // Sequence: bad frame, good frame.
        let mut combined = buf_bad;
        combined.extend_from_slice(&buf_good);
        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(combined))]);
        let mut chunks = invoke_stream("test-bedrock".to_string(), byte_stream);

        // We should NOT see an Err from the stream -- the bad frame
        // is skipped (with a WARN) and the good frame's `ping`
        // produces no chunks but doesn't fail. Without the recovery
        // this would yield an Err on the bad frame's CRC mismatch.
        while let Some(item) = chunks.next().await {
            if let Err(e) = item {
                panic!("regression: stream errored instead of skipping: {e:?}");
            }
        }
    }
}
