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
//! there is no in-stream config knob (architects' decision).
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

/// Maximum number of opaque delta events captured per unknown content
/// block. Same downgrade semantics as the byte cap.
pub(super) const MAX_OPAQUE_DELTAS_PER_BLOCK: usize = 10_000;

/// Per-block byte cap on the bounded opaque-event capture used when the
/// anthropic-api SSE state machine opens an unknown content block. Once
/// a block crosses this cap it degrades to sink-drain (capture stops;
/// canonical stream keeps flowing). 256 KB is generous for any
/// reasonable opaque content block while keeping per-stream memory
/// bounded against an adversarial upstream.
pub(super) const MAX_OPAQUE_BYTES_PER_BLOCK: usize = 256 * 1024;

/// Per-stream byte cap on total opaque-capture across ALL unknown
/// blocks in one streaming response. `SseState::pending_opaque` only
/// drains onto the next EMITTED canonical chunk, and a stream of only
/// unknown blocks emits none until `MessageStop`, so without this
/// ceiling the buffer grows with `block_count * MAX_OPAQUE_BYTES_PER_BLOCK`
/// unbounded. 4 MB is 16x the per-block cap: generous for any
/// legitimate multi-block server-tool response while bounding the
/// heap held by buffered `raw_data` / `raw_delta` against an
/// adversarial upstream. Once crossed the whole stream degrades to
/// sink-drain (see `SseState::open_unknown_block`).
pub(super) const MAX_TOTAL_OPAQUE_BYTES_PER_STREAM: usize = 4 * 1024 * 1024;

/// Per-stream cap on the NUMBER of captured opaque events (start +
/// delta sentinels) across all unknown blocks in one response. The
/// byte cap alone does not bound the many-tiny-blocks / many-tiny-deltas
/// case: millions of ~30-byte entries stay well under 4 MB of payload
/// yet blow the `Vec<OpaqueSseEvent>` entry overhead (each enum entry
/// is tens of bytes of stack plus its heap). 40_000 is 4x the per-block
/// delta cap (`MAX_OPAQUE_DELTAS_PER_BLOCK`) so a single legitimate
/// block that saturates its own delta cap -- plus several more normal
/// blocks -- never trips this per-stream backstop; a lower value (e.g.
/// below the per-block cap) would degrade a single lawful block. Only
/// start + delta captures are counted here; each block also emits one
/// uncounted stop sentinel, so the worst-case buffered entry count is
/// bounded by ~2x this value before the next drain. All three opaque
/// caps migrate to config together if an operator ever needs them
/// tunable; there is no in-stream knob today.
pub(super) const MAX_OPAQUE_EVENTS_PER_STREAM: usize = 40_000;

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
    /// True once the `ContentBlockStart` sentinel has been pushed. Gates
    /// the `ContentBlockStop` push so a block whose start was dropped
    /// (e.g. an oversized start payload that trips the byte cap before
    /// anything is buffered) never emits a start-less orphan stop.
    pub(super) start_emitted: bool,
}

