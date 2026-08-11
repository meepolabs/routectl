//! Shared AWS-eventstream framing driver for the Bedrock egresses.
//!
//! Bedrock streams responses in `application/vnd.amazon.eventstream`
//! framing -- a binary format with a 12-byte prelude (total_length,
//! headers_length, prelude_crc), a headers section (`:event-type`,
//! `:content-type`, ...), a payload, and a trailing message CRC. Both
//! the InvokeModel-stream decoder and the ConverseStream decoder consume
//! the identical framing; only the per-frame payload interpretation
//! differs (Invoke unwraps base64 Anthropic SSE; Converse parses typed
//! AWS event objects).
//!
//! This module owns the byte loop, the prelude/length/DoS invariants,
//! the decode-error recovery, the EOF truncation checks, and -- crucially
//! -- the log-hygiene policy on the error-skip path (prelude-only at
//! WARN, full payload hex only at TRACE). Centralizing the policy here
//! keeps it from drifting between the two decoders. Per-provider code
//! receives a decoded, validated `Message` and never touches raw frame
//! bytes.

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::Message;
use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, Stream, StreamExt};

use routectl_core::{ChatChunk, Error, Result};

/// Cap on a single eventstream frame's advertised total length. AWS
/// eventstream's wire `total_length` is a raw `u32` (~4 GB cap by spec)
/// and Bedrock places no documented upper bound, but legitimate chunks
/// are bounded by the model's per-frame output -- typically well under
/// 64 KB. 8 MB is generous enough that we never trip on real traffic but
/// small enough that a malicious or compromised upstream can't drive the
/// buffer toward OOM by advertising a giant frame and never sending the
/// bytes.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Per-provider phrasing for the framing layer's log lines and error
/// envelopes. The two decoders historically carried distinct strings
/// (e.g. "bedrock eventstream ..." vs "bedrock converse-stream ...");
/// keeping that wording stable avoids surprising operators or log
/// dashboards keyed on the existing text.
#[derive(Clone, Copy)]
pub enum FrameLabel {
    Invoke,
    Converse,
}

impl FrameLabel {
    fn cap_exceeded(self, advertised: usize) -> String {
        match self {
            Self::Invoke => format!(
                "bedrock eventstream frame advertised {advertised} bytes, exceeds cap {MAX_FRAME_BYTES}"
            ),
            Self::Converse => format!(
                "bedrock converse-stream frame advertised {advertised} bytes, exceeds cap {MAX_FRAME_BYTES}"
            ),
        }
    }

    const fn consumed_overflow(self) -> &'static str {
        match self {
            Self::Invoke => "bedrock eventstream decoder consumed more than usize::MAX bytes",
            Self::Converse => "bedrock converse-stream consumed more than usize::MAX bytes",
        }
    }

    fn upstream_read(self, e: reqwest::Error) -> String {
        match self {
            Self::Invoke => format!("bedrock upstream byte read failed: {e}"),
            Self::Converse => format!("bedrock converse upstream byte read failed: {e}"),
        }
    }

    fn eof_buffered(self, left: usize) -> String {
        match self {
            Self::Invoke => {
                format!("bedrock stream truncated: {left} buffered bytes left at EOF")
            }
            Self::Converse => {
                format!("bedrock converse-stream truncated: {left} buffered bytes left at EOF")
            }
        }
    }

    const fn eof_prelude(self) -> &'static str {
        match self {
            Self::Invoke => {
                "bedrock stream truncated: prelude consumed but frame body never arrived before EOF"
            }
            Self::Converse => {
                "bedrock converse-stream truncated: prelude consumed but frame body never arrived before EOF"
            }
        }
    }

    const fn warn_skip(self) -> &'static str {
        match self {
            Self::Invoke => "bedrock eventstream frame decode failed; skipping frame",
            Self::Converse => "bedrock converse-stream frame decode failed; skipping frame",
        }
    }

    const fn trace_dump(self) -> &'static str {
        match self {
            Self::Invoke => "bedrock eventstream frame decode failed (full hex dump)",
            Self::Converse => "bedrock converse-stream frame decode failed (full hex dump)",
        }
    }
}

