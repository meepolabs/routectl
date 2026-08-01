//! Bedrock ConverseStream eventstream decoder.
//!
//! Mirrors the Invoke-stream decoder in `super::super::eventstream` but
//! routes each AWS frame's `:event-type` header into the typed
//! Converse-specific event payloads in `super::response_types`. Same
//! framing invariants apply: 8 MB advertised-length DoS cap;
//! `MessageFrameDecoder`'s prelude-buffered state must be tracked to
//! avoid mis-reading header bytes as a length on the iteration after
//! `Incomplete`; truncation-after-prelude must surface as an explicit
//! error so the router's circuit breaker doesn't record a "successful"
//! probe for a half-flushed connection.
//!
//! State carried across frames:
//!   - The active `BlockState` map keyed by `contentBlockIndex`. AWS
//!     emits text / toolUse / reasoning blocks in arbitrary index order,
//!     so we track per-block kind to know how to interpret the next
//!     `contentBlockDelta`. Cleared when `contentBlockStop` fires for
//!     that index.
//!   - A `tool_call_index` counter assigned in block-start order so the
//!     OpenAI-shape `tool_calls[].index` is stable and deterministic.
//!   - The captured `messageStop.stopReason` so the closing chunk
//!     carries `finish_reason`. The metadata frame (which arrives last
//!     per AWS's documented event order) carries `usage` and `metrics`
//!     and emits its own closing chunk -- if metadata never arrives but
//!     messageStop did, we still emit the finish_reason.

use std::collections::HashMap;

use aws_smithy_types::event_stream::Message;
use bytes::Bytes;
use futures::stream::{BoxStream, Stream};
use serde_json::{Value, json};
use uuid::Uuid;

use routectl_core::{
    ChatChunk, Error, ReasoningDetail, ReasoningDetailKind, Result, Role,
    schema::{ChunkChoice, ChunkDelta, UsageDelta},
};

use super::super::frame::{self, FrameHandler, FrameLabel};
use super::response::{lift_stop_sequence, translate_cache_details};
use super::response_types::{
    ConverseUsage, StreamContentBlockDelta, StreamContentBlockStart,
    StreamContentBlockStartPayload, StreamContentBlockStop, StreamDelta, StreamMessageStart,
    StreamMessageStop, StreamMetadata,
};
use crate::anthropic_api::response::map_stop_reason;

/// Format tag matching the Anthropic-API egress so chained downstreams
/// see consistent reasoning_details across the Bedrock-Invoke and
/// Bedrock-Converse paths.
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Per-content-block streaming state.
#[derive(Debug, Clone)]
enum BlockState {
    /// Plain text accumulator. Text deltas flow through verbatim.
    Text,
    /// Tool-use accumulator. Carries the metadata captured at
    /// `contentBlockStart` so each delta can attribute the partial
    /// JSON to the right tool_call entry.
    ToolUse {
        id: String,
        name: String,
        call_index: u32,
    },
    /// Reasoning block. Index in the reasoning_details array assigned
    /// at start time so all deltas for this block carry the same
    /// `reasoning_details[].index`.
    ///
    /// Strategy A (matches anthropic_api/sse.rs): `accumulated` buffers
    /// thinking text deltas; `signature` is filled by signature_delta;
    /// `detail_id` is minted at first delta. The structured
    /// `ReasoningDetail` is deferred to `contentBlockStop` so the
    /// terminal entry carries BOTH text and signature -- the shape
    /// Anthropic's replay path requires for multi-turn echo through
    /// Bedrock-Invoke or anthropic-api egresses.
    Reasoning {
        detail_index: u32,
        accumulated: String,
        signature: Option<String>,
        detail_id: String,
    },
}

/// Persistent state across all frames in one ConverseStream response.
#[derive(Debug, Default)]
pub struct ConverseStreamState {
    blocks: HashMap<u32, BlockState>,
    next_call_index: u32,
    next_detail_index: u32,
    /// Set once the opening `delta.role="assistant"` chunk has been
    /// emitted (at `messageStart`). Guards the once-per-stream invariant
    /// so a malformed upstream repeating `messageStart` cannot emit a
    /// second role chunk.
    role_emitted: bool,
    /// Captured at messageStop; emitted on the closing chunk. AWS
    /// emits messageStop before metadata, so we hold onto the value
    /// until metadata flushes (or until end-of-stream if metadata
    /// never arrives).
    pending_stop_reason: Option<String>,
    /// Matched stop sequence captured at messageStop when AWS stops
    /// on `stop_sequence` AND the request opted into the
    /// `/stop_sequence` response-field path. Mirrors the
    /// non-streaming `extract_matched_stop_sequence` gate so streaming
    /// canonical chunks carry the same `matched_stop_sequence` shape.
    pending_stop_sequence: Option<String>,
}

