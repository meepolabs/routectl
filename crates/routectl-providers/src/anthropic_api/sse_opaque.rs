//! Bounded opaque-event capture for unknown Anthropic SSE content
//! blocks.
//!
//! When the SSE state machine opens an `OpenBlockKind::Unknown` block
//! (an unrecognized `content_block.type` such as `server_tool_use` or
//! `web_search_tool_result`) the raw upstream-wire bytes for each
//! `content_block_*` event are captured into
//! `routectl_core::OpaqueSseEvent` values and buffered on
//! `SseState::pending_opaque` for verbatim re-emission by the
//! matching Anthropic ingress.
//!
//! Capture is bounded per block: once a block crosses
//! `MAX_OPAQUE_BYTES_PER_BLOCK` or `MAX_OPAQUE_DELTAS_PER_BLOCK` it
//! transitions to a "degraded" sink-drain mode -- subsequent events
//! on that block are dropped from capture while the canonical stream
//! keeps flowing. The bounded-capture downgrade IS the kill switch;
//! there is no config knob (architects' decision).
//!
//! Fidelity note: `record_start` and `record_delta` re-serialize the
//! held `serde_json::Value` to bytes rather than preserving the exact
//! upstream byte slice. The eventsource decoder hands us the parsed
//! Value, not the original byte range, so a perfect byte-for-byte
//! capture is not achievable here. serde_json round-trips valid JSON
//! cleanly, so this is a small, lossless-for-valid-input fidelity
//! gap; downstream consumers that need exact-byte parity must intercept
//! at the byte stream layer instead.

use routectl_core::OpaqueSseEvent;
use serde_json::Value;

/// Maximum total opaque bytes captured per unknown content block.
/// Sum of the start payload plus every captured delta payload. Once
/// crossed, the block degrades to sink-drain for the remainder of its
/// life.
pub(super) const MAX_OPAQUE_BYTES_PER_BLOCK: usize = 256 * 1024;

/// Maximum number of opaque delta events captured per unknown content
/// block. Same downgrade semantics as the byte cap.
pub(super) const MAX_OPAQUE_DELTAS_PER_BLOCK: usize = 10_000;

/// Per-block running totals for the bounded opaque-capture state.
/// One instance lives on `SseState::current_capture` while an
/// `OpenBlockKind::Unknown` block is open; cleared at
/// `content_block_stop`.
#[derive(Debug)]
pub(super) struct OpaqueCapture {
    pub(super) upstream_index: u32,
    pub(super) type_tag: String,
    pub(super) bytes_so_far: usize,
    pub(super) delta_count: usize,
    /// Set once a cap is exceeded; subsequent events skip capture.
    pub(super) degraded: bool,
}

impl OpaqueCapture {
    pub(super) fn new(upstream_index: u32, type_tag: String) -> Self {
        Self {
            upstream_index,
            type_tag,
            bytes_so_far: 0,
            delta_count: 0,
            degraded: false,
        }
    }

