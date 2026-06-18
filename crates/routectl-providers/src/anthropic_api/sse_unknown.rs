//! Forward-compat handling for unknown Anthropic SSE content blocks
//! plus the content-block index invariant shared by every open-block
//! kind.
//!
//! Unknown content blocks: an unrecognized `content_block.type` opens
//! an `OpenBlockKind::Unknown` block. No canonical chunk is emitted at
//! the block's lifetime boundaries; its raw bytes are captured
//! opaquely (see `sse_opaque`) for verbatim re-emission by the
//! matching Anthropic ingress, and typed deltas that arrive inside it
//! are also routed through opaque capture so server-tool blocks whose
//! `input` streams via `input_json_delta` preserve their bytes.
//!
//! Index invariant: Anthropic guarantees `content_block_delta` and
//! `content_block_stop` events carry the same `index` as the
//! `content_block_start` that opened the block. We validate that
//! invariant across ALL open-block kinds: a mismatched event is
//! dropped (not applied to the wrong block) and logged, leaving the
//! open block untouched so the next in-order event still attributes
//! correctly.

use serde_json::{json, Value};

use super::sse::{OpenBlockKind, SseState};
use super::sse_opaque::OpaqueCapture;
use super::types::SseDelta;

/// Stable wire-ish tag for an open block kind, used in index-mismatch
/// WARN lines. Mirrors the Anthropic `content_block.type` vocabulary.
pub(super) fn block_type_name(open: &OpenBlockKind) -> &'static str {
    match open {
        OpenBlockKind::Text { .. } => "text",
        OpenBlockKind::Thinking { .. } => "thinking",
        OpenBlockKind::ToolUse { .. } => "tool_use",
        OpenBlockKind::Unknown { .. } => "unknown",
    }
}

impl SseState {
    /// Validate that `event_index` matches the currently-open block's
    /// `upstream_index`. Returns `true` when it matches (or no block
    /// is open -- nothing to validate against). On mismatch emits the
    /// standardized WARN and returns `false`; the caller drops the
    /// event WITHOUT touching `open_block`, so the next in-order
    /// event still attributes to the correct block.
    pub(super) fn index_matches(&self, event_index: u32, event_kind: &str, provider: &str) -> bool {
        let Some(open) = self.open_block.as_ref() else {
            return true;
        };
        let expected = open.upstream_index();
        if expected == event_index {
            return true;
        }
        tracing::warn!(
            provider = %provider,
            expected_index = expected,
            got_index = event_index,
            event_kind = %event_kind,
            open_block_type = block_type_name(open),
            "anthropic SSE: content-block index mismatch; dropping misattributed event",
        );
        false
    }

    /// Open an `OpenBlockKind::Unknown` block for an unrecognized
    /// `content_block.type` and seed opaque capture with the block's
    /// start payload. Emits the v2-capture WARN. No canonical chunk
    /// is produced at start; the matching ingress reconstructs the
    /// block from `chunk.opaque_events`.
    pub(super) fn open_unknown_block(&mut self, index: u32, value: &Value, provider: &str) {
        // Sanitize `type_tag` at capture time: `content_block.type` is
        // upstream-controlled and flows straight into tracing fields;
        // unsanitized CR, LF, or ANSI control sequences would corrupt
        // log output on a text subscriber.
        // Sanitizing here once means every downstream use -- the stored
        // OpenBlockKind field, the WARN log, and OpaqueCapture -- all
        // inherit the clean value without per-site guards.
        let type_tag = routectl_core::sanitize_for_log(
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        );
        self.open_block = Some(OpenBlockKind::Unknown {
            upstream_index: index,
            type_tag: type_tag.clone(),
        });
        tracing::warn!(
            provider = %provider,
            upstream_index = index,
            block_type = %type_tag,
            mode = "v2_capture",
            "anthropic SSE: opening forward-compat opaque content block",
        );
        let mut capture = OpaqueCapture::new(index, type_tag);
        capture.record_start(value, provider, &mut self.pending_opaque);
        self.current_capture = Some(capture);
    }

