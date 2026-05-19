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

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::Message;
use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::{json, Value};
use uuid::Uuid;

use routectl_core::{
    schema::{ChunkChoice, ChunkDelta, UsageDelta},
    ChatChunk, Error, ReasoningDetail, ReasoningDetailKind, Result,
};

use super::response::map_stop_reason;
use super::response_types::{
    ConverseUsage, StreamContentBlockDelta, StreamContentBlockStart,
    StreamContentBlockStartPayload, StreamContentBlockStop, StreamDelta, StreamMessageStart,
    StreamMessageStop, StreamMetadata,
};

/// Same cap as the Invoke side. See
/// `super::super::eventstream::MAX_FRAME_BYTES` for the rationale.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
struct ConverseStreamState {
    blocks: HashMap<u32, BlockState>,
    next_call_index: u32,
    next_detail_index: u32,
    /// Captured at messageStop; emitted on the closing chunk. AWS
    /// emits messageStop before metadata, so we hold onto the value
    /// until metadata flushes (or until end-of-stream if metadata
    /// never arrives).
    pending_stop_reason: Option<String>,
}

/// Decode Bedrock ConverseStream frames into routectl `ChatChunk`s.
/// Symmetric to `super::super::eventstream::invoke_stream` -- same
/// framing layer, Converse-specific per-frame handler.
pub fn stream<S>(provider_id: String, byte_stream: S) -> BoxStream<'static, Result<ChatChunk>>
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let s = async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut decoder = MessageFrameDecoder::new();
        let mut state = ConverseStreamState::default();
        // Smithy-prelude tracking state -- see invoke_stream for
        // the full story. False until the smithy decoder buffers a
        // prelude internally on Incomplete; flipped back to false
        // when smithy returns Complete (which calls `self.reset()`).
        let mut smithy_has_prelude_buffered = false;

        let mut byte_stream = Box::pin(byte_stream);
        loop {
            loop {
                // Advertised-length DoS guard. Mirrors invoke_stream.
                if !smithy_has_prelude_buffered && buffer.len() >= 4 {
                    let advertised = u32::from_be_bytes([
                        buffer[0], buffer[1], buffer[2], buffer[3],
                    ]) as usize;
                    if advertised > MAX_FRAME_BYTES {
                        yield Err(Error::Streaming(format!(
                            "bedrock converse-stream frame advertised {advertised} bytes, exceeds cap {MAX_FRAME_BYTES}"
                        )));
                        return;
                    }
                }
                let mut cursor = std::io::Cursor::new(buffer.as_ref());
                match decoder.decode_frame(&mut cursor) {
                    Ok(DecodedFrame::Complete(message)) => {
                        let consumed = usize::try_from(cursor.position()).map_err(|_| {
                            Error::Streaming(
                                "bedrock converse-stream consumed more than usize::MAX bytes"
                                    .into(),
                            )
                        })?;
                        let _ = buffer.split_to(consumed);
                        smithy_has_prelude_buffered = false;

                        match handle_converse_frame(&provider_id, message, &mut state) {
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
                        let consumed = cursor.position() as usize;
                        if consumed > 0 {
                            let _ = buffer.split_to(consumed);
                            smithy_has_prelude_buffered = true;
                        }
                        break;
                    }
                    Err(e) => {
                        // Skip-and-continue, mirroring invoke_stream
                        // so a single bad frame doesn't kill an
                        // in-flight stream.
                        let advertised = if buffer.len() >= 4 {
                            u32::from_be_bytes([
                                buffer[0], buffer[1], buffer[2], buffer[3],
                            ]) as usize
                        } else {
                            0
                        };
                        let dump_len = advertised.min(256).min(buffer.len());
                        let hex: String = buffer[..dump_len]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        tracing::warn!(
                            provider = %provider_id,
                            err = %e,
                            frame_len = advertised,
                            hex = %hex,
                            "bedrock converse-stream frame decode failed; skipping frame"
                        );
                        if advertised > 0 && buffer.len() >= advertised {
                            let _ = buffer.split_to(advertised);
                        } else {
                            buffer.clear();
                        }
                        decoder = MessageFrameDecoder::new();
                        smithy_has_prelude_buffered = false;
                        continue;
                    }
                }
            }

            match byte_stream.next().await {
                Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                Some(Err(e)) => {
                    yield Err(Error::Streaming(format!(
                        "bedrock converse upstream byte read failed: {e}"
                    )));
                    return;
                }
                None => {
                    if !buffer.is_empty() {
                        yield Err(Error::Streaming(format!(
                            "bedrock converse-stream truncated: {} buffered bytes left at EOF",
                            buffer.len()
                        )));
                    } else if smithy_has_prelude_buffered {
                        yield Err(Error::Streaming(
                            "bedrock converse-stream truncated: prelude consumed but frame body never arrived before EOF"
                                .to_string(),
                        ));
                    } else if state.pending_stop_reason.is_some() {
                        // messageStop arrived but metadata never did.
                        // AWS docs put metadata last, but a network
                        // truncation or middleware quirk can drop it
                        // silently. Without this flush, finish_reason
                        // (and any partial usage we held) vanish from
                        // the wire and clients see a stream that just
                        // stops. Emit the closing chunk with the
                        // captured stop_reason and an empty UsageDelta.
                        tracing::warn!(
                            provider = %provider_id,
                            "stream ended after messageStop without metadata; \
                             emitting closing chunk with no usage info"
                        );
                        yield Ok(build_closing_chunk(&mut state, None));
                    }
                    return;
                }
            }
        }
    };

    Box::pin(s)
}