/// Per-frame handler for the ConverseStream. Holds the cross-frame
/// block-state map; the shared framing layer owns everything up to the
/// decoded `Message`.
struct ConverseFrameHandler {
    state: ConverseStreamState,
}

impl FrameHandler for ConverseFrameHandler {
    fn on_frame(&mut self, provider_id: &str, message: Message) -> Result<Vec<ChatChunk>> {
        handle_converse_frame(provider_id, message, &mut self.state)
    }

    fn on_eof(&mut self, provider_id: &str) -> Vec<ChatChunk> {
        // messageStop arrived but metadata never did. AWS docs put
        // metadata last, but a network truncation or middleware quirk can
        // drop it silently. Without this flush, finish_reason (and any
        // partial usage we held) vanish from the wire and clients see a
        // stream that just stops. Emit the closing chunk with the captured
        // stop_reason and an empty UsageDelta.
        if self.state.pending_stop_reason.is_some() {
            tracing::warn!(
                provider = %provider_id,
                "stream ended after messageStop without metadata; \
                 emitting closing chunk with no usage info"
            );
            vec![build_closing_chunk(&mut self.state, None)]
        } else {
            Vec::new()
        }
    }
}

/// Decode Bedrock ConverseStream frames into routectl `ChatChunk`s.
/// Symmetric to `super::super::eventstream::invoke_stream` -- the shared
/// `frame::decode_frames` driver handles the AWS-eventstream framing;
/// this function supplies only the Converse-specific frame routing.
pub fn stream<S>(provider_id: String, byte_stream: S) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let handler = ConverseFrameHandler {
        state: ConverseStreamState::default(),
    };
    frame::decode_frames(provider_id, byte_stream, handler, FrameLabel::Converse)
}

/// Translate one decoded Converse-stream frame to zero-or-more canonical
/// `ChatChunk`s. Separate from `stream()` so unit tests can drive the
/// decoder synchronously without spinning up a futures runtime.
pub fn handle_converse_frame(
    provider_id: &str,
    message: Message,
    state: &mut ConverseStreamState,
) -> Result<Vec<ChatChunk>> {
    let payload = message.payload();
    let event_type = match frame::frame_type(&message) {
        frame::FrameType::Event(name) => name.to_string(),
        // A protocol-typed failure frame. Every exception ends the stream,
        // recognized member name or not -- skipping one truncates the
        // response while the breaker records the seat healthy.
        frame::FrameType::Exception(name) => {
            return Err(frame::exception_error(provider_id, name, payload));
        }
        frame::FrameType::Untyped => {
            tracing::debug!(
                provider = provider_id,
                "bedrock converse: skipping eventstream frame with no \
                 :message-type or :event-type"
            );
            return Ok(vec![]);
        }
    };

    match event_type.as_str() {
        "messageStart" => {
            // Parse the payload so a malformed start surfaces as a
            // streaming error rather than a silent skip.
            let _: StreamMessageStart = parse_payload(provider_id, payload, "messageStart")?;
            // Opening role chunk: an OpenAI-Chat stream opens with a
            // single `delta.role="assistant"` chunk before any content,
            // matching every peer egress lane. Emitted at the structural
            // stream open (`messageStart`) only -- a stream that errors
            // before it yields no role chunk. Guarded to fire once.
            if state.role_emitted {
                Ok(vec![])
            } else {
                state.role_emitted = true;
                Ok(vec![role_chunk()])
            }
        }
        "contentBlockStart" => {
            let ev: StreamContentBlockStart =
                parse_payload(provider_id, payload, "contentBlockStart")?;
            handle_block_start(provider_id, state, ev);
            Ok(vec![])
        }
        "contentBlockDelta" => {
            let ev: StreamContentBlockDelta =
                parse_payload(provider_id, payload, "contentBlockDelta")?;
            Ok(handle_block_delta(provider_id, state, ev))
        }
        "contentBlockStop" => {
            let ev: StreamContentBlockStop =
                parse_payload(provider_id, payload, "contentBlockStop")?;
            // Strategy A: on a Reasoning block, emit the aggregated
            // structured detail carrying both text + signature.
            let removed = state.blocks.remove(&ev.content_block_index);
            let chunks = if let Some(BlockState::Reasoning {
                detail_index,
                accumulated,
                signature,
                detail_id,
            }) = removed
            {
                if accumulated.is_empty() && signature.is_none() {
                    // Empty thinking block -- skip emission so replay
                    // doesn't push a doomed empty Thinking block.
                    vec![]
                } else {
                    vec![reasoning_terminal_chunk(
                        detail_index,
                        detail_id,
                        accumulated,
                        signature,
                    )]
                }
            } else {
                vec![]
            };
            Ok(chunks)
        }
        "messageStop" => {
            let ev: StreamMessageStop = parse_payload(provider_id, payload, "messageStop")?;
            // Capture stop_reason for the metadata-or-EOS chunk. AWS
            // emits messageStop before metadata, so we hold it.
            // Mirror the non-streaming gate: only lift `stop_sequence`
            // out of additionalModelResponseFields when the upstream
            // actually stopped on a matched sequence; debug-log if the
            // gate is satisfied but the value is absent or non-string.
            state.pending_stop_sequence = lift_stop_sequence(
                provider_id,
                ev.stop_reason.as_deref(),
                ev.additional_model_response_fields.as_ref(),
            );
            state.pending_stop_reason = ev.stop_reason;
            Ok(vec![])
        }
        "metadata" => {
            let ev: StreamMetadata = parse_payload(provider_id, payload, "metadata")?;
            Ok(vec![build_closing_chunk(state, ev.usage.as_ref())])
        }
        // AWS exception event types named in `:event-type` by an upstream
        // that violates the protocol's `:message-type: exception` framing.
        // Status codes come from the shared frame-layer mapping so the two
        // Bedrock lanes classify identically.
        "internalServerException"
        | "modelStreamErrorException"
        | "validationException"
        | "throttlingException"
        | "serviceUnavailableException"
        | "accessDeniedException"
        | "unauthorizedException" => Err(frame::exception_error(provider_id, &event_type, payload)),
        // Unknown frames -- log + skip per the same forward-compat
        // policy as invoke_stream.
        other => {
            tracing::debug!(
                provider = provider_id,
                event_type = other,
                "bedrock converse: skipping unknown eventstream frame"
            );
            Ok(vec![])
        }
    }
}