/// Per-frame payload interpretation. The framing driver calls `on_frame`
/// for each decoded, validated `Message` and yields whatever chunks it
/// returns; `on_eof` runs once at graceful end-of-stream so a handler can
/// flush any state it held across frames.
pub trait FrameHandler {
    /// Interpret one decoded, validated frame. Returns zero-or-more
    /// chunks to yield. `Err` is stream-fatal.
    fn on_frame(&mut self, provider_id: &str, message: Message) -> Result<Vec<ChatChunk>>;

    /// Called once at graceful EOF (empty buffer, no prelude pending).
    /// Returned chunks are the last items yielded. Default: none.
    fn on_eof(&mut self, _provider_id: &str) -> Vec<ChatChunk> {
        Vec::new()
    }
}

/// Look up a string-valued eventstream header by name.
pub fn header_str<'a>(message: &'a Message, name: &str) -> Option<&'a str> {
    for header in message.headers() {
        if header.name().as_str() == name
            && let Ok(s) = header.value().as_string()
        {
            return Some(s.as_str());
        }
    }
    None
}

/// What a decoded frame's headers say the frame IS.
///
/// The amazon-eventstream wire protocol types every frame with
/// `:message-type`; `:event-type` is only present when that value is
/// `"event"`. A modeled exception arrives as `:message-type: "exception"`
/// with the member name in `:exception-type`. Classifying on `:event-type`
/// alone therefore cannot see an exception frame at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType<'a> {
    /// A content event; the name is the `:event-type` header value.
    Event(&'a str),
    /// A failure frame. The name is the `:exception-type` member name
    /// when present, else the raw `:message-type` value -- so a frame the
    /// protocol types as a failure is never mistaken for content, even
    /// when its member name is one we do not know.
    Exception(&'a str),
    /// Neither `:message-type` nor `:event-type` names the frame. Nothing
    /// can be decoded from it and nothing marks it as a failure.
    Untyped,
}

/// Classify a decoded frame from its protocol headers.
///
/// Authority: `aws_smithy_eventstream::smithy::parse_response_headers`,
/// which reads `:event-type` only for `:message-type == "event"` and
/// `:exception-type` for `:message-type == "exception"`.
///
/// A missing `:message-type` violates the protocol, but is treated as
/// `"event"` so an upstream that emits only `:event-type` still decodes.
/// Any other `:message-type` (including the protocol's framework-level
/// `"error"`) is a failure frame: skipping it would truncate the response
/// while reporting clean completion.
pub fn frame_type(message: &Message) -> FrameType<'_> {
    match header_str(message, ":message-type") {
        Some("event") | None => match header_str(message, ":event-type") {
            Some(name) => FrameType::Event(name),
            None => FrameType::Untyped,
        },
        Some(message_type) => {
            FrameType::Exception(header_str(message, ":exception-type").unwrap_or(message_type))
        }
    }
}

