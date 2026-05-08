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
//! Translation to ChatChunk is in `converse.rs` (M2.7); this module
//! handles only the framing layer for both shapes.

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::Message;
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine;
use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::Value;

use routectl_core::{ChatChunk, Error, Result};

use crate::anthropic_api::sse::SseState;

/// Cap on a single eventstream frame's advertised total length. AWS
/// eventstream's wire `total_length` is a raw `u32` (~4 GB cap by spec)
/// and Bedrock places no documented upper bound, but legitimate
/// chunks are bounded by the model's per-frame output -- typically
/// well under 64 KB. 8 MB is generous enough that we never trip on
/// real traffic but small enough that a malicious or compromised
/// upstream can't drive the buffer toward OOM by advertising a giant
/// frame and never sending the bytes.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Decode Bedrock InvokeModel-stream frames into routectl `ChatChunk`s.
///
/// `byte_stream` is the body of a `/invoke-with-response-stream` HTTP
/// response from `reqwest`. Each emitted item is an attempt to parse
/// the next complete eventstream frame; partial frames buffer
/// internally until enough bytes arrive.
pub fn invoke_stream<S>(
    provider_id: String,
    byte_stream: S,
) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut decoder = MessageFrameDecoder::new();
        let mut sse_state = SseState::default();

        let mut byte_stream = Box::pin(byte_stream);
        loop {
            // Try to decode any complete frames already in buffer. The
            // advertised-length DoS guard runs at the TOP of this inner
            // loop so it fires before EVERY decode attempt, not just
            // once per outer-loop tick. Otherwise a buffer holding
            // [small valid frame][giant malicious frame] would consume
            // the small one and decode the giant one without checking
            // its advertised total_length.
            loop {
                if buffer.len() >= 4 {
                    let advertised = u32::from_be_bytes([
                        buffer[0], buffer[1], buffer[2], buffer[3],
                    ]) as usize;
                    if advertised > MAX_FRAME_BYTES {
                        yield Err(Error::Streaming(format!(
                            "bedrock eventstream frame advertised {advertised} bytes, exceeds cap {MAX_FRAME_BYTES}"
                        )));
                        return;
                    }
                }
                let mut cursor = std::io::Cursor::new(buffer.as_ref());
                match decoder.decode_frame(&mut cursor) {
                    Ok(DecodedFrame::Complete(message)) => {
                        let consumed = usize::try_from(cursor.position()).map_err(|_| {
                            Error::Streaming(
                                "bedrock eventstream decoder consumed more than usize::MAX bytes"
                                    .into(),
                            )
                        })?;
                        let _ = buffer.split_to(consumed);

                        match handle_invoke_frame(&provider_id, message, &mut sse_state) {
                            Ok(maybe_chunk) => {
                                if let Some(chunk) = maybe_chunk {
                                    yield Ok(chunk);
                                }
                            }
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        }
                    }
                    Ok(DecodedFrame::Incomplete) => break,
                    Err(e) => {
                        yield Err(Error::Streaming(format!(
                            "bedrock eventstream decode failed: {e}"
                        )));
                        return;
                    }
                }
            }

            // Need more bytes; pull next chunk from upstream.
            match byte_stream.next().await {
                Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    yield Err(Error::Streaming(format!(
                        "bedrock upstream byte read failed: {e}"
                    )));
                    return;
                }
                None => return,
            }
        }
    };

    Box::pin(stream)
}

/// Decode Bedrock Converse-stream frames into routectl `ChatChunk`s.
/// Translation of the AWS-shaped events lives in `converse.rs` (planned
/// for v0.4.0); for now this stub mirrors the Invoke shape so the trait
/// surface holds. Yields a single not-implemented `Err` on first poll
/// so callers see an explicit error rather than a silently-empty stream.
pub fn converse_stream<S>(
    _provider_id: String,
    _byte_stream: S,
) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(futures::stream::once(async {
        Err(Error::Streaming(
            "bedrock converse-stream chunk translation not implemented yet (M2.7)".into(),
        ))
    }))
}

/// Map a single decoded eventstream frame into an optional ChatChunk.
/// Returns `Ok(None)` for non-content frames (no-op) and `Err` for
/// upstream exception frames.
fn handle_invoke_frame(
    provider_id: &str,
    message: Message,
    sse_state: &mut SseState,
) -> Result<Option<ChatChunk>> {
    let event_type = header_str(&message, ":event-type")
        .unwrap_or("")
        .to_string();
    let payload_bytes = message.payload();

    match event_type.as_str() {
        "chunk" => {
            // Payload is JSON: { "bytes": "<base64>" } where the
            // base64-decoded bytes is an Anthropic Messages SSE event.
            let outer: Value = serde_json::from_slice(payload_bytes).map_err(|e| {
                Error::Streaming(format!(
                    "bedrock chunk payload not JSON: {e} (raw len={})",
                    payload_bytes.len()
                ))
            })?;
            let b64 = outer.get("bytes").and_then(|v| v.as_str()).ok_or_else(|| {
                Error::Streaming("bedrock chunk payload missing `bytes` field".into())
            })?;
            let decoded = B64_STANDARD.decode(b64).map_err(|e| {
                Error::Streaming(format!("bedrock chunk bytes not valid base64: {e}"))
            })?;
            let inner = std::str::from_utf8(&decoded).map_err(|e| {
                Error::Streaming(format!("bedrock chunk bytes not valid utf-8: {e}"))
            })?;
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
                            message = %err_msg,
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
                    message = %msg,
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

fn header_str<'a>(message: &'a Message, name: &str) -> Option<&'a str> {
    for header in message.headers() {
        if header.name().as_str() == name {
            if let Ok(s) = header.value().as_string() {
                return Some(s.as_str());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    use base64::Engine;
    use bytes::Bytes;
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
    fn chunk_payload_missing_bytes_field_errors() {
        let payload = r#"{"not_bytes":"oops"}"#;
        let err = handle("chunk", payload).unwrap_err();
        match err {
            Error::Streaming(msg) => assert!(msg.contains("missing `bytes` field"), "got: {msg}"),
            other => panic!("expected Streaming, got {other:?}"),
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
}