fn handle_block_start(
    provider_id: &str,
    state: &mut ConverseStreamState,
    ev: StreamContentBlockStart,
) {
    let kind = match ev.start {
        Some(StreamContentBlockStartPayload::ToolUse { tool_use }) => {
            let call_index = state.next_call_index;
            state.next_call_index += 1;
            BlockState::ToolUse {
                id: tool_use.tool_use_id,
                name: tool_use.name,
                call_index,
            }
        }
        Some(StreamContentBlockStartPayload::Other(ref raw)) => {
            // An unrecognized start payload. Per AWS docs only tool_use
            // blocks carry a typed start payload today; a future AWS
            // block type would land here first. `raw` is an
            // upstream-controlled JSON value that may carry model output,
            // so DEBUG emits only a non-content marker (the top-level key
            // list); the full shape is gated behind TRACE and routed
            // through the prompt-redaction + control-char sanitizer so it
            // inherits the same hygiene as the body-trace helpers. Default
            // to Text; the first delta's shape disambiguates and the delta
            // handler upgrades to Reasoning on the first reasoningContent
            // delta.
            let payload_keys: String = match raw {
                serde_json::Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join(","),
                serde_json::Value::Array(_) => "<array>".to_string(),
                serde_json::Value::String(_) => "<string>".to_string(),
                serde_json::Value::Number(_) => "<number>".to_string(),
                serde_json::Value::Bool(_) => "<bool>".to_string(),
                serde_json::Value::Null => "<null>".to_string(),
            };
            tracing::debug!(
                provider = provider_id,
                payload_keys = %routectl_core::sanitize_for_log(&payload_keys),
                "bedrock converse: unknown contentBlockStart payload type; \
                 defaulting to Text block state -- first delta will disambiguate"
            );
            if tracing::enabled!(tracing::Level::TRACE) {
                let redacted = routectl_core::redact_prompts_in(raw);
                let serialized = serde_json::to_string(&redacted).unwrap_or_default();
                tracing::trace!(
                    provider = provider_id,
                    start_payload = %routectl_core::sanitize_for_log(&serialized),
                    "bedrock converse: unknown contentBlockStart payload (redacted)"
                );
            }
            BlockState::Text
        }
        None => {
            // AWS marks `ContentBlockStartEvent.start` required and its
            // union has only image / toolResult / toolUse members, so a
            // start frame with no payload is not a documented shape --
            // text and reasoning blocks get NO start frame at all (the
            // event-sequence docs label contentBlockStart "tool use
            // only", and the worked example goes messageStart ->
            // contentBlockDelta directly). Tolerated rather than
            // rejected: default to Text and let the delta handler
            // upgrade to Reasoning on the first reasoningContent delta.
            // Blocks are opened lazily by `handle_block_delta`, so no
            // delta depends on this arm running.
            BlockState::Text
        }
    };
    state.blocks.insert(ev.content_block_index, kind);
}

