//! Replay of the opaque content-block events the Anthropic-API egress
//! captured, onto the client stream with ingress-allocated indexes.

use routectl_core::{OpaqueSseEvent, sanitize_for_log};

use crate::ingress::SseEvent;

use super::AnthropicStreamState;
use super::stream::close_open_block;

/// Replay opaque SSE events captured by the Anthropic-API egress.
///
/// Each event carries the captured upstream bytes for an unknown
/// `content_block` type (e.g. `server_tool_use`,
/// `web_search_tool_result`). We re-emit those bytes as the SSE
/// `data:` payload's `content_block` / `delta` field. The result is
/// value-preserving (semantically lossless for valid JSON) rather
/// than literally byte-for-byte: the egress already captured the
/// parsed `serde_json::Value`, so the re-emitted bytes are a clean
/// re-serialization, not the exact upstream byte slice.
///
/// Index allocation: each `ContentBlockStart` consumes one fresh
/// `state.next_index` value and records the upstream->ingress mapping
/// in `state.opaque_index_map`. Subsequent `ContentBlockDelta` /
/// `ContentBlockStop` for the same `upstream_index` look up the
/// recorded ingress index. `ContentBlockStop` clears the entry.
///
/// A canonical block already open (text / thinking) is closed first
/// so the wire never has two block_starts without an intervening
/// block_stop. The egress only attaches opaque events at boundaries,
/// so this is defensive in practice.
///
/// Errors on a single event are logged at WARN and skipped; the
/// stream continues. The replay path MUST NOT terminate the stream
/// because canonical block emission is the authoritative half.
pub(super) fn replay_opaque_events(
    opaque_events: &[OpaqueSseEvent],
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    for ev in opaque_events {
        match ev {
            OpaqueSseEvent::ContentBlockStart {
                upstream_index,
                type_tag,
                raw_data,
            } => {
                close_open_block(state, events);
                let ingress_index = state.next_index;
                state.next_index += 1;
                if let Some(old_idx) = state
                    .opaque_index_map
                    .insert(*upstream_index, ingress_index)
                {
                    // Duplicate upstream_index ContentBlockStart before
                    // the prior stop -- egress bug or upstream
                    // malformation. Emit a content_block_stop for the
                    // orphaned block so a strict client never sees an
                    // unclosed block; log a WARN so the upstream's wire
                    // bug is visible to operators.
                    tracing::warn!(
                        provider = "anthropic",
                        upstream_index = *upstream_index,
                        old_ingress_index = old_idx,
                        "anthropic ingress: duplicate opaque ContentBlockStart; \
                         emitting stop for orphaned block",
                    );
                    events.push(SseEvent::named(
                        "content_block_stop",
                        format!("{{\"type\":\"content_block_stop\",\"index\":{old_idx}}}"),
                    ));
                }
                if let Some(data) = build_opaque_start_payload(ingress_index, raw_data) {
                    events.push(SseEvent::named("content_block_start", data));
                } else {
                    tracing::warn!(
                        provider = "anthropic",
                        upstream_index = *upstream_index,
                        type_tag = %sanitize_for_log(type_tag),
                        "anthropic ingress: skipping opaque content_block_start; \
                         non-utf8 raw_data bytes",
                    );
                }
            }
            OpaqueSseEvent::ContentBlockDelta {
                upstream_index,
                raw_delta,
            } => {
                let Some(&ingress_index) = state.opaque_index_map.get(upstream_index) else {
                    tracing::warn!(
                        provider = "anthropic",
                        upstream_index = *upstream_index,
                        "anthropic ingress: opaque content_block_delta with no prior \
                         content_block_start; skipping",
                    );
                    continue;
                };
                if let Some(data) = build_opaque_delta_payload(ingress_index, raw_delta) {
                    events.push(SseEvent::named("content_block_delta", data));
                } else {
                    tracing::warn!(
                        provider = "anthropic",
                        upstream_index = *upstream_index,
                        "anthropic ingress: skipping opaque content_block_delta; \
                         non-utf8 raw_delta bytes",
                    );
                }
            }
            OpaqueSseEvent::ContentBlockStop { upstream_index } => {
                let Some(ingress_index) = state.opaque_index_map.remove(upstream_index) else {
                    tracing::warn!(
                        provider = "anthropic",
                        upstream_index = *upstream_index,
                        "anthropic ingress: opaque content_block_stop with no prior \
                         content_block_start; skipping",
                    );
                    continue;
                };
                events.push(SseEvent::named(
                    "content_block_stop",
                    format!("{{\"type\":\"content_block_stop\",\"index\":{ingress_index}}}"),
                ));
            }
            // OpaqueSseEvent is `#[non_exhaustive]`. Future variants
            // ship without a code edit here; until this match learns
            // them, log + skip rather than panic. The canonical block
            // path is unaffected.
            _ => {
                tracing::warn!(
                    provider = "anthropic",
                    "anthropic ingress: unknown OpaqueSseEvent variant; skipping",
                );
            }
        }
    }
}

/// Compose the SSE `data:` payload for an opaque
/// `content_block_start`. Embeds `raw_bytes` verbatim as the
/// `content_block` value. Returns `None` if `raw_bytes` is not
/// valid UTF-8 (Anthropic SSE is JSON-over-UTF-8 by spec; this is
/// a defensive guard, not an expected path).
fn build_opaque_start_payload(ingress_index: usize, raw_bytes: &[u8]) -> Option<String> {
    let raw_str = std::str::from_utf8(raw_bytes).ok()?;
    Some(format!(
        "{{\"type\":\"content_block_start\",\"index\":{ingress_index},\"content_block\":{raw_str}}}",
    ))
}

/// Compose the SSE `data:` payload for an opaque
/// `content_block_delta`. Embeds `raw_bytes` verbatim as the
/// `delta` value.
fn build_opaque_delta_payload(ingress_index: usize, raw_bytes: &[u8]) -> Option<String> {
    let raw_str = std::str::from_utf8(raw_bytes).ok()?;
    Some(format!(
        "{{\"type\":\"content_block_delta\",\"index\":{ingress_index},\"delta\":{raw_str}}}",
    ))
}