/// Build the `Error::Upstream` for a Bedrock failure frame, mapping the
/// exception member name to the HTTP status the router classifies on.
///
/// Shared by both streaming lanes so the two Bedrock paths cannot drift
/// in how they classify the same upstream failure. An unrecognized name
/// maps to 500: a failure frame must end the stream even when its member
/// name is new to us, or the client sees a silently truncated response
/// and the breaker records the seat healthy.
pub fn exception_error(provider_id: &str, exception_type: &str, payload: &[u8]) -> Error {
    let parsed: serde_json::Value =
        serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null);
    let raw = parsed
        .pointer("/message")
        .or_else(|| parsed.pointer("/Message"))
        .and_then(|v| v.as_str())
        .unwrap_or(exception_type);
    // Bound the carried message at the shared error-body ceiling, matching
    // the non-stream lane (`bedrock::mod`). The only other bound here is the
    // 8 MiB frame cap -- 128x this ceiling -- so without the truncation a
    // hostile or misconfigured upstream could make every failed stream carry
    // a multi-megabyte String that is then cloned along the retry/fallback
    // chain.
    let msg = truncate_on_char_boundary(raw, routectl_core::MAX_ERROR_BODY_BYTES);
    // Statuses are AWS's own, per the ConverseStream response-element docs
    // (API_runtime_ConverseStream): each in-stream exception member
    // documents an HTTP Status Code. Preserving them keeps the router's
    // failure classification (retry class, breaker debit) aligned with
    // what AWS actually said went wrong -- collapsing everything to 500
    // would turn a documented-retryable 424/408 into a generic server
    // failure.
    let status: u16 = match exception_type {
        "throttlingException" => 429,
        "validationException" => 400,
        "serviceUnavailableException" => 503,
        "accessDeniedException" => 403,
        "unauthorizedException" => 401,
        // "A streaming error occurred. Retry your request." -- 424.
        "modelStreamErrorException" => 424,
        // "Processing time exceeded the model timeout length." -- 408.
        "modelTimeoutException" => 408,
        "internalServerException" => 500,
        _ => 500,
    };
    if matches!(
        exception_type,
        "accessDeniedException" | "unauthorizedException"
    ) {
        tracing::warn!(
            provider = %provider_id,
            event_type = %exception_type,
            message = %routectl_core::sanitize_for_log(&msg),
            "bedrock in-stream auth/permission exception",
        );
    }
    // No HTTP headers exist at the eventstream frame layer.
    Error::upstream(provider_id, status, msg)
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char
/// (`&s[..max]` panics on a multi-byte boundary, and an exception message is
/// upstream-controlled text that may be non-ASCII).
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Drive an AWS-eventstream byte stream through a per-provider handler,
/// emitting `ChatChunk`s. Partial frames buffer internally until enough
/// bytes arrive; the advertised-length DoS guard, prelude-tracking, and
/// decode-error recovery all live here so both Bedrock egresses share one
/// hardened implementation.
pub fn decode_frames<S, H>(
    provider_id: String,
    byte_stream: S,
    mut handler: H,
    label: FrameLabel,
) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
    H: FrameHandler + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut decoder = MessageFrameDecoder::new();
        // Tracks whether smithy has already consumed the 12-byte prelude
        // into its internal buffer but has not yet returned Complete. The
        // advertised-length DoS guard reads `buffer[0..4]` as the next
        // frame's `total_length` -- but that's only valid when no prelude
        // has been previously consumed. Once smithy has the prelude
        // buffered (after an Incomplete return), `buffer[0..4]` is the
        // START OF THE HEADERS section, not a length, and the cap check
        // would spuriously fire on header bytes that look like a giant
        // little-endian integer. Set to true when we drain the prelude on
        // Incomplete; cleared back to false when smithy returns Complete
        // (which internally calls `self.reset()` so its `prelude_read`
        // flag goes back to false).
        let mut smithy_has_prelude_buffered = false;

        let mut byte_stream = Box::pin(byte_stream);
        loop {
            // Try to decode any complete frames already in buffer. The
            // advertised-length DoS guard runs at the TOP of this inner
            // loop so it fires before EVERY decode attempt, not just once
            // per outer-loop tick. Otherwise a buffer holding [small valid
            // frame][giant malicious frame] would consume the small one
            // and decode the giant one without checking its advertised
            // total_length.
            loop {
                if !smithy_has_prelude_buffered && buffer.len() >= 4 {
                    let advertised = u32::from_be_bytes([
                        buffer[0], buffer[1], buffer[2], buffer[3],
                    ]) as usize;
                    if advertised > MAX_FRAME_BYTES {
                        yield Err(Error::Streaming(label.cap_exceeded(advertised)));
                        return;
                    }
                }
                let mut cursor = std::io::Cursor::new(buffer.as_ref());
                match decoder.decode_frame(&mut cursor) {
                    Ok(DecodedFrame::Complete(message)) => {
                        let consumed = usize::try_from(cursor.position()).map_err(|_| {
                            Error::Streaming(label.consumed_overflow().to_string())
                        })?;
                        let _ = buffer.split_to(consumed);
                        // smithy.decode_frame internally calls
                        // `self.reset()` on a successful Complete, clearing
                        // its `prelude_read` flag. Mirror that here so the
                        // cap check above re-engages for the next frame's
                        // prelude.
                        smithy_has_prelude_buffered = false;

                        match handler.on_frame(&provider_id, message) {
                            Ok(chunks) => {
                                for c in chunks {
                                    yield Ok(c);
                                }
                            }
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        }
                    }
                    Ok(DecodedFrame::Incomplete) => {
                        // Drain whatever bytes smithy consumed before
                        // returning Incomplete.
                        //
                        // `MessageFrameDecoder::decode_frame` reads the
                        // 12-byte prelude into its internal state on first
                        // call and sets `prelude_read = true`. It returns
                        // Incomplete because the rest of the frame hasn't
                        // arrived. Without this drain, the next iteration
                        // creates a fresh cursor at offset 0 -- but the
                        // prelude bytes are still there. On re-entry smithy
                        // skips re-reading the prelude (because
                        // `prelude_read=true`) and reads the next bytes
                        // from cursor offset 0 as the headers section --
                        // which is actually still the prelude. The
                        // big-endian `total_length` field gets interpreted
                        // as header tag/value pairs, fails UTF-8
                        // validation, and surfaces as `InvalidUtf8String`
                        // on a frame whose payload is perfectly valid.
                        //
                        // Symptom in the wild: any Bedrock streaming
                        // response that arrives in multiple HTTP body
                        // chunks hits a mid-stream UTF-8 error. Mirror
                        // smithy's prelude consumption back to our
                        // `BytesMut` so the next iteration's fresh cursor
                        // starts past the consumed prelude bytes.
                        let consumed = cursor.position() as usize;
                        if consumed > 0 {
                            let _ = buffer.split_to(consumed);
                            // Track that smithy still has the prelude
                            // buffered internally -- the cap check at the
                            // top of the loop must skip until smithy
                            // returns Complete (and resets).
                            smithy_has_prelude_buffered = true;
                        }
                        break;
                    }
                    Err(e) => {
                        // Skip the failed frame instead of killing the
                        // stream. A single transient bad frame should not
                        // conflate with a stream-wide failure and force the
                        // client to restart the whole response.
                        //
                        // Recovery: read the advertised `total_length` from
                        // `buffer[0..4]` (already DoS-capped at the top of
                        // the loop), drain that many bytes (or clear the
                        // whole buffer if it's smaller than the advertised
                        // length), reset the decoder, and continue.
                        //
                        // When `smithy_has_prelude_buffered` is true, smithy
                        // already consumed the 12-byte prelude on a prior
                        // Incomplete return and we drained it from `buffer`.
                        // So `buffer[0..4]` is now the START OF THE HEADERS
                        // section, NOT a frame `total_length` -- reading it
                        // as an advertised length yields garbage and would
                        // drive a mis-aligned `split_to`. In that state the
                        // only safe recovery is to clear the buffer (treat
                        // advertised as 0); the upstream marks a new frame
                        // boundary on the next send, so at most one extra
                        // frame is lost.
                        //
                        // Log hygiene: the 12-byte prelude (always
                        // non-content: total_length, headers_length,
                        // prelude_crc) is safe to emit at WARN for
                        // diagnosability. The variable-length payload may
                        // carry model output and must NOT go to a
                        // third-party log SaaS at WARN; it is gated at TRACE
                        // instead.
                        let advertised = if !smithy_has_prelude_buffered && buffer.len() >= 4 {
                            u32::from_be_bytes([
                                buffer[0], buffer[1], buffer[2], buffer[3],
                            ]) as usize
                        } else {
                            0
                        };
                        // Log only the 12-byte prelude at WARN.
                        let prelude_len = 12_usize.min(buffer.len());
                        let prelude_hex: String = buffer[..prelude_len]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        tracing::warn!(
                            provider = %provider_id,
                            err = %e,
                            frame_len = advertised,
                            prelude_hex = %prelude_hex,
                            "{}",
                            label.warn_skip()
                        );
                        // Full hex dump (up to 256 bytes) at TRACE only, so
                        // payload bytes never reach a third-party log SaaS
                        // by default.
                        if tracing::enabled!(tracing::Level::TRACE) {
                            let dump_len = advertised.min(256).min(buffer.len());
                            let hex: String = buffer[..dump_len]
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            tracing::trace!(
                                provider = %provider_id,
                                frame_len = advertised,
                                hex = %hex,
                                "{}",
                                label.trace_dump()
                            );
                        }

                        // Skip past the failed frame.
                        if advertised > 0 && buffer.len() >= advertised {
                            // Full malformed frame in buffer -- drain
                            // exactly the advertised length so frame N+1
                            // stays aligned.
                            let _ = buffer.split_to(advertised);
                        } else {
                            // Partial or zero-length: drop everything we
                            // have. The upstream marks a new frame boundary
                            // on every send, so the worst case is one extra
                            // frame lost.
                            buffer.clear();
                        }
                        // Reset the decoder so its internal `prelude_read`
                        // state and our flag align.
                        decoder = MessageFrameDecoder::new();
                        smithy_has_prelude_buffered = false;
                        continue;
                    }
                }
            }

            // Need more bytes; pull next chunk from upstream.
            match byte_stream.next().await {
                Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    yield Err(Error::Streaming(label.upstream_read(e)));
                    return;
                }
                None => {
                    // Upstream closed. If we still have buffered bytes --
                    // OR if smithy has a partial frame's prelude buffered
                    // internally -- the stream was truncated mid-frame.
                    // Both must surface as errors, not clean EOF.
                    //
                    // Without the `smithy_has_prelude_buffered` half of
                    // this check, an upstream that closes after sending
                    // exactly 12 bytes (a prelude with no body) would
                    // report success: our `buffer` is empty (we drained the
                    // 12 prelude bytes after the Incomplete return) but
                    // smithy still has an incomplete frame staged. The
                    // router's circuit breaker would record a "successful"
                    // probe for an unhealthy upstream.
                    if !buffer.is_empty() {
                        yield Err(Error::Streaming(label.eof_buffered(buffer.len())));
                    } else if smithy_has_prelude_buffered {
                        yield Err(Error::Streaming(label.eof_prelude().to_string()));
                    } else {
                        // Graceful EOF -- let the handler flush any state
                        // it held across frames (e.g. a captured
                        // stop_reason awaiting a metadata frame that never
                        // arrived).
                        for c in handler.on_eof(&provider_id) {
                            yield Ok(c);
                        }
                    }
                    return;
                }
            }
        }
    };

    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    use bytes::Bytes;

    /// Minimal handler: records the `:event-type` of every frame it sees
    /// into a channel and yields no chunks. Lets the framing tests assert
    /// on which frames reached the per-provider layer without any payload
    /// interpretation. Channel-backed so it stays `Send` for the async
    /// `decode_frames` driver.
    struct RecordingHandler {
        tx: std::sync::mpsc::Sender<String>,
    }

    impl FrameHandler for RecordingHandler {
        fn on_frame(&mut self, _provider_id: &str, message: Message) -> Result<Vec<ChatChunk>> {
            let et = header_str(&message, ":event-type")
                .unwrap_or("")
                .to_string();
            let _ = self.tx.send(et);
            Ok(Vec::new())
        }
    }

    fn make_frame(event_type: &str, payload_json: &str) -> Message {
        Message::new(Bytes::from(payload_json.to_string().into_bytes())).add_header(Header::new(
            ":event-type",
            HeaderValue::String(event_type.to_string().into()),
        ))
    }

    fn encode(event_type: &str, payload_json: &str) -> Vec<u8> {
        let frame = make_frame(event_type, payload_json);
        let mut buf = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut buf)
            .expect("encode eventstream frame");
        buf
    }

    #[test]
    fn header_str_finds_event_type() {
        let frame = make_frame("messageStart", "{}");
        assert_eq!(header_str(&frame, ":event-type"), Some("messageStart"));
        assert_eq!(header_str(&frame, ":content-type"), None);
    }

    /// `:message-type: "exception"` names the member in `:exception-type`;
    /// `:event-type` is absent. Per aws-smithy-eventstream's
    /// `parse_response_headers`.
    #[test]
    fn exception_message_type_classifies_from_exception_type_header() {
        let frame = Message::new(Bytes::from_static(b"{}"))
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("exception".to_string().into()),
            ))
            .add_header(Header::new(
                ":exception-type",
                HeaderValue::String("throttlingException".to_string().into()),
            ));
        assert_eq!(
            frame_type(&frame),
            FrameType::Exception("throttlingException")
        );
    }

    #[test]
    fn event_message_type_classifies_from_event_type_header() {
        let frame = Message::new(Bytes::from_static(b"{}"))
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".to_string().into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String("contentBlockDelta".to_string().into()),
            ));
        assert_eq!(frame_type(&frame), FrameType::Event("contentBlockDelta"));
    }

    /// A missing `:message-type` violates the protocol but is tolerated as
    /// an event so an upstream sending only `:event-type` still decodes.
    #[test]
    fn missing_message_type_falls_back_to_event_type() {
        let frame = make_frame("messageStart", "{}");
        assert_eq!(frame_type(&frame), FrameType::Event("messageStart"));
    }

    /// A `:message-type` that is neither "event" nor "exception" (e.g. the
    /// protocol's framework-level "error") is still a failure frame, named
    /// by its `:message-type` when no `:exception-type` accompanies it.
    #[test]
    fn other_message_type_is_a_failure_frame() {
        let frame = Message::new(Bytes::from_static(b"{}")).add_header(Header::new(
            ":message-type",
            HeaderValue::String("error".to_string().into()),
        ));
        assert_eq!(frame_type(&frame), FrameType::Exception("error"));
    }

    #[test]
    fn frame_with_no_type_headers_is_untyped() {
        let frame = Message::new(Bytes::from_static(b"{}"));
        assert_eq!(frame_type(&frame), FrameType::Untyped);
    }

    #[test]
    fn exception_error_maps_member_names_to_statuses() {
        let cases = [
            ("throttlingException", 429_u16),
            ("validationException", 400),
            ("serviceUnavailableException", 503),
            ("accessDeniedException", 403),
            ("unauthorizedException", 401),
            ("internalServerException", 500),
            ("someFutureAwsException", 500),
        ];
        for (name, expected) in cases {
            match exception_error("test", name, br#"{"message":"boom"}"#) {
                Error::Upstream { status, body, .. } => {
                    assert_eq!(status, expected, "status for {name}");
                    assert!(body.contains("boom"), "body for {name}: {body}");
                }
                other => panic!("expected Upstream for {name}, got {other:?}"),
            }
        }
    }

    /// With no `message` field in the payload, the member name itself is
    /// the error body -- a caller must never see an empty explanation.
    #[test]
    fn exception_error_falls_back_to_member_name_as_body() {
        match exception_error("test", "throttlingException", b"not json") {
            Error::Upstream { body, .. } => assert_eq!(body, "throttlingException"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn recording_handler_unit() {
        // Exercise RecordingHandler directly so the framing tests below
        // can rely on it recording one event-type per frame.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut h = RecordingHandler { tx };
        h.on_frame("test", make_frame("messageStop", "{}")).unwrap();
        let seen: Vec<String> = rx.try_iter().collect();
        assert_eq!(seen, vec!["messageStop".to_string()]);
    }

    /// A frame split exactly at the 12-byte prelude boundary across two
    /// HTTP body chunks must still decode cleanly. Without the Incomplete
    /// drain this surfaces as `InvalidUtf8String` from smithy; without the
    /// cap-check skip it fires a spurious advertised-length error.
    #[tokio::test]
    async fn frame_split_at_prelude_boundary_decodes() {
        let buf = encode("messageStart", r#"{"role":"assistant"}"#);
        assert!(buf.len() > 12, "frame must exceed its 12B prelude");
        let (head, tail) = buf.split_at(12);
        let byte_stream = futures::stream::iter(vec![
            Ok(Bytes::copy_from_slice(head)),
            Ok(Bytes::copy_from_slice(tail)),
        ]);

        let (tx, rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let mut chunks =
            decode_frames("test".to_string(), byte_stream, handler, FrameLabel::Invoke);
        while let Some(item) = chunks.next().await {
            if let Err(e) = item {
                panic!("prelude-split frame errored: {e:?}");
            }
        }
        let seen: Vec<String> = rx.try_iter().collect();
        assert_eq!(seen, vec!["messageStart".to_string()]);
    }

    /// An upstream that sends only a 12-byte prelude then closes is a
    /// mid-frame truncation. The buffer is empty (we drained the prelude on
    /// Incomplete) but smithy has the frame staged, so the EOF check must
    /// surface a prelude-truncation error rather than a clean close.
    #[tokio::test]
    async fn eof_after_prelude_only_is_truncation_error() {
        let buf = encode("messageStart", r#"{"role":"assistant"}"#);
        let prelude_only = Bytes::copy_from_slice(&buf[..12]);
        let byte_stream = futures::stream::iter(vec![Ok(prelude_only)]);

        let (tx, _rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let mut chunks = decode_frames(
            "test".to_string(),
            byte_stream,
            handler,
            FrameLabel::Converse,
        );

        let mut saw_truncation = false;
        while let Some(item) = chunks.next().await {
            match item {
                Ok(_) => panic!("expected truncation error, got Ok"),
                Err(Error::Streaming(msg)) => {
                    assert!(
                        msg.contains("truncated") && msg.contains("prelude"),
                        "expected prelude-truncation message, got: {msg}"
                    );
                    // Converse label phrasing must come through.
                    assert!(msg.contains("converse-stream"), "label not applied: {msg}");
                    saw_truncation = true;
                }
                Err(other) => panic!("expected Streaming error, got: {other:?}"),
            }
        }
        assert!(saw_truncation, "prelude-only EOF closed cleanly");
    }

    /// A frame advertising more than the 8 MB cap must abort the stream
    /// with the cap-exceeded error before any decode is attempted. The
    /// label selects the provider-specific phrasing.
    #[tokio::test]
    async fn advertised_length_over_cap_aborts() {
        // total_length in the first 4 bytes, big-endian, just over the cap.
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        let mut bytes = oversized.to_vec();
        // Pad so buffer.len() >= 4 is satisfied with extra noise.
        bytes.extend_from_slice(&[0u8; 8]);
        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(bytes))]);

        let (tx, _rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let mut chunks =
            decode_frames("test".to_string(), byte_stream, handler, FrameLabel::Invoke);

        let first = chunks.next().await.expect("expected an item");
        match first {
            Err(Error::Streaming(msg)) => {
                assert!(msg.contains("exceeds cap"), "got: {msg}");
                assert!(msg.contains("eventstream frame"), "invoke label: {msg}");
            }
            other => panic!("expected cap-exceeded Streaming error, got {other:?}"),
        }
        // Stream ends after the fatal error.
        assert!(chunks.next().await.is_none());
    }

    /// Two intact frames decode in sequence and both reach the handler.
    #[tokio::test]
    async fn two_intact_frames_reach_handler() {
        let mut combined = encode("messageStart", r#"{"role":"assistant"}"#);
        combined.extend_from_slice(&encode("messageStop", r#"{"stopReason":"end_turn"}"#));
        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(combined))]);

        let (tx, rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let mut chunks =
            decode_frames("test".to_string(), byte_stream, handler, FrameLabel::Invoke);
        while let Some(item) = chunks.next().await {
            if let Err(e) = item {
                panic!("two-frame stream errored: {e:?}");
            }
        }
        let seen: Vec<String> = rx.try_iter().collect();
        assert_eq!(
            seen,
            vec!["messageStart".to_string(), "messageStop".to_string()]
        );
    }

    /// on_eof runs on a graceful close (empty buffer, no prelude pending)
    /// and its chunks are yielded last.
    #[tokio::test]
    async fn on_eof_flushes_at_graceful_close() {
        struct EofFlushHandler;
        impl FrameHandler for EofFlushHandler {
            fn on_frame(
                &mut self,
                _provider_id: &str,
                _message: Message,
            ) -> Result<Vec<ChatChunk>> {
                Ok(Vec::new())
            }
            fn on_eof(&mut self, _provider_id: &str) -> Vec<ChatChunk> {
                vec![ChatChunk::default()]
            }
        }

        let buf = encode("messageStop", r#"{"stopReason":"end_turn"}"#);
        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(buf))]);
        let mut chunks = decode_frames(
            "test".to_string(),
            byte_stream,
            EofFlushHandler,
            FrameLabel::Converse,
        );

        let mut count = 0;
        while let Some(item) = chunks.next().await {
            item.expect("graceful EOF should not error");
            count += 1;
        }
        assert_eq!(
            count, 1,
            "on_eof flush chunk should be yielded exactly once"
        );
    }

    /// A frame that splits at the 12-byte prelude boundary and then
    /// delivers corrupt continuation bytes drives smithy's `decode_frame`
    /// into its `Err` arm while we have already drained the prelude
    /// (`smithy_has_prelude_buffered == true`). In that state `buffer[0..4]`
    /// is the HEADERS section, not a frame `total_length`, so the recovery
    /// must NOT read it as an advertised length. The fix forces
    /// `advertised = 0` whenever the prelude is buffered, so the recovery
    /// clears the buffer and realigns on the next frame boundary instead of
    /// `split_to`-ing a garbage count.
    ///
    /// This test exercises that exact code path (corrupt body after a
    /// prelude-only chunk) and asserts the driver recovers -- it neither
    /// panics nor surfaces a fatal error, and a following intact frame
    /// still reaches the handler. It does NOT assert the precise byte count
    /// drained on recovery: the old (buggy) and new code both eventually
    /// re-sync because the upstream marks a fresh frame boundary on each
    /// send, so the only observable difference is "one extra frame lost"
    /// under specific garbage-length values -- a best-effort recovery
    /// property, not a hard alignment invariant, and not cleanly
    /// distinguishable without coupling to smithy's internal CRC behavior.
    #[tokio::test]
    async fn error_with_prelude_buffered_recovers_cleanly() {
        // A valid frame, split so chunk 1 is exactly the 12-byte prelude.
        let good = encode("messageStart", r#"{"role":"assistant"}"#);
        assert!(good.len() > 12, "frame must exceed its 12B prelude");
        let prelude = &good[..12];
        // Corrupt continuation: same LENGTH as the real frame body (so
        // smithy attempts a full decode rather than waiting for more
        // bytes) but byte-flipped so the trailing message CRC fails and
        // decode_frame returns Err while smithy_has_prelude_buffered is
        // true.
        let corrupt_tail: Vec<u8> = good[12..].iter().map(|b| b ^ 0xFF).collect();
        // A following intact frame must still reach the handler after
        // recovery realigns the buffer.
        let recovery = encode("messageStop", r#"{"stopReason":"end_turn"}"#);

        let byte_stream = futures::stream::iter(vec![
            Ok(Bytes::copy_from_slice(prelude)),
            Ok(Bytes::from(corrupt_tail)),
            Ok(Bytes::from(recovery)),
        ]);

        let (tx, rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let mut chunks =
            decode_frames("test".to_string(), byte_stream, handler, FrameLabel::Invoke);

        // The corrupt frame is skipped (no fatal error reaches the client).
        while let Some(item) = chunks.next().await {
            if let Err(e) = item {
                panic!("decode-error recovery must not surface a fatal error: {e:?}");
            }
        }
        let seen: Vec<String> = rx.try_iter().collect();
        assert!(
            seen.contains(&"messageStop".to_string()),
            "after a prelude-buffered decode error, the next intact frame \
             must still decode; got {seen:?}"
        );
    }
}