fn handle_block_delta(
    provider_id: &str,
    state: &mut ConverseStreamState,
    ev: StreamContentBlockDelta,
) -> Vec<ChatChunk> {
    let Some(delta) = ev.delta else {
        return vec![];
    };
    match delta {
        StreamDelta::Text { text } => {
            // Self-starting: AWS sends NO contentBlockStart for a text
            // block (start is required and its union has no text
            // member), so the first delta opens the block. An existing
            // ToolUse entry at this index is a genuine wire violation --
            // a text delta on a tool block would corrupt the accumulator.
            match state.blocks.get(&ev.content_block_index) {
                Some(BlockState::ToolUse { .. }) => {
                    tracing::debug!(
                        provider = provider_id,
                        content_block_index = ev.content_block_index,
                        "bedrock converse: skipping text delta on a tool_use block"
                    );
                    return vec![];
                }
                Some(_) => {}
                None => {
                    state
                        .blocks
                        .insert(ev.content_block_index, BlockState::Text);
                }
            }
            vec![text_chunk(text)]
        }
        StreamDelta::ToolUse { tool_use } => {
            let block = state.blocks.get(&ev.content_block_index);
            let (id, name, call_index) = match block {
                Some(BlockState::ToolUse {
                    id,
                    name,
                    call_index,
                }) => (id.clone(), name.clone(), *call_index),
                _ => {
                    // Defensive: AWS emits `contentBlockStart` before
                    // any toolUse delta. If we got here without a
                    // matching start, skip the delta rather than
                    // synthesize a partial tool_call.
                    return vec![];
                }
            };
            vec![tool_delta_chunk(id, name, call_index, tool_use.input)]
        }
        StreamDelta::ReasoningContent { reasoning_content } => {
            // Self-starting, same as the text arm: AWS sends no
            // contentBlockStart for a reasoning block, so the first
            // reasoningContent delta at this index opens it. A
            // placeholder `Text` state (inserted by a start frame with
            // an unrecognized or absent payload) upgrades in place so
            // the detail index is minted exactly once per block. Only a
            // ToolUse entry is skipped -- that is a real wire violation.
            match state.blocks.get(&ev.content_block_index) {
                Some(BlockState::Reasoning { .. }) => {}
                Some(BlockState::ToolUse { .. }) => {
                    tracing::debug!(
                        provider = provider_id,
                        content_block_index = ev.content_block_index,
                        "bedrock converse: skipping reasoning delta on a tool_use block"
                    );
                    return vec![];
                }
                Some(BlockState::Text) | None => {
                    let di = state.next_detail_index;
                    state.next_detail_index += 1;
                    state.blocks.insert(
                        ev.content_block_index,
                        BlockState::Reasoning {
                            detail_index: di,
                            accumulated: String::new(),
                            signature: None,
                            detail_id: Uuid::new_v4().to_string(),
                        },
                    );
                }
            }
            let mut chunks = Vec::new();
            // Strategy A: accumulate text + signature on the open
            // block; emit only the LIVE `reasoning` string per delta.
            // The structured `ReasoningDetail` lands at
            // contentBlockStop with both fields paired, matching the
            // anthropic_api streaming path so replay round-trips.
            if let Some(text) = reasoning_content.text {
                if let Some(BlockState::Reasoning { accumulated, .. }) =
                    state.blocks.get_mut(&ev.content_block_index)
                {
                    accumulated.push_str(&text);
                }
                chunks.push(reasoning_text_chunk(text));
            }
            if let Some(sig) = reasoning_content.signature
                && let Some(BlockState::Reasoning { signature, .. }) =
                    state.blocks.get_mut(&ev.content_block_index)
            {
                *signature = Some(sig);
            }
            // No per-event chunk for signature -- the terminal
            // detail at contentBlockStop carries it.
            if let Some(redacted) = reasoning_content.redacted_content {
                // Redacted reasoning has no text/signature pair; emit
                // immediately. Allocate a fresh detail index from the
                // shared counter (the same one the text-reasoning path
                // increments above) so the redacted ReasoningDetail
                // carries Some(index) and sort-by-index preserves wire
                // order -- without it the detail's index is None and a
                // stable sort cannot place it relative to text blocks.
                let di = state.next_detail_index;
                state.next_detail_index += 1;
                chunks.push(reasoning_redacted_chunk(di, redacted));
            }
            chunks
        }
        StreamDelta::Other(_) => {
            // Forward compat: a future AWS delta type (citation, image,
            // toolResult) lands here. Log at DEBUG so a future AWS-typed
            // delta is visible in trace logs without noise at higher levels.
            tracing::debug!(
                provider = provider_id,
                content_block_index = ev.content_block_index,
                "bedrock converse: unknown content-block delta type; skipping"
            );
            vec![]
        }
    }
}