    /// Handle a `content_block_delta` while an `OpenBlockKind::Unknown`
    /// block is open. Capture every delta variant -- including typed
    /// deltas -- through the opaque capture path so a server-tool block
    /// streaming its `input` via `input_json_delta` (or any other typed
    /// delta inside an unknown block) preserves its bytes for verbatim
    /// re-emission. Always yields no canonical chunk.
    pub(super) fn capture_unknown_delta(&mut self, delta: &SseDelta, provider: &str) {
        let raw_value: Value = match delta {
            SseDelta::Other(v) => v.clone(),
            SseDelta::TextDelta { text } => json!({"type": "text_delta", "text": text}),
            SseDelta::InputJsonDelta { partial_json } => {
                json!({"type": "input_json_delta", "partial_json": partial_json})
            }
            SseDelta::ThinkingDelta { thinking } => {
                json!({"type": "thinking_delta", "thinking": thinking})
            }
            SseDelta::SignatureDelta { signature } => {
                json!({"type": "signature_delta", "signature": signature})
            }
        };
        if let Some(capture) = self.current_capture.as_mut() {
            capture.record_delta(&raw_value, provider, &mut self.pending_opaque);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::sse::SseState;

    // -----------------------------------------------------------------
    // OpenBlockKind::Unknown lifecycle + sink-drain
    // -----------------------------------------------------------------

    /// An unknown block lifecycle (start / delta / stop) emits no
    /// canonical chunk: the entire block is consumed opaquely and the
    /// downstream `ChunkDelta` shape is unchanged.
    #[test]
    fn unknown_block_open_emits_no_canonical_chunk() {
        // Arrange
        let mut state = SseState::default();

        // Act + Assert
        let r1 = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use","id":"x","input":{}}
                }"#,
            )
            .unwrap();
        let r2 = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":0,
                    "delta":{"type":"citations_delta"}
                }"#,
            )
            .unwrap();
        let r3 = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        assert!(r1.is_none(), "start emits nothing canonical");
        assert!(r2.is_none(), "delta emits nothing canonical");
        assert!(r3.is_none(), "stop emits nothing canonical");
    }

    /// A subsequent known block (text) opens cleanly after an unknown
    /// block closes; canonical chunks attribute to the new block at
    /// its own upstream index.
    #[test]
    fn unknown_block_open_then_known_text_block_works_normally() {
        // Arrange -- unknown block at index 0, text block at index 1.
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":1,
                    "content_block":{"type":"text","text":""}
                }"#,
            )
            .unwrap();

        // Act
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":1,
                    "delta":{"type":"text_delta","text":"hello"}
                }"#,
            )
            .unwrap()
            .expect("text delta emits a canonical chunk");

        // Assert
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
    }

    /// An unknown block in the middle of a stream must not perturb
    /// the message-level usage accounting that runs alongside it.
    #[test]
    fn unknown_block_does_not_affect_message_delta_usage() {
        // Arrange
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message":{
                        "id":"m1","type":"message","role":"assistant",
                        "content":[],"model":"claude",
                        "stop_reason":null,"stop_sequence":null,
                        "usage":{"input_tokens":50,"output_tokens":0}
                    }
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // Act
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta":{"stop_reason":"end_turn","stop_sequence":null},
                    "usage":{"output_tokens":7}
                }"#,
            )
            .unwrap()
            .expect("message_delta still emits");

        // Assert
        let usage = chunk.usage.expect("usage on closing chunk");
        assert_eq!(usage.prompt_tokens, Some(50));
        assert_eq!(usage.completion_tokens, Some(7));
    }

    // -----------------------------------------------------------------
    // Index-invariant validation across ALL OpenBlockKind variants
    // -----------------------------------------------------------------

    /// Matching delta index against the open block emits the canonical
    /// chunk -- the happy path.
    #[test]
    fn delta_index_match_emits_chunk() {
        // Arrange
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"text","text":""}
                }"#,
            )
            .unwrap();

        // Act
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":0,
                    "delta":{"type":"text_delta","text":"hi"}
                }"#,
            )
            .unwrap()
            .expect("matching delta emits");

        // Assert
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }

    /// A delta whose index disagrees with the open block is dropped
    /// (not misattributed). Open block is left intact so the next
    /// well-formed delta still emits.
    #[test]
    fn delta_index_mismatch_drops_event_and_warns() {
        // Arrange -- text block at index 0.
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"text","text":""}
                }"#,
            )
            .unwrap();

        // Act -- delta arrives with a stale index.
        let dropped = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":1,
                    "delta":{"type":"text_delta","text":"X"}
                }"#,
            )
            .unwrap();

        // Assert -- dropped, open block untouched.
        assert!(dropped.is_none(), "mismatched delta dropped");
        assert_eq!(
            state.open_block.as_ref().map(|b| b.upstream_index()),
            Some(0),
            "open block must remain at its original index",
        );

        // A subsequent well-formed delta still emits.
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":0,
                    "delta":{"type":"text_delta","text":"hi"}
                }"#,
            )
            .unwrap()
            .expect("next valid delta still emits");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }

    /// Stop events with a mismatched index are dropped without
    /// closing the open block.
    #[test]
    fn stop_index_mismatch_drops_event() {
        // Arrange
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"text","text":""}
                }"#,
            )
            .unwrap();

        // Act
        let r = state
            .parse_event("test", r#"{"type":"content_block_stop","index":1}"#)
            .unwrap();

        // Assert
        assert!(r.is_none(), "mismatched stop dropped");
        assert_eq!(
            state.open_block.as_ref().map(|b| b.upstream_index()),
            Some(0),
            "open block must remain open",
        );
    }

    /// The Unknown variant participates in the same index-validation
    /// invariant as the typed variants.
    #[test]
    fn unknown_variant_participates_in_index_validation() {
        // Arrange -- unknown block at index 0.
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use"}
                }"#,
            )
            .unwrap();

        // Act -- delta arrives with a mismatched index.
        let r = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":1,
                    "delta":{"type":"citations_delta"}
                }"#,
            )
            .unwrap();

        // Assert
        assert!(r.is_none(), "mismatched delta dropped on unknown block");
        assert_eq!(
            state.open_block.as_ref().map(|b| b.upstream_index()),
            Some(0),
        );
    }

    // -----------------------------------------------------------------
    // Log-injection: sanitize upstream-controlled type_tag at capture
    // -----------------------------------------------------------------

    /// An upstream-controlled `content_block.type` containing CR, LF, or
    /// ANSI escape sequences must be sanitized before it is stored in
    /// `OpenBlockKind::Unknown.type_tag` and before it reaches any tracing
    /// field. Verifies the fix: sanitize at capture time (once) so all
    /// downstream log sites inherit the clean value.
    #[test]
    fn open_unknown_block_type_tag_is_sanitized_before_logging() {
        use super::super::sse::OpenBlockKind;

        // Arrange: a content_block_start whose type tag embeds CRLF and
        // an ANSI escape sequence -- unsanitized control sequences would
        // corrupt log output on a text-format tracing subscriber.
        let mut state = SseState::default();

        // Act: open the unknown block via parse_event.
        let _ = state
            .parse_event(
                "test",
                // The type field contains \n (newline), \r (carriage return),
                // and \x1b (ESC for ANSI). We embed them via JSON unicode
                // escapes so the test source stays ASCII-only.
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"evil\nfake_line\r\u001b[31mred"}}"#,
            )
            .unwrap();

        // Assert: the type_tag stored in OpenBlockKind::Unknown is sanitized.
        // Since block_type = %type_tag in the WARN log reads from the same
        // stored string, a clean stored value means a clean logged field.
        let stored_tag = match state.open_block.as_ref().expect("block must be open") {
            OpenBlockKind::Unknown { type_tag, .. } => type_tag.clone(),
            other => panic!("expected Unknown block, got: {other:?}"),
        };
        assert!(
            !stored_tag.contains('\n'),
            "stored type_tag must not contain newline (CR/LF injection); got: {stored_tag:?}"
        );
        assert!(
            !stored_tag.contains('\r'),
            "stored type_tag must not contain carriage return; got: {stored_tag:?}"
        );
        assert!(
            !stored_tag.contains('\x1b'),
            "stored type_tag must not contain ESC (ANSI injection); got: {stored_tag:?}"
        );
        // The placeholder character `?` must appear where the injected bytes
        // were so operators can see that filtering occurred.
        assert!(
            stored_tag.contains('?'),
            "sanitized type_tag must contain placeholder `?` characters; got: {stored_tag:?}"
        );
    }

    /// A typed `input_json_delta` arriving inside an unknown block must
    /// be captured opaquely so a server-tool block whose input streams
    /// non-trivially preserves its bytes for verbatim re-emission. Pin
    /// the contract: the next emitted chunk's `opaque_events` carries a
    /// `ContentBlockDelta` whose `raw_delta` round-trips to the original
    /// `input_json_delta` JSON.
    #[test]
    fn input_json_delta_inside_unknown_block_captured_opaquely() {
        use routectl_core::OpaqueSseEvent;
        use serde_json::Value;

        // Arrange -- open an unknown block, feed an input_json_delta,
        // close, then drive a message_delta to flush pending_opaque.
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{}}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":"{\"q\":\"hi\"}"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // Act
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta":{"stop_reason":"end_turn","stop_sequence":null},
                    "usage":{"output_tokens":1}
                }"#,
            )
            .unwrap()
            .expect("message_delta emits a chunk");

        // Assert -- find the captured ContentBlockDelta and round-trip
        // its raw_delta to the input_json_delta JSON.
        let delta_event = chunk
            .opaque_events
            .iter()
            .find(|e| matches!(e, OpaqueSseEvent::ContentBlockDelta { .. }))
            .expect("input_json_delta inside unknown block must be captured");
        match delta_event {
            OpaqueSseEvent::ContentBlockDelta {
                upstream_index,
                raw_delta,
            } => {
                assert_eq!(*upstream_index, 0);
                let parsed: Value =
                    serde_json::from_slice(raw_delta).expect("raw_delta round-trips as JSON");
                assert_eq!(
                    parsed.get("type").and_then(Value::as_str),
                    Some("input_json_delta"),
                );
                assert_eq!(
                    parsed.get("partial_json").and_then(Value::as_str),
                    Some("{\"q\":\"hi\"}"),
                );
            }
            other => panic!("expected ContentBlockDelta, got: {other:?}"),
        }
    }
}
