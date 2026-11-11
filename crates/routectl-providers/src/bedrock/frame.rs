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
//! the fail-closed handling of frame decode errors, the EOF truncation
//! checks, and -- crucially -- the log-hygiene policy on the decode-error
//! path (prelude-only at WARN, full payload hex only at TRACE). Centralizing the policy here
//! keeps it from drifting between the two decoders. Per-provider code
//! receives a decoded, validated `Message` and never touches raw frame
//! bytes.

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::Message;
use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, Stream, StreamExt};

use routectl_core::{ChatChunk, Error, Result, sanitize_for_log};

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

    const fn warn_decode_failed(self) -> &'static str {
        match self {
            Self::Invoke => "bedrock eventstream frame decode failed; ending stream",
            Self::Converse => "bedrock converse-stream frame decode failed; ending stream",
        }
    }

    fn decode_failed(self, e: &impl std::fmt::Display) -> String {
        match self {
            Self::Invoke => format!("bedrock eventstream frame decode failed: {e}"),
            Self::Converse => format!("bedrock converse-stream frame decode failed: {e}"),
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
/// flush any state it held across frames, or end the stream with an error
/// when the lane's terminal event never arrived.
pub trait FrameHandler {
    /// Interpret one decoded, validated frame. Returns zero-or-more
    /// chunks to yield. `Err` is stream-fatal.
    fn on_frame(&mut self, provider_id: &str, message: Message) -> Result<Vec<ChatChunk>>;

    /// Called once at graceful EOF (empty buffer, no prelude pending).
    /// Returned chunks are the last items yielded; `Err` is yielded as the
    /// stream's final item. Default: none.
    fn on_eof(&mut self, _provider_id: &str) -> Result<Vec<ChatChunk>> {
        Ok(Vec::new())
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
    // An unrecognized exception still ends the stream, failing closed as a
    // generic server fault.
    let status = crate::aws_error::aws_stream_exception_status(exception_type).unwrap_or(500);
    if matches!(
        exception_type,
        "accessDeniedException" | "unauthorizedException"
    ) {
        tracing::warn!(
            provider = %provider_id,
            event_type = %sanitize_for_log(exception_type),
            message = %routectl_core::upstream_text_for_log(raw),
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

/// Outcome of one decode attempt over the buffered bytes.
enum DecodeStep {
    /// A complete, CRC-valid frame; its bytes are already drained.
    Frame(Message),
    /// The buffer holds no complete frame; read more bytes.
    NeedMore,
    /// Stream-fatal: the stream yields this error and ends.
    Fatal(Error),
}

/// Bytes received but not yet decoded, plus the decoder's own staged
/// prelude state.
struct FrameBuffer {
    buffer: BytesMut,
    decoder: MessageFrameDecoder,
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
    smithy_has_prelude_buffered: bool,
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
            decoder: MessageFrameDecoder::new(),
            smithy_has_prelude_buffered: false,
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next frame's advertised `total_length`, readable only while no
    /// prelude is staged inside the decoder (otherwise `buffer[0..4]` is
    /// header bytes, not a length).
    fn advertised_len(&self) -> Option<usize> {
        if self.smithy_has_prelude_buffered || self.buffer.len() < 4 {
            return None;
        }
        let b = &self.buffer;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    /// Attempt to decode one frame from the buffer. The advertised-length
    /// DoS guard runs before EVERY attempt, not once per upstream chunk:
    /// otherwise a buffer holding [small valid frame][giant malicious
    /// frame] would consume the small one and decode the giant one
    /// without checking its advertised total_length.
    fn try_decode(&mut self, provider_id: &str, label: FrameLabel) -> DecodeStep {
        if let Some(advertised) = self.advertised_len()
            && advertised > MAX_FRAME_BYTES
        {
            return DecodeStep::Fatal(Error::Streaming(label.cap_exceeded(advertised)));
        }
        let mut cursor = std::io::Cursor::new(self.buffer.as_ref());
        match self.decoder.decode_frame(&mut cursor) {
            Ok(DecodedFrame::Complete(message)) => {
                let Ok(consumed) = usize::try_from(cursor.position()) else {
                    return DecodeStep::Fatal(Error::Streaming(
                        label.consumed_overflow().to_string(),
                    ));
                };
                let _ = self.buffer.split_to(consumed);
                // smithy.decode_frame internally calls `self.reset()` on a
                // successful Complete, clearing its `prelude_read` flag.
                // Mirror that here so the cap check re-engages for the next
                // frame's prelude.
                self.smithy_has_prelude_buffered = false;
                DecodeStep::Frame(message)
            }
            Ok(DecodedFrame::Incomplete) => {
                // Drain whatever bytes smithy consumed before returning
                // Incomplete.
                //
                // `MessageFrameDecoder::decode_frame` reads the 12-byte
                // prelude into its internal state on first call and sets
                // `prelude_read = true`. It returns Incomplete because the
                // rest of the frame hasn't arrived. Without this drain, the
                // next attempt creates a fresh cursor at offset 0 -- but the
                // prelude bytes are still there. On re-entry smithy skips
                // re-reading the prelude (because `prelude_read=true`) and
                // reads the next bytes from cursor offset 0 as the headers
                // section -- which is actually still the prelude. The
                // big-endian `total_length` field gets interpreted as header
                // tag/value pairs, fails UTF-8 validation, and surfaces as
                // `InvalidUtf8String` on a frame whose payload is perfectly
                // valid.
                //
                // Symptom in the wild: any Bedrock streaming response that
                // arrives in multiple HTTP body chunks hits a mid-stream
                // UTF-8 error. Mirror smithy's prelude consumption back to
                // our `BytesMut` so the next attempt's fresh cursor starts
                // past the consumed prelude bytes.
                let consumed = cursor.position() as usize;
                if consumed > 0 {
                    let _ = self.buffer.split_to(consumed);
                    // smithy still has the prelude buffered internally: the
                    // cap check must skip until smithy returns Complete (and
                    // resets).
                    self.smithy_has_prelude_buffered = true;
                }
                DecodeStep::NeedMore
            }
            Err(e) => {
                let advertised = self.advertised_len().unwrap_or(0);
                log_decode_error(provider_id, label, &self.buffer, advertised, &e);
                DecodeStep::Fatal(Error::Streaming(label.decode_failed(&e)))
            }
        }
    }

    /// The truncation error for an upstream close, or `None` when the close
    /// is clean.
    ///
    /// Buffered bytes and a prelude staged inside smithy are both
    /// mid-frame truncations. Without the staged-prelude half, an upstream
    /// that closes after sending exactly 12 bytes (a prelude with no body)
    /// would report success: `buffer` is empty (the prelude was drained
    /// after the Incomplete return) but smithy still has an incomplete
    /// frame staged, and the router's circuit breaker would record a
    /// "successful" probe for an unhealthy upstream.
    fn eof_error(&self, label: FrameLabel) -> Option<Error> {
        if !self.buffer.is_empty() {
            Some(Error::Streaming(label.eof_buffered(self.buffer.len())))
        } else if self.smithy_has_prelude_buffered {
            Some(Error::Streaming(label.eof_prelude().to_string()))
        } else {
            None
        }
    }
}

/// Log a frame decode failure.
///
/// Log hygiene: the 12-byte prelude (always non-content: total_length,
/// headers_length, prelude_crc) is safe to emit at WARN for
/// diagnosability. The variable-length payload may carry model output and
/// must NOT go to a third-party log SaaS at WARN; it is gated at TRACE
/// instead.
fn log_decode_error(
    provider_id: &str,
    label: FrameLabel,
    buffer: &[u8],
    advertised: usize,
    err: &impl std::fmt::Display,
) {
    let prelude_len = 12_usize.min(buffer.len());
    tracing::warn!(
        provider = %provider_id,
        err = %err,
        frame_len = advertised,
        prelude_hex = %hex_bytes(&buffer[..prelude_len]),
        "{}",
        label.warn_decode_failed()
    );
    if tracing::enabled!(tracing::Level::TRACE) {
        let dump_len = advertised.min(256).min(buffer.len());
        tracing::trace!(
            provider = %provider_id,
            frame_len = advertised,
            hex = %hex_bytes(&buffer[..dump_len]),
            "{}",
            label.trace_dump()
        );
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The stream's final items once upstream stops producing bytes: the
/// read error, a truncation error, or -- only at a clean close -- the
/// handler's `on_eof` output. A clean close lets the handler flush any
/// state it held across frames (e.g. a captured stop_reason awaiting a
/// metadata frame that never arrived).
fn end_of_input_items<H: FrameHandler>(
    read_error: Option<reqwest::Error>,
    frames: &FrameBuffer,
    handler: &mut H,
    provider_id: &str,
    label: FrameLabel,
) -> Vec<Result<ChatChunk>> {
    if let Some(e) = read_error {
        return vec![Err(Error::Streaming(label.upstream_read(e)))];
    }
    if let Some(truncated) = frames.eof_error(label) {
        return vec![Err(truncated)];
    }
    match handler.on_eof(provider_id) {
        Ok(chunks) => chunks.into_iter().map(Ok).collect(),
        Err(e) => vec![Err(e)],
    }
}

/// Drive an AWS-eventstream byte stream through a per-provider handler,
/// emitting `ChatChunk`s. Partial frames buffer internally until enough
/// bytes arrive; the advertised-length DoS guard, prelude-tracking, and
/// decode-error handling all live here so both Bedrock egresses share one
/// hardened implementation.
///
/// Any frame decode or CRC failure ends the stream with
/// `Error::Streaming`. The eventstream has no resync marker and HTTP body
/// chunk boundaries do not align with frame boundaries, so after a bad
/// frame there is no trustworthy position to resume from.
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
        let mut frames = FrameBuffer::new();
        let mut byte_stream = Box::pin(byte_stream);
        loop {
            // Decode every complete frame already buffered.
            loop {
                match frames.try_decode(&provider_id, label) {
                    DecodeStep::Frame(message) => match handler.on_frame(&provider_id, message) {
                        Ok(chunks) => {
                            for c in chunks {
                                yield Ok(c);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    },
                    DecodeStep::NeedMore => break,
                    DecodeStep::Fatal(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }

            let read_error = match byte_stream.next().await {
                Some(Ok(bytes)) => {
                    frames.extend(&bytes);
                    continue;
                }
                Some(Err(e)) => Some(e),
                None => None,
            };
            for item in end_of_input_items(read_error, &frames, &mut handler, &provider_id, label) {
                yield item;
            }
            return;
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

    /// The in-stream access-denied WARN scrubs a principal from the
    /// payload message and keeps the clean text beside it.
    #[test]
    fn in_stream_auth_warn_redacts_a_principal_and_keeps_clean_text() {
        let payload = br#"{"message":"User: arn:aws:iam::123456789012:role/ops-role cannot invoke. SENTINEL-CLEAN-TEXT"}"#;
        for exception in ["accessDeniedException", "unauthorizedException"] {
            let events = routectl_testkit::capture_events(|| {
                let _ = exception_error("test", exception, payload);
            });

            // The explicit `message` field takes the event's message slot, so
            // the whole rendered event is checked.
            let warns: Vec<_> = events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN)
                .collect();
            assert_eq!(warns.len(), 1, "{exception}: {events:?}");
            let message = std::iter::once(warns[0].message.as_str())
                .chain(warns[0].fields.iter().map(|(_, v)| v.as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            assert!(!message.contains("arn:aws"), "{exception}: {message}");
            assert!(!message.contains("123456789012"), "{exception}: {message}");
            assert!(message.contains("[redacted]"), "{exception}: {message}");
            assert!(
                message.contains("SENTINEL-CLEAN-TEXT"),
                "{exception}: {message}"
            );
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
            fn on_eof(&mut self, _provider_id: &str) -> Result<Vec<ChatChunk>> {
                Ok(vec![ChatChunk::default()])
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

    /// An `on_eof` error at a graceful close is the stream's one and last
    /// item, after every chunk the frames produced.
    #[tokio::test]
    async fn on_eof_error_is_yielded_as_the_final_item() {
        struct EofErrorHandler;
        impl FrameHandler for EofErrorHandler {
            fn on_frame(
                &mut self,
                _provider_id: &str,
                _message: Message,
            ) -> Result<Vec<ChatChunk>> {
                Ok(vec![ChatChunk::default()])
            }
            fn on_eof(&mut self, provider_id: &str) -> Result<Vec<ChatChunk>> {
                Err(Error::upstream(provider_id, 0, "cut"))
            }
        }

        let buf = encode("contentBlockDelta", "{}");
        let byte_stream = futures::stream::iter(vec![Ok(Bytes::from(buf))]);
        let items: Vec<_> = decode_frames(
            "test".to_string(),
            byte_stream,
            EofErrorHandler,
            FrameLabel::Converse,
        )
        .collect()
        .await;

        assert_eq!(items.len(), 2, "items: {items:?}");
        assert!(items[0].is_ok(), "frame chunk first: {items:?}");
        match &items[1] {
            Err(Error::Upstream { status, body, .. }) => {
                assert_eq!(*status, 0);
                assert_eq!(body, "cut");
            }
            other => panic!("expected the on_eof error last, got {other:?}"),
        }
    }

    /// Drain `decode_frames` and return every item it yielded plus the
    /// event-types that reached the handler.
    async fn run_driver(
        chunks: Vec<Vec<u8>>,
        label: FrameLabel,
    ) -> (Vec<Result<ChatChunk>>, Vec<String>) {
        let byte_stream = futures::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))));
        let (tx, rx) = std::sync::mpsc::channel();
        let handler = RecordingHandler { tx };
        let items: Vec<_> = decode_frames("test".to_string(), byte_stream, handler, label)
            .collect()
            .await;
        (items, rx.try_iter().collect())
    }

    /// A frame whose message CRC is flipped: structurally intact, so the
    /// decoder reads it in full and fails on the checksum.
    fn encode_with_bad_crc(event_type: &str, payload_json: &str) -> Vec<u8> {
        let mut buf = encode(event_type, payload_json);
        let n = buf.len();
        buf[n - 4] ^= 0xFF;
        buf[n - 3] ^= 0xFF;
        buf
    }

    fn assert_single_decode_error(items: &[Result<ChatChunk>], label_fragment: &str) {
        assert_eq!(items.len(), 1, "expected exactly one item, got {items:?}");
        match &items[0] {
            Err(Error::Streaming(msg)) => {
                assert!(msg.contains("frame decode failed"), "got: {msg}");
                assert!(msg.contains(label_fragment), "label not applied: {msg}");
            }
            other => panic!("expected Streaming decode error, got {other:?}"),
        }
    }

    /// Corrupt continuation bytes after a prelude-only chunk fail the
    /// decode while the prelude is already held by the decoder. The stream
    /// must end with the error; the intact frame in the next chunk must
    /// never reach the handler.
    #[tokio::test]
    async fn decode_error_with_prelude_buffered_ends_stream() {
        let good = encode("messageStart", r#"{"role":"assistant"}"#);
        assert!(good.len() > 12, "frame must exceed its 12B prelude");
        let prelude = good[..12].to_vec();
        let corrupt_tail: Vec<u8> = good[12..].iter().map(|b| b ^ 0xFF).collect();
        let after = encode("messageStop", r#"{"stopReason":"end_turn"}"#);

        let (items, seen) =
            run_driver(vec![prelude, corrupt_tail, after], FrameLabel::Invoke).await;

        assert_single_decode_error(&items, "eventstream frame");
        assert!(seen.is_empty(), "no frame may reach the handler: {seen:?}");
    }

    /// A bad-CRC frame followed by a valid frame split mid-body across two
    /// chunks ends the stream at the bad frame; no part of the following
    /// frame is parsed or handed to the handler. One row per label because
    /// the label is the only lane-specific input to the driver.
    #[tokio::test]
    async fn bad_crc_frame_ends_stream_before_split_following_frame() {
        let cases = [
            ("invoke", FrameLabel::Invoke, "eventstream frame"),
            ("converse", FrameLabel::Converse, "converse-stream frame"),
        ];
        for (row, label, fragment) in cases {
            let bad = encode_with_bad_crc("contentBlockDelta", r#"{"text":"lost"}"#);
            let next = encode("contentBlockDelta", r#"{"text":"after"}"#);
            let (head, tail) = next.split_at(next.len() / 2);

            let (items, seen) = run_driver(vec![bad, head.to_vec(), tail.to_vec()], label).await;

            assert_single_decode_error(&items, fragment);
            assert!(
                seen.is_empty(),
                "row {row}: frame reached handler: {seen:?}"
            );
        }
    }

    /// Records every frame and each `on_eof` call, yields one chunk per
    /// frame, and fails on the frame whose event-type is `fail_on`.
    struct ExitProbe {
        fail_on: Option<&'static str>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        eof_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FrameHandler for ExitProbe {
        fn on_frame(&mut self, provider_id: &str, message: Message) -> Result<Vec<ChatChunk>> {
            let et = header_str(&message, ":event-type")
                .unwrap_or("")
                .to_string();
            self.seen.lock().expect("seen lock").push(et.clone());
            if self.fail_on == Some(et.as_str()) {
                return Err(Error::upstream(provider_id, 0, "handler refused frame"));
            }
            Ok(vec![ChatChunk::default()])
        }

        fn on_eof(&mut self, _provider_id: &str) -> Result<Vec<ChatChunk>> {
            self.eof_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    struct ExitOutcome {
        items: Vec<Result<ChatChunk>>,
        seen: Vec<String>,
        eof_calls: usize,
    }

    type Wire = Vec<std::result::Result<Bytes, reqwest::Error>>;

    async fn run_exit_probe(wire: Wire, fail_on: Option<&'static str>) -> ExitOutcome {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let eof_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler = ExitProbe {
            fail_on,
            seen: seen.clone(),
            eof_calls: eof_calls.clone(),
        };
        let items: Vec<_> = decode_frames(
            "test".to_string(),
            futures::stream::iter(wire),
            handler,
            FrameLabel::Invoke,
        )
        .collect()
        .await;
        let seen = seen.lock().expect("seen lock").clone();
        ExitOutcome {
            items,
            seen,
            eof_calls: eof_calls.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// A real `reqwest::Error` built without any network I/O: an
    /// unparseable URL fails at request build.
    fn transport_error() -> reqwest::Error {
        reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("an unparseable URL must not build a request")
    }

    /// Four bytes advertising one byte over the frame cap, padded past the
    /// 4-byte length read.
    fn oversized_prelude() -> Vec<u8> {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    fn ok_chunks(chunks: Vec<Vec<u8>>) -> Wire {
        chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect()
    }

    fn assert_frame_chunk_then_streaming_error(
        row: &str,
        items: &[Result<ChatChunk>],
        message_fragment: &str,
    ) {
        assert_eq!(items.len(), 2, "row {row}: items {items:?}");
        assert!(items[0].is_ok(), "row {row}: first frame chunk: {items:?}");
        match &items[1] {
            Err(Error::Streaming(msg)) => {
                assert!(msg.contains(message_fragment), "row {row}: got {msg}");
            }
            other => panic!("row {row}: expected Streaming error last, got {other:?}"),
        }
    }

    /// An oversized frame after an intact one ends the stream with the cap
    /// error after the first frame's chunk, whether the oversized prelude
    /// shares the intact frame's body chunk or arrives in a later one.
    #[tokio::test]
    async fn oversized_second_frame_ends_stream() {
        let first = encode("messageStart", r#"{"role":"assistant"}"#);
        let mut same_chunk = first.clone();
        same_chunk.extend_from_slice(&oversized_prelude());
        let after = encode("messageStop", r#"{"stopReason":"end_turn"}"#);
        let cases = [
            ("same_chunk", ok_chunks(vec![same_chunk, after.clone()])),
            (
                "next_chunk",
                ok_chunks(vec![first.clone(), oversized_prelude(), after.clone()]),
            ),
        ];
        for (row, wire) in cases {
            let out = run_exit_probe(wire, None).await;

            assert_frame_chunk_then_streaming_error(row, &out.items, "exceeds cap");
            assert_eq!(out.seen, vec!["messageStart".to_string()], "row {row}");
        }
    }

    /// EOF inside the prelude, right after the prelude, and inside the body
    /// each end the stream with exactly one truncation error after the
    /// chunks of every intact frame before it.
    #[tokio::test]
    async fn truncated_prelude_and_truncated_body_each_end_with_one_error() {
        let intact = encode("messageStart", r#"{"role":"assistant"}"#);
        let cut = encode("messageStop", r#"{"stopReason":"end_turn"}"#);
        assert!(cut.len() > 13, "frame must have a body past its prelude");
        let cases = [
            (
                "partial_prelude",
                cut[..7].to_vec(),
                "7 buffered bytes left at EOF",
            ),
            (
                "prelude_only",
                cut[..12].to_vec(),
                "prelude consumed but frame body never arrived",
            ),
            (
                "partial_body",
                cut[..cut.len() - 1].to_vec(),
                "buffered bytes left at EOF",
            ),
        ];
        for (row, tail, fragment) in cases {
            let out = run_exit_probe(ok_chunks(vec![intact.clone(), tail]), None).await;

            assert_frame_chunk_then_streaming_error(row, &out.items, fragment);
            assert_eq!(out.seen, vec!["messageStart".to_string()], "row {row}");
        }
    }

    /// A transport error ends the stream with the upstream-read error; a
    /// frame queued behind it is never read.
    #[tokio::test]
    async fn transport_error_is_terminal() {
        let wire: Wire = vec![
            Ok(Bytes::from(encode(
                "messageStart",
                r#"{"role":"assistant"}"#,
            ))),
            Err(transport_error()),
            Ok(Bytes::from(encode(
                "messageStop",
                r#"{"stopReason":"end_turn"}"#,
            ))),
        ];

        let out = run_exit_probe(wire, None).await;

        assert_frame_chunk_then_streaming_error(
            "transport",
            &out.items,
            "bedrock upstream byte read failed",
        );
        assert_eq!(out.seen, vec!["messageStart".to_string()]);
    }

    /// A handler error is yielded unchanged as the stream's last item; no
    /// later frame reaches the handler, even one already buffered.
    #[tokio::test]
    async fn handler_failure_is_terminal() {
        let mut combined = encode("messageStart", r#"{"role":"assistant"}"#);
        combined.extend_from_slice(&encode("contentBlockDelta", "{}"));
        combined.extend_from_slice(&encode("messageStop", r#"{"stopReason":"end_turn"}"#));

        let out = run_exit_probe(ok_chunks(vec![combined]), Some("contentBlockDelta")).await;

        assert_eq!(out.items.len(), 2, "items: {:?}", out.items);
        assert!(out.items[0].is_ok(), "first frame chunk: {:?}", out.items);
        match &out.items[1] {
            Err(Error::Upstream { status, body, .. }) => {
                assert_eq!(*status, 0);
                assert_eq!(body, "handler refused frame");
            }
            other => panic!("expected the handler error last, got {other:?}"),
        }
        assert_eq!(
            out.seen,
            vec!["messageStart".to_string(), "contentBlockDelta".to_string()]
        );
    }

    /// `on_eof` runs once at a clean close and never on any error exit.
    #[tokio::test]
    async fn on_eof_runs_exactly_once_per_exit() {
        let intact = encode("messageStart", r#"{"role":"assistant"}"#);
        let cut = encode("messageStop", r#"{"stopReason":"end_turn"}"#);
        let bad_crc = encode_with_bad_crc("contentBlockDelta", "{}");
        let cases: Vec<(&str, Wire, Option<&'static str>, usize)> = vec![
            ("clean_eof", ok_chunks(vec![intact.clone()]), None, 1),
            ("empty_stream", ok_chunks(Vec::new()), None, 1),
            (
                "partial_prelude",
                ok_chunks(vec![intact.clone(), cut[..7].to_vec()]),
                None,
                0,
            ),
            (
                "prelude_only",
                ok_chunks(vec![intact.clone(), cut[..12].to_vec()]),
                None,
                0,
            ),
            (
                "partial_body",
                ok_chunks(vec![intact.clone(), cut[..cut.len() - 1].to_vec()]),
                None,
                0,
            ),
            (
                "oversized",
                ok_chunks(vec![intact.clone(), oversized_prelude()]),
                None,
                0,
            ),
            (
                "decode_error",
                ok_chunks(vec![intact.clone(), bad_crc]),
                None,
                0,
            ),
            (
                "transport_error",
                vec![Ok(Bytes::from(intact.clone())), Err(transport_error())],
                None,
                0,
            ),
            (
                "handler_failure",
                ok_chunks(vec![intact.clone()]),
                Some("messageStart"),
                0,
            ),
        ];
        for (row, wire, fail_on, expected) in cases {
            let out = run_exit_probe(wire, fail_on).await;

            assert_eq!(out.eof_calls, expected, "row {row}: items {:?}", out.items);
        }
    }
}