impl OpaqueCapture {
    pub(super) const fn new(upstream_index: u32, type_tag: String) -> Self {
        Self {
            upstream_index,
            type_tag,
            bytes_so_far: 0,
            delta_count: 0,
            degraded: false,
            start_emitted: false,
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
        self.start_emitted = true;
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
    /// summary. The stop sentinel is appended only when the matching
    /// `ContentBlockStart` was actually pushed (`start_emitted`) -- a
    /// block whose start was dropped (e.g. an oversized start payload
    /// that tripped the byte cap before anything buffered) must NOT
    /// emit a start-less orphan stop. A block that DID emit its start
    /// always emits its stop, even when it later degraded: degrading
    /// stops capturing further DELTAS (see `record_delta`), not the
    /// stop boundary, so the consumer sees (start, ..., truncated
    /// deltas, stop) rather than an unclosed block. The INFO summary
    /// fires regardless so operators see a per-block rollup.
    pub(super) fn record_stop(&mut self, provider: &str, out: &mut Vec<OpaqueSseEvent>) {
        tracing::info!(
            provider = %provider,
            upstream_index = self.upstream_index,
            block_type = %self.type_tag,
            captured_bytes = self.bytes_so_far,
            delta_count = self.delta_count,
            start_emitted = self.start_emitted,
            "anthropic SSE: opaque block closed",
        );
        if self.start_emitted {
            out.push(OpaqueSseEvent::ContentBlockStop {
                upstream_index: self.upstream_index,
            });
        }
    }

    const fn would_overflow_bytes(&self, add: usize) -> bool {
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
                    "delta":{{"type":"citations_delta","seq":{i}}}
                }}"#,
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
    /// continues to flow normally past the cap, and the degraded
    /// block's stop sentinel is still emitted so the start/stop pair
    /// stays well-formed (only the post-cap deltas are dropped).
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
        // The degraded block's start was already emitted, so its stop
        // sentinel must still ride out to keep the (start, ..., stop)
        // pair well-formed. Degrading drops post-cap deltas, not the
        // stop boundary.
        let stop_count = final_chunk
            .opaque_events
            .iter()
            .filter(|e| matches!(e, OpaqueSseEvent::ContentBlockStop { .. }))
            .count();
        assert_eq!(
            stop_count, 1,
            "degraded block must still emit its stop sentinel to pair the start",
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
        // sums to ~300 KB, comfortably past the 256 KB default cap.
        let big_text: String = "x".repeat(1024);

        // Act
        for _ in 0..300 {
            let payload = format!(
                r#"{{
                    "type":"content_block_delta","index":0,
                    "delta":{{"type":"citations_delta","blob":"{big_text}"}}
                }}"#,
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

    // -----------------------------------------------------------------
    // Orphan ContentBlockStop guard (start_emitted)
    // -----------------------------------------------------------------

    /// A `content_block_start` payload that alone exceeds the per-block
    /// byte cap degrades the block immediately: no `ContentBlockStart`
    /// is captured. The matching `content_block_stop` must then NOT emit
    /// an orphan `ContentBlockStop` -- the captured sequence for that
    /// block is empty, not `[ContentBlockStop]` (which would be a
    /// start-less stop the client never saw a start for).
    #[test]
    fn oversized_start_emits_no_orphan_stop() {
        // Arrange -- a start payload comfortably past the 256 KB cap.
        let mut state = SseState::default();
        let big = "x".repeat(300 * 1024);
        let start = format!(
            r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"server_tool_use","blob":"{big}"}}}}"#,
        );

        // Act -- open (degrades on the oversized start), then close.
        let _ = state.parse_event("test", &start).unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // Drive a message_delta to flush any buffered opaque events.
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

        // Assert -- no orphan stop; the whole block was dropped from
        // capture because its start never rode out.
        assert!(
            chunk.opaque_events.is_empty(),
            "oversized start must not leave an orphan stop; got: {:?}",
            chunk.opaque_events,
        );
        assert!(
            state.pending_opaque.is_empty(),
            "no opaque events buffered for the dropped block",
        );
    }

    /// The inverse of the orphan-stop guard: a block whose start WAS
    /// emitted (within the cap) must still emit its stop so the pair
    /// stays well-formed.
    #[test]
    fn emitted_start_still_emits_its_stop() {
        // Arrange
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
            .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // Act -- flush.
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

        // Assert -- start + stop, well paired.
        assert_eq!(chunk.opaque_events.len(), 2, "start + stop");
        assert!(matches!(
            &chunk.opaque_events[0],
            OpaqueSseEvent::ContentBlockStart {
                upstream_index: 0,
                ..
            },
        ));
        assert!(matches!(
            &chunk.opaque_events[1],
            OpaqueSseEvent::ContentBlockStop { upstream_index: 0 },
        ));
    }

    // -----------------------------------------------------------------
    // Per-stream opaque memory bound (degrade-to-drop)
    // -----------------------------------------------------------------

    /// Upstream `index` carried by any opaque event.
    fn opaque_index(ev: &OpaqueSseEvent) -> u32 {
        match ev {
            OpaqueSseEvent::ContentBlockStart { upstream_index, .. }
            | OpaqueSseEvent::ContentBlockDelta { upstream_index, .. }
            | OpaqueSseEvent::ContentBlockStop { upstream_index } => *upstream_index,
            _ => u32::MAX,
        }
    }

    /// Drive a `message_delta` to flush `pending_opaque` onto a chunk.
    fn flush(state: &mut SseState) -> routectl_core::ChatChunk {
        state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta":{"stop_reason":"end_turn","stop_sequence":null},
                    "usage":{"output_tokens":1}
                }"#,
            )
            .unwrap()
            .expect("message_delta flushes a chunk")
    }

    /// Open unknown blocks and feed each a run of `citations_delta`
    /// deltas whose `blob` field is `blob_len` bytes, advancing to the
    /// next block whenever the current block degrades per-block, until
    /// the PER-STREAM cap trips. `blob_len` of a few KB trips the byte
    /// cap in a handful of blocks; `blob_len` of 0 keeps each delta tiny
    /// so the entry-count cap trips first. Returns the index of the
    /// block open when the stream degraded. Uses only `unwrap`, so any
    /// `Err` from `parse_event` would panic the test -- the trip must
    /// never surface as an error.
    fn trip_stream(state: &mut SseState, blob_len: usize) -> u32 {
        let blob = "x".repeat(blob_len);
        let mut idx = 0u32;
        loop {
            let start = format!(
                r#"{{"type":"content_block_start","index":{idx},"content_block":{{"type":"server_tool_use"}}}}"#,
            );
            let _ = state.parse_event("test", &start).unwrap();
            for _ in 0..10_001 {
                if state.opaque_stream_degraded {
                    break;
                }
                if state.current_capture.as_ref().is_none_or(|c| c.degraded) {
                    break;
                }
                let delta = format!(
                    r#"{{"type":"content_block_delta","index":{idx},"delta":{{"type":"citations_delta","blob":"{blob}"}}}}"#,
                );
                let _ = state.parse_event("test", &delta).unwrap();
            }
            let stop = format!(r#"{{"type":"content_block_stop","index":{idx}}}"#);
            let _ = state.parse_event("test", &stop).unwrap();
            if state.opaque_stream_degraded {
                return idx;
            }
            idx += 1;
            assert!(idx < 200, "per-stream cap must trip within 200 blocks");
        }
    }

    /// Crossing the per-stream BYTE cap before any canonical chunk has
    /// been emitted trips the sticky degrade flag, never aborts (all
    /// events parse Ok), and the stream keeps flowing afterward.
    #[test]
    fn stream_byte_cap_trips_degrade_never_aborts() {
        // Arrange + Act
        let mut state = SseState::default();
        assert!(!state.canonical_chunk_emitted, "no canonical chunk yet");
        trip_stream(&mut state, 4096);

        // Assert -- degraded on bytes, pre-canonical.
        assert!(state.opaque_stream_degraded, "stream must be degraded");
        assert!(
            state.opaque_bytes_total > super::MAX_TOTAL_OPAQUE_BYTES_PER_STREAM,
            "byte cap crossed, got {}",
            state.opaque_bytes_total,
        );
        assert!(
            !state.canonical_chunk_emitted,
            "trip landed before any canonical chunk",
        );

        // The stream continues cleanly past the trip: a normal
        // message_delta still emits its closing chunk.
        let chunk = flush(&mut state);
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "stream continues to its normal terminal chunk after the trip",
        );
    }

    /// The many-tiny-deltas case blows the ENTRY count, not the byte
    /// budget: with tiny deltas the per-stream event cap trips while
    /// total bytes stay under the byte cap.
    #[test]
    fn stream_event_cap_trips_on_entry_count() {
        // Arrange + Act -- tiny deltas (empty blob).
        let mut state = SseState::default();
        trip_stream(&mut state, 0);

        // Assert -- degraded via the event cap, not the byte cap.
        assert!(state.opaque_stream_degraded, "stream must be degraded");
        assert!(
            state.opaque_events_total > super::MAX_OPAQUE_EVENTS_PER_STREAM,
            "event cap crossed, got {}",
            state.opaque_events_total,
        );
        assert!(
            state.opaque_bytes_total <= super::MAX_TOTAL_OPAQUE_BYTES_PER_STREAM,
            "byte cap must NOT be the trip cause here, got {}",
            state.opaque_bytes_total,
        );
    }

    /// Pairing symmetry across the trip boundary: a block opened BEFORE
    /// the trip keeps both its start and stop; a block opened AFTER the
    /// trip emits neither.
    #[test]
    fn pairing_symmetry_across_trip_boundary() {
        // Arrange -- trip the stream; blocks 0..N are pre/at-trip. The
        // returned index is the block open when the cap tripped: its
        // start was buffered pre-trip and a later delta of the SAME
        // block crossed the cap -- the subtle boundary case.
        let mut state = SseState::default();
        let trip_idx = trip_stream(&mut state, 4096);
        assert!(state.opaque_stream_degraded);
        assert!(trip_idx > 0, "trip should span several blocks");

        // Act -- open a clearly post-trip block at a high index.
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_start","index":900,
                    "content_block":{"type":"server_tool_use"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"content_block_delta","index":900,
                    "delta":{"type":"citations_delta"}
                }"#,
            )
            .unwrap();
        let _ = state
            .parse_event("test", r#"{"type":"content_block_stop","index":900}"#)
            .unwrap();

        // Flush every buffered opaque event onto one chunk.
        let chunk = flush(&mut state);

        // Assert -- post-trip block 900 emits neither start nor stop.
        assert!(
            !chunk.opaque_events.iter().any(|e| opaque_index(e) == 900),
            "post-trip block must emit nothing (no start, no stop)",
        );
        // Pre-trip block 0 keeps a well-formed start/stop pair.
        assert!(
            chunk.opaque_events.iter().any(|e| matches!(
                e,
                OpaqueSseEvent::ContentBlockStart {
                    upstream_index: 0,
                    ..
                },
            )),
            "pre-trip block start must ride out",
        );
        assert!(
            chunk
                .opaque_events
                .iter()
                .any(|e| matches!(e, OpaqueSseEvent::ContentBlockStop { upstream_index: 0 },)),
            "pre-trip block stop must ride out to pair its start",
        );
        // The BOUNDARY block -- open when the cap tripped mid-delta --
        // must still emit BOTH its start (buffered pre-trip) and its
        // stop (record_stop runs because start_emitted is set). The
        // carried-risk case: a trip during a block's deltas must not
        // orphan that block's start.
        assert!(
            chunk.opaque_events.iter().any(|e| matches!(
                e,
                OpaqueSseEvent::ContentBlockStart { upstream_index, .. } if *upstream_index == trip_idx
            )),
            "boundary block start (buffered pre-trip) must ride out",
        );
        assert!(
            chunk.opaque_events.iter().any(|e| matches!(
                e,
                OpaqueSseEvent::ContentBlockStop { upstream_index } if *upstream_index == trip_idx
            )),
            "boundary block stop must ride out to pair its pre-trip start",
        );
    }

    /// Exactly ONE WARN fires at the trip, carrying provider,
    /// bytes/events at trip, and the canonical-chunk-emitted flag
    /// (false here -- trip before any canonical chunk). The trip
    /// produces no ERROR-level event, so classification / retry /
    /// fallback are untouched.
    #[test]
    fn stream_degrade_emits_exactly_one_warn_pre_canonical() {
        let events = routectl_testkit::capture_events(|| {
            let mut state = SseState::default();
            trip_stream(&mut state, 4096);
        });

        let trips: Vec<_> = events
            .iter()
            .filter(|e| e.message.contains("per-stream opaque-capture cap exceeded"))
            .collect();
        assert_eq!(trips.len(), 1, "exactly one per-stream trip WARN");
        let w = trips[0];
        assert_eq!(w.level, tracing::Level::WARN);
        assert_eq!(w.field("provider"), Some("test"));
        assert!(
            w.field("opaque_bytes_total").is_some(),
            "WARN carries bytes at trip",
        );
        assert!(
            w.field("opaque_events_total").is_some(),
            "WARN carries events at trip",
        );
        assert_eq!(
            w.field("canonical_chunk_emitted"),
            Some("false"),
            "trip before any canonical chunk",
        );
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::ERROR),
            "the trip must not surface as an error / classification event",
        );
    }

    /// When a canonical chunk WAS emitted before the trip, the single
    /// WARN reflects `canonical_chunk_emitted = true` (post-first-chunk
    /// fidelity loss). The degrade behavior itself is identical.
    #[test]
    fn stream_degrade_warn_reflects_prior_canonical_chunk() {
        let events = routectl_testkit::capture_events(|| {
            let mut state = SseState::default();
            // Emit a real canonical chunk first (text block).
            let _ = state
                .parse_event(
                    "test",
                    r#"{
                        "type":"content_block_start","index":0,
                        "content_block":{"type":"text","text":""}
                    }"#,
                )
                .unwrap();
            let c = state
                .parse_event(
                    "test",
                    r#"{
                        "type":"content_block_delta","index":0,
                        "delta":{"type":"text_delta","text":"hi"}
                    }"#,
                )
                .unwrap();
            assert!(c.is_some(), "text delta emits a canonical chunk");
            let _ = state
                .parse_event("test", r#"{"type":"content_block_stop","index":0}"#)
                .unwrap();
            assert!(state.canonical_chunk_emitted);
            trip_stream(&mut state, 4096);
        });

        let trips: Vec<_> = events
            .iter()
            .filter(|e| e.message.contains("per-stream opaque-capture cap exceeded"))
            .collect();
        assert_eq!(trips.len(), 1, "exactly one per-stream trip WARN");
        assert_eq!(
            trips[0].field("canonical_chunk_emitted"),
            Some("true"),
            "trip landed after a canonical chunk had been emitted",
        );
    }
}