/// Translate one decoded Converse-stream frame to zero-or-more canonical
/// `ChatChunk`s. Separate from `stream()` so unit tests can drive the
/// decoder synchronously without spinning up a futures runtime.
fn handle_converse_frame(
    provider_id: &str,
    message: Message,
    state: &mut ConverseStreamState,
) -> Result<Vec<ChatChunk>> {
    let event_type = header_str(&message, ":event-type")
        .unwrap_or("")
        .to_string();
    let payload = message.payload();

    match event_type.as_str() {
        "messageStart" => {
            // Role only -- no content yet. Nothing to emit; the OpenAI
            // wire shape doesn't have a "role assigned" event. We still
            // parse the payload so a malformed start surfaces as a
            // streaming error rather than a silent skip.
            let _: StreamMessageStart = parse_payload(provider_id, payload, "messageStart")?;
            Ok(vec![])
        }
        "contentBlockStart" => {
            let ev: StreamContentBlockStart =
                parse_payload(provider_id, payload, "contentBlockStart")?;
            handle_block_start(state, ev);
            Ok(vec![])
        }
        "contentBlockDelta" => {
            let ev: StreamContentBlockDelta =
                parse_payload(provider_id, payload, "contentBlockDelta")?;
            Ok(handle_block_delta(state, ev))
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
            state.pending_stop_reason = ev.stop_reason;
            Ok(vec![])
        }
        "metadata" => {
            let ev: StreamMetadata = parse_payload(provider_id, payload, "metadata")?;
            Ok(vec![build_closing_chunk(state, ev.usage.as_ref())])
        }
        // AWS exception event types. Status codes mirror invoke's
        // mapping so the router sees consistent classification across
        // the two Bedrock paths.
        "internalServerException"
        | "modelStreamErrorException"
        | "validationException"
        | "throttlingException"
        | "serviceUnavailableException"
        | "accessDeniedException"
        | "unauthorizedException" => Err(decode_exception_event(provider_id, &event_type, payload)),
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

fn handle_block_start(state: &mut ConverseStreamState, ev: StreamContentBlockStart) {
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
        Some(StreamContentBlockStartPayload::Other(_)) | None => {
            // Per AWS docs only tool_use blocks carry a typed start
            // payload. Text + reasoning open without one. We don't know
            // for certain at start time which kind this is -- but the
            // first delta's shape disambiguates and we update on the
            // fly. Default to Text; the delta handler upgrades to
            // Reasoning on the first reasoningContent delta.
            BlockState::Text
        }
    };
    state.blocks.insert(ev.content_block_index, kind);
}

fn handle_block_delta(
    state: &mut ConverseStreamState,
    ev: StreamContentBlockDelta,
) -> Vec<ChatChunk> {
    let Some(delta) = ev.delta else {
        return vec![];
    };
    match delta {
        StreamDelta::Text { text } => {
            // Symmetric with tool-use: require a prior contentBlockStart
            // (AWS emits one for every block, even text). A delta on
            // an unknown block is an out-of-band event -- skip rather
            // than synthesize a chunk that would corrupt the canonical
            // stream. Mirrors the ToolUse arm's defensive-skip below.
            if !state.blocks.contains_key(&ev.content_block_index) {
                tracing::debug!(
                    content_block_index = ev.content_block_index,
                    "skipping text delta on unknown block (no prior contentBlockStart)"
                );
                return vec![];
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
            // Upgrade the placeholder `Text` state (inserted on
            // `contentBlockStart` for no-payload blocks) to
            // `Reasoning` on the first reasoningContent delta. If
            // there's NO prior state at this index, that means
            // contentBlockStart was missed -- skip rather than
            // synthesize. Symmetric with the text + tool-use arms.
            match state.blocks.get(&ev.content_block_index) {
                Some(BlockState::Reasoning { .. }) => {}
                Some(BlockState::Text) => {
                    let di = state.next_detail_index;
                    state.next_detail_index += 1;
                    state.blocks.insert(
                        ev.content_block_index,
                        BlockState::Reasoning {
                            detail_index: di,
                            accumulated: String::new(),
                            signature: None,
                            detail_id: uuid::Uuid::new_v4().to_string(),
                        },
                    );
                }
                _ => {
                    tracing::debug!(
                        content_block_index = ev.content_block_index,
                        "skipping reasoning delta on unknown or non-text block \
                         (no prior contentBlockStart, or block is tool_use)"
                    );
                    return vec![];
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
            if let Some(sig) = reasoning_content.signature {
                if let Some(BlockState::Reasoning { signature, .. }) =
                    state.blocks.get_mut(&ev.content_block_index)
                {
                    *signature = Some(sig);
                }
                // No per-event chunk for signature -- the terminal
                // detail at contentBlockStop carries it.
            }
            if let Some(redacted) = reasoning_content.redacted_content {
                // Redacted reasoning has no text/signature pair; emit
                // immediately as today.
                chunks.push(reasoning_redacted_chunk(redacted));
            }
            chunks
        }
        StreamDelta::Other(_) => {
            // Forward compat -- skip silently. A future AWS delta type
            // (citation, image, toolResult) lands here; the OpenAI
            // canonical shape doesn't have a place for them today.
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
            // Same caveat as `converse/response.rs::translate`:
            // Bedrock Converse stream events do not include the
            // matched stop sequence on `messageStop`. AWS surfaces
            // it via `additionalModelResponseFields` only when
            // `additionalModelResponseFieldPaths` opted in on the
            // request side. Tracked as a follow-up. Bedrock-Invoke
            // (which delegates to `anthropic_api::sse::SseState`)
            // already lifts the native field.
            matched_stop_sequence: None,
        }],
        usage: usage_delta,
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
    }
}

fn reasoning_redacted_chunk(data: String) -> ChatChunk {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(Uuid::new_v4().to_string()),
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: None,
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
    }
}

fn parse_payload<T: serde::de::DeserializeOwned>(
    provider_id: &str,
    bytes: &[u8],
    label: &str,
) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| {
        Error::Streaming(format!(
            "bedrock converse-stream {} payload parse failed (provider={provider_id}): {e}",
            label
        ))
    })
}

fn decode_exception_event(provider_id: &str, event_type: &str, payload: &[u8]) -> Error {
    // CLAUDE.md log conventions: 256-char body excerpt + structured
    // tracing for auth/permission events.
    let v: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let msg = v
        .pointer("/message")
        .or_else(|| v.pointer("/Message"))
        .and_then(|x| x.as_str())
        .unwrap_or(event_type)
        .to_string();
    let status: u16 = match event_type {
        "throttlingException" => 429,
        "validationException" => 400,
        "serviceUnavailableException" => 503,
        "accessDeniedException" => 403,
        "unauthorizedException" => 401,
        _ => 500,
    };
    if matches!(
        event_type,
        "accessDeniedException" | "unauthorizedException"
    ) {
        tracing::warn!(
            provider = %provider_id,
            event_type = %event_type,
            message = %truncate_excerpt(&msg),
            "bedrock in-stream auth/permission exception",
        );
    }
    Error::upstream(provider_id, status, msg)
}

fn truncate_excerpt(s: &str) -> String {
    s.chars()
        .take(routectl_core::MAX_LOG_BODY_EXCERPT)
        .collect()
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
#[path = "eventstream_tests.rs"]
mod tests;