fn build_closing_chunk(
    state: &mut ConverseStreamState,
    usage: Option<&ConverseUsage>,
) -> ChatChunk {
    let finish_reason = state
        .pending_stop_reason
        .take()
        .and_then(|s| map_stop_reason(Some(s.as_str())));
    let matched_stop_sequence = state.pending_stop_sequence.take();
    let usage_delta = usage.map(|u| {
        let cache_write = u.cache_write_input_tokens.unwrap_or(0);
        let cache_read = u.cache_read_input_tokens.unwrap_or(0);
        let prompt_tokens = u
            .input_tokens
            .saturating_add(cache_write)
            .saturating_add(cache_read);
        let completion_tokens = u.output_tokens;
        let total_tokens = u
            .total_tokens
            .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
        UsageDelta {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            total_tokens: Some(total_tokens),
            cache_creation_input_tokens: u.cache_write_input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation: u.cache_details.as_deref().map(translate_cache_details),
            ..Default::default()
        }
    });
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason,
            matched_stop_sequence,
        }],
        usage: usage_delta,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Opening chunk carrying only `delta.role="assistant"`. Non-final, so
/// `usage` and `finish_reason` stay absent (both skip-serialize when
/// None), matching the peer lanes' opening chunk shape. id/model are
/// empty like every other Converse-stream chunk (the ingress stamps the
/// stream-stable envelope).
fn role_chunk() -> ChatChunk {
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some(Role::Assistant),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

fn text_chunk(text: String) -> ChatChunk {
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

fn tool_delta_chunk(id: String, name: String, call_index: u32, partial_json: String) -> ChatChunk {
    let tool_call_delta: Value = json!({
        "index": call_index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": partial_json}
    });
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![tool_call_delta]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Strategy A live thinking-string chunk: carries only the plain
/// `reasoning` string. The structured `ReasoningDetail` is deferred
/// to `reasoning_terminal_chunk` at `contentBlockStop`.
fn reasoning_text_chunk(thinking: String) -> ChatChunk {
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning: Some(thinking),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

/// Strategy A terminal chunk: emits ONE aggregated `ReasoningDetail`
/// per thinking block carrying both `text` and `signature`. Mirrors
/// `anthropic_api::sse::make_thinking_terminal_chunk` so replay
/// across either egress (Bedrock-Invoke or anthropic-api) sees the
/// same byte-shape.
fn reasoning_terminal_chunk(
    detail_index: u32,
    detail_id: String,
    accumulated: String,
    signature: Option<String>,
) -> ChatChunk {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some(detail_id),
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(detail_index),
        payload: json!({
            "text": accumulated,
            "signature": signature.unwrap_or_default(),
        }),
    };
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![detail],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

fn reasoning_redacted_chunk(detail_index: u32, data: String) -> ChatChunk {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(Uuid::new_v4().to_string()),
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(detail_index),
        payload: json!({"data": data}),
    };
    ChatChunk {
        id: String::new(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![detail],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

fn parse_payload<T: serde::de::DeserializeOwned>(
    provider_id: &str,
    bytes: &[u8],
    label: &str,
) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| {
        Error::Streaming(format!(
            "bedrock converse-stream {label} payload parse failed (provider={provider_id}): {e}"
        ))
    })
}

#[cfg(test)]
#[path = "eventstream_tests.rs"]
mod tests;