    /// Capture the `content_block_start` payload. `value` is the inner
    /// `content_block` object (carries the `type` tag plus
    /// shape-specific fields). A pathologically large start payload
    /// can itself trip the byte cap; we treat that as an immediate
    /// degrade so we never buffer an oversized blob.
    pub(super) fn record_start(
        &mut self,
        value: &Value,
        provider: &str,
        out: &mut Vec<OpaqueSseEvent>,
    ) {
        let raw_data = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(
                    provider = %provider,
                    upstream_index = self.upstream_index,
                    error = %err,
                    "anthropic SSE: failed to re-serialize opaque block start; skipping event",
                );
                return;
            }
        };
        if self.would_overflow_bytes(raw_data.len()) {
            self.degrade(provider, "byte_overflow");
            return;
        }
        self.bytes_so_far = self.bytes_so_far.saturating_add(raw_data.len());
        out.push(OpaqueSseEvent::ContentBlockStart {
            upstream_index: self.upstream_index,
            type_tag: self.type_tag.clone(),
            raw_data,
        });
    }

    /// Capture one `content_block_delta` payload. `value` is the inner
    /// `delta` object. Enforces both caps; emits a DEBUG line per
    /// captured delta so operators can reconstruct fidelity post-hoc
    /// without paying the INFO budget.
    pub(super) fn record_delta(
        &mut self,
        value: &Value,
        provider: &str,
        out: &mut Vec<OpaqueSseEvent>,
    ) {
        if self.degraded {
            return;
        }
        let raw_delta = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(
                    provider = %provider,
                    upstream_index = self.upstream_index,
                    error = %err,
                    "anthropic SSE: failed to re-serialize opaque delta; skipping event",
                );
                return;
            }
        };
        let byte_overflow = self.would_overflow_bytes(raw_delta.len());
        let delta_overflow = self.delta_count.saturating_add(1) > MAX_OPAQUE_DELTAS_PER_BLOCK;
        if byte_overflow || delta_overflow {
            let reason = if byte_overflow {
                "byte_overflow"
            } else {
                "delta_overflow"
            };
            self.degrade(provider, reason);
            return;
        }
        let delta_bytes = raw_delta.len();
        self.bytes_so_far = self.bytes_so_far.saturating_add(delta_bytes);
        self.delta_count = self.delta_count.saturating_add(1);
        out.push(OpaqueSseEvent::ContentBlockDelta {
            upstream_index: self.upstream_index,
            raw_delta,
        });
        tracing::debug!(
            provider = %provider,
            upstream_index = self.upstream_index,
            delta_bytes,
            "anthropic SSE: captured opaque delta",
        );
    }

    /// Capture the `content_block_stop` sentinel and emit the INFO
    /// summary. The summary fires regardless of degraded state so
    /// operators see a per-block rollup; the stop sentinel is appended
    /// to capture only when not degraded so the consumer only sees
    /// well-paired (start, ..., stop) sequences.
    pub(super) fn record_stop(&mut self, provider: &str, out: &mut Vec<OpaqueSseEvent>) {
        tracing::info!(
            provider = %provider,
            upstream_index = self.upstream_index,
            block_type = %self.type_tag,
            captured_bytes = self.bytes_so_far,
            delta_count = self.delta_count,
            "anthropic SSE: opaque block closed",
        );
        if self.degraded {
            return;
        }
        out.push(OpaqueSseEvent::ContentBlockStop {
            upstream_index: self.upstream_index,
        });
    }

    fn would_overflow_bytes(&self, add: usize) -> bool {
        self.bytes_so_far.saturating_add(add) > MAX_OPAQUE_BYTES_PER_BLOCK
    }

    fn degrade(&mut self, provider: &str, reason: &str) {
        self.degraded = true;
        tracing::warn!(
            provider = %provider,
            upstream_index = self.upstream_index,
            block_type = %self.type_tag,
            reason = %reason,
            captured_bytes = self.bytes_so_far,
            delta_count = self.delta_count,
            "anthropic SSE: opaque-capture cap exceeded; degrading block to sink-drain",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::sse::SseState;
    use routectl_core::OpaqueSseEvent;
    use serde_json::Value;

    // -----------------------------------------------------------------
    // Egress capture into ChatChunk.opaque_events
    // -----------------------------------------------------------------

    /// Open an unknown content block, feed five unknown deltas, close
    /// it, then drive a `message_delta` to flush the buffer. The
    /// emitted chunk's `opaque_events` must carry exactly seven
    /// entries -- one start, five deltas, one stop -- with the inner
    /// payload bytes recoverable verbatim.
    #[test]
    fn unknown_block_captures_start_delta_stop_into_opaque_events() {
        // Arrange
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{
                        "type":"server_tool_use",
                        "id":"srv_01",
                        "input":{"q":"hi"}
                    }
                }"#,
            )
            .unwrap();
        for i in 0..5_i32 {
            let payload = format!(
                r#"{{
                    "type":"content_block_delta","index":0,
                    "delta":{{"type":"citations_delta","seq":{}}}
                }}"#,
                i,
            );
            let _ = state.parse_event("test", &payload).unwrap();
        }
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // Act -- force emission so pending_opaque drains.
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

        // Assert
        assert_eq!(
            chunk.opaque_events.len(),
            7,
            "1 start + 5 deltas + 1 stop = 7",
        );
        match &chunk.opaque_events[0] {
            OpaqueSseEvent::ContentBlockStart {
                upstream_index,
                type_tag,
                raw_data,
            } => {
                assert_eq!(*upstream_index, 0);
                assert_eq!(type_tag, "server_tool_use");
                let parsed: Value = serde_json::from_slice(raw_data).expect("raw_data is JSON");
                assert_eq!(parsed["type"], "server_tool_use");
                assert_eq!(parsed["id"], "srv_01");
                assert_eq!(parsed["input"]["q"], "hi");
            }
            other => panic!("expected ContentBlockStart, got: {other:?}"),
        }
        for (i, ev) in chunk.opaque_events[1..6].iter().enumerate() {
            match ev {
                OpaqueSseEvent::ContentBlockDelta {
                    upstream_index,
                    raw_delta,
                } => {
                    assert_eq!(*upstream_index, 0);
                    let parsed: Value =
                        serde_json::from_slice(raw_delta).expect("raw_delta is JSON");
                    assert_eq!(parsed["type"], "citations_delta");
                    assert_eq!(parsed["seq"], i as i64);
                }
                other => panic!("expected ContentBlockDelta at {i}, got: {other:?}"),
            }
        }
        assert!(matches!(
            &chunk.opaque_events[6],
            OpaqueSseEvent::ContentBlockStop { upstream_index: 0 },
        ));
    }

    /// After a chunk is emitted carrying drained opaque events,
    /// `pending_opaque` must be empty so the same events do not ride
    /// out a second time on the next emission.
    #[test]
    fn opaque_events_drain_on_next_emission() {
        // Arrange -- start, one delta, stop (3 buffered events).
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":0,
                    "content_block":{"type":"server_tool_use","id":"x"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":0,
                    "delta":{"type":"citations_delta"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();
        assert_eq!(state.pending_opaque.len(), 3, "3 events buffered pre-drain");

        // Act -- emit a chunk via message_delta to drain.
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
            .expect("emit");

        // Assert
        assert_eq!(chunk.opaque_events.len(), 3, "drained into chunk");
        assert!(state.pending_opaque.is_empty(), "buffer drained");
    }

    // -----------------------------------------------------------------
    // Bounded caps with per-block downgrade
    // -----------------------------------------------------------------

    /// Feed 10001 unknown deltas. Exactly 10000 must be captured into
    /// opaque_events; the 10001st event triggers the per-block
    /// downgrade and is dropped from capture. The stream itself
    /// continues to flow normally past the cap.
    #[test]
    fn delta_overflow_triggers_downgrade_and_warn() {
        // Arrange
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

        // Act -- 10001 unknown deltas (one past the cap).
        for _ in 0..10_001 {
            let _ = state
                .parse_event(
                    "test",
                    r#"{
                        "type":"content_block_delta","index":0,
                        "delta":{"type":"citations_delta"}
                    }"#,
                )
                .unwrap();
        }

        // Assert -- capture saturated at 10000, degraded.
        let cap = state
            .current_capture
            .as_ref()
            .expect("capture still present");
        assert!(cap.degraded, "capture must be degraded after the 10001st");
        assert_eq!(
            cap.delta_count,
            super::MAX_OPAQUE_DELTAS_PER_BLOCK,
            "exactly 10000 captured",
        );
        // pending_opaque has 1 start + 10000 deltas = 10001 entries.
        assert_eq!(state.pending_opaque.len(), 10_001);

        // Stream continues: stop + message_delta still flow cleanly.
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();
        let final_chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta":{"stop_reason":"end_turn","stop_sequence":null},
                    "usage":{"output_tokens":1}
                }"#,
            )
            .unwrap()
            .expect("stream continues past overflow");
        // Stop sentinel was suppressed under degraded mode.
        let stop_count = final_chunk
            .opaque_events
            .iter()
            .filter(|e| matches!(e, OpaqueSseEvent::ContentBlockStop { .. }))
            .count();
        assert_eq!(
            stop_count, 0,
            "degraded block must not emit a stop sentinel into capture",
        );
    }

    /// Feed deltas summing to more than 256 KB. Capture must stop at
    /// the byte cap (`captured_bytes <= MAX_OPAQUE_BYTES_PER_BLOCK`)
    /// and the block must transition to degraded.
    #[test]
    fn byte_overflow_triggers_downgrade_and_warn() {
        // Arrange
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
        // Each delta payload is roughly 1 KB; feeding 300 of them
        // sums to ~300 KB, comfortably past the 256 KB cap.
        let big_text: String = "x".repeat(1024);

        // Act
        for _ in 0..300 {
            let payload = format!(
                r#"{{
                    "type":"content_block_delta","index":0,
                    "delta":{{"type":"citations_delta","blob":"{}"}}
                }}"#,
                big_text,
            );
            let _ = state.parse_event("test", &payload).unwrap();
        }

        // Assert
        let cap = state.current_capture.as_ref().expect("capture present");
        assert!(cap.degraded, "byte overflow must degrade");
        assert!(
            cap.bytes_so_far <= super::MAX_OPAQUE_BYTES_PER_BLOCK,
            "captured bytes stay under the cap, got {}",
            cap.bytes_so_far,
        );
        assert!(
            !state.pending_opaque.is_empty(),
            "some deltas captured before the cap kicked in",
        );
    }
}
