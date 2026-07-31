//! Anthropic SSE event state machine.
//!
//! Anthropic streams a sequence of typed events. The state machine here tracks
//! which content block is currently open so that deltas are attributed to the
//! correct block type (text / thinking / tool_use).
//!
//! The stateful accumulation lives inside the stream() method in mod.rs which
//! owns an SseState and drives parse_event() directly.

use serde_json::{Value, json};
use uuid::Uuid;

use routectl_core::{
    ChatChunk, Error, OpaqueSseEvent, ReasoningDetail, Result, Role,
    schema::{CacheCreation, ChunkChoice, ChunkDelta, UsageDelta},
};

use super::response::map_stop_reason;
use super::sse_opaque::{MAX_OPAQUE_BYTES_PER_BLOCK, MAX_OPAQUE_DELTAS_PER_BLOCK, OpaqueCapture};
use super::types::SseEvent;

/// Which kind of content block is currently open. Every variant
/// carries the upstream `index` from its `content_block_start` so the
/// state machine can validate that subsequent `content_block_delta`
/// and `content_block_stop` events attribute to the correct block
/// (Anthropic's wire-shape invariant).
#[derive(Debug, Clone)]
pub enum OpenBlockKind {
    /// A `text` content block.
    Text {
        /// Upstream `content_block` index this block opened at.
        upstream_index: u32,
    },
    /// A `thinking` content block accumulating reasoning text.
    Thinking {
        /// Upstream `content_block` index this block opened at.
        upstream_index: u32,
        /// Accumulated thinking text from `thinking_delta` events.
        /// Aggregated into ONE structured `ReasoningDetail` emitted at
        /// `content_block_stop` so the final assistant message has a
        /// reasoning_details entry whose payload carries BOTH `text`
        /// and `signature` -- the shape Anthropic's replay path
        /// requires on multi-turn follow-ups. The intermediate
        /// `thinking_delta` chunks still carry the live `reasoning`
        /// string for streaming UI; only the structured detail is
        /// deferred.
        accumulated: String,
        /// Signature from a `signature_delta` event, if any. Anthropic
        /// 4.5 sometimes omits this on tool-only thinking turns; left
        /// `None` means the terminal detail emits with an empty
        /// signature (and replay tolerates this with a debug skip --
        /// see `request.rs::emit_reasoning_blocks`).
        signature: Option<String>,
        /// Stable detail id minted at `content_block_start`. The same
        /// id rides on the terminal aggregated detail so a client
        /// replaying or coalescing by id sees one logical entry per
        /// thinking block.
        detail_id: String,
        /// Block index in reasoning_details array.
        detail_index: u32,
    },
    /// A `tool_use` content block accumulating input JSON.
    ToolUse {
        /// Upstream `content_block` index this block opened at.
        upstream_index: u32,
        /// Tool-use id echoed on the emitted tool call.
        id: String,
        /// Tool name echoed on the emitted tool call.
        name: String,
        /// Index in the tool_calls array being built.
        call_index: u32,
    },
    /// Forward-compat: a `content_block.type` value that is not in the
    /// known typed set (e.g. `server_tool_use`,
    /// `web_search_tool_result`). The block emits no canonical chunks;
    /// its raw bytes ride on `ChatChunk.opaque_events` for verbatim
    /// re-emission by the matching Anthropic ingress (see
    /// `sse_opaque`).
    Unknown {
        /// Upstream `content_block` index this block opened at.
        upstream_index: u32,
        /// The unrecognized `content_block.type` value.
        type_tag: String,
    },
}

impl OpenBlockKind {
    /// Upstream `content_block` index this open block was opened at.
    /// Used to validate that subsequent delta and stop events
    /// attribute to the correct block.
    pub const fn upstream_index(&self) -> u32 {
        match self {
            Self::Text { upstream_index }
            | Self::Thinking { upstream_index, .. }
            | Self::ToolUse { upstream_index, .. }
            | Self::Unknown { upstream_index, .. } => *upstream_index,
        }
    }
}

/// Persistent state across SSE events for one streaming response.
#[derive(Debug, Default)]
pub struct SseState {
    /// Response id carried onto every emitted chunk.
    pub id: String,
    /// Model name carried onto every emitted chunk.
    pub model: String,
    /// Next index to assign in the reasoning_details array.
    pub next_detail_index: u32,
    /// Next index to assign in the tool_calls array.
    pub next_call_index: u32,
    /// The content block currently open, if any.
    pub open_block: Option<OpenBlockKind>,
    /// Captured from `message_start.message.usage`. Anthropic emits
    /// the input side of usage exactly once, in `message_start`; the
    /// streaming `message_delta` events carry only output-side updates.
    /// We carry the captured input fields forward so the final
    /// `message_delta` chunk we emit downstream has full prompt_tokens
    /// (sum of input + cache_creation + cache_read), matching what
    /// OpenAI clients expect on the closing usage frame.
    pub captured_input_usage: Option<CapturedInputUsage>,
    /// Buffer of opaque events captured while an `OpenBlockKind::Unknown`
    /// block was open. Drained into `ChatChunk.opaque_events` on the
    /// next emitted canonical chunk; flushed onto a synthetic empty
    /// chunk at `MessageStop` if no canonical chunk followed the
    /// unknown block.
    pub pending_opaque: Vec<OpaqueSseEvent>,
    /// Per-block running totals for the bounded opaque-capture state.
    /// `Some` while an `OpenBlockKind::Unknown` block is open; cleared
    /// on `content_block_stop`.
    pub(super) current_capture: Option<OpaqueCapture>,
    /// Per-STREAM running totals for bounded opaque-capture, summed
    /// across every unknown block (unlike `current_capture`, these
    /// never reset per block). `pending_opaque` only drains onto an
    /// emitted canonical chunk, so a stream of only unknown blocks
    /// buffers unbounded without a per-stream ceiling. Counts only the
    /// start + delta captures that were actually pushed.
    pub(super) opaque_bytes_total: usize,
    pub(super) opaque_events_total: usize,
    /// Sticky: set once either per-stream opaque cap is crossed. Mirrors
    /// the per-block `OpaqueCapture::degraded` idiom at stream scope.
    /// Once set, `open_unknown_block` / `capture_unknown_delta`
    /// short-circuit to sink-drain -- no further opaque bytes buffer.
    /// Never reset within a response.
    pub(super) opaque_stream_degraded: bool,
    /// True once ANY chunk has been emitted downstream for this
    /// response. Recorded so the per-stream degrade WARN can report
    /// whether the trip landed before or after the first client-visible
    /// chunk (pre-first-chunk vs post-first-chunk fidelity loss); the
    /// degrade-to-drop behavior itself is identical either way.
    pub(super) canonical_chunk_emitted: bool,
    /// Set once the opening `delta.role="assistant"` chunk has been
    /// emitted (at `message_start`). Guards the once-per-stream
    /// invariant so a malformed upstream repeating `message_start`
    /// cannot emit a second role chunk.
    pub(super) role_emitted: bool,
    /// Thinking blocks completed since the last tool_use block (or since
    /// the start of the turn). Accumulates across `content_block_stop`
    /// events for Thinking blocks; CLEARED at each `ContentBlockStart::ToolUse`
    /// so each tool_use sees only the thinking that immediately preceded it
    /// (non-cumulative). Cleared on `MessageStop`.
    pub(super) completed_thinking: Vec<ReasoningDetail>,
    /// `(tool_use_id, thinking_snapshot)` pairs ready for the
    /// post-stream batch write into the thinking cache. Populated at
    /// each `ContentBlockStart::ToolUse` with a clone of `completed_thinking`
    /// at that point (non-cumulative: only thinking since the last
    /// tool_use). Drained by `stream()` in `mod.rs` after the SSE
    /// pipeline finishes -- NOT cleared here.
    pub(super) pending_cache_writes: Vec<(String, Vec<ReasoningDetail>)>,
    /// Per-request tool-name reverse map (upstream renamed name ->
    /// original client name) from the cloak forward pass. Used to restore
    /// the client's original tool names on streamed `tool_use` blocks.
    /// The name is reversed ONCE when the block opens
    /// (`ContentBlockStart::ToolUse`); every `input_json_delta` chunk
    /// re-emits the stored name and so inherits the reversal. Empty map =
    /// no-op.
    pub tool_reverse: std::collections::HashMap<String, String>,
}

/// Input-side usage captured once from `message_start`, carried forward
/// so the closing usage frame reports full prompt tokens.
#[derive(Debug, Default, Clone)]
pub struct CapturedInputUsage {
    /// Base input tokens.
    pub input_tokens: u32,
    /// Tokens written to the prompt cache, if reported.
    pub cache_creation_input_tokens: Option<u32>,
    /// Tokens read from the prompt cache, if reported.
    pub cache_read_input_tokens: Option<u32>,
    /// Per-TTL cache-creation breakdown, if reported.
    pub cache_creation: Option<CacheCreation>,
}

impl CapturedInputUsage {
    /// Sum of input + cache_creation + cache_read tokens. Mirrors the
    /// canonical `Usage.prompt_tokens` semantics.
    #[cfg(test)]
    fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
    }
}

impl SseState {
    /// Construct a fresh state for one streaming response. Emits a
    /// single TRACE line announcing the active opaque-capture caps so
    /// operators triaging an overflow have the limits visible in logs
    /// without grepping the source. Fires once per stream (not per
    /// event), so the TRACE-level cost is bounded.
    pub fn new(provider_id: &str) -> Self {
        tracing::trace!(
            provider = %provider_id,
            max_opaque_bytes_per_block = MAX_OPAQUE_BYTES_PER_BLOCK,
            max_opaque_deltas_per_block = MAX_OPAQUE_DELTAS_PER_BLOCK,
            "anthropic SSE state opened: opaque-capture caps active",
        );
        Self::default()
    }

    /// Parse one raw SSE data line (the JSON string after "data: ").
    /// Returns Ok(None) for housekeeping events, Ok(Some(chunk)) for
    /// content. Wraps `dispatch_event` to drain `pending_opaque` onto
    /// the next emitted chunk so opaque events captured during prior
    /// no-emit events ride out together with the next canonical
    /// emission (see `sse_opaque`).
    pub fn parse_event(&mut self, provider_id: &str, data: &str) -> Result<Option<ChatChunk>> {
        let event: SseEvent = serde_json::from_str(data)
            .map_err(|e| Error::Streaming(format!("bad sse json: {e}")))?;
        let emitted = self.dispatch_event(provider_id, event)?;
        if emitted.is_some() {
            self.canonical_chunk_emitted = true;
        }
        Ok(emitted.map(|mut chunk| {
            if !self.pending_opaque.is_empty() {
                chunk.opaque_events = std::mem::take(&mut self.pending_opaque);
            }
            chunk
        }))
    }

    fn dispatch_event(&mut self, provider_id: &str, event: SseEvent) -> Result<Option<ChatChunk>> {
        match event {
            SseEvent::MessageStart { message } => {
                self.id = message.id;
                self.model = message.model;
                if let Some(u) = message.usage {
                    self.captured_input_usage = Some(CapturedInputUsage {
                        input_tokens: u.input_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                        cache_creation: u.cache_creation.as_ref().map(|c| CacheCreation {
                            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
                            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
                        }),
                    });
                }
                // Opening role chunk: an OpenAI-Chat stream opens with
                // a single `delta.role="assistant"` chunk before any
                // content, matching every peer egress lane. Emitted at
                // the structural stream open (`message_start`) only, so a
                // stream that errors before `message_start` yields no role
                // chunk. Guarded to fire exactly once.
                if self.role_emitted {
                    Ok(None)
                } else {
                    self.role_emitted = true;
                    Ok(Some(self.role_chunk()))
                }
            }

            SseEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                use super::types::SseContentBlockStart;
                match content_block {
                    SseContentBlockStart::Text { .. } => {
                        self.open_block = Some(OpenBlockKind::Text {
                            upstream_index: index,
                        });
                    }
                    SseContentBlockStart::Thinking { .. } => {
                        let di = self.next_detail_index;
                        self.next_detail_index += 1;
                        self.open_block = Some(OpenBlockKind::Thinking {
                            upstream_index: index,
                            accumulated: String::new(),
                            signature: None,
                            detail_id: Uuid::new_v4().to_string(),
                            detail_index: di,
                        });
                    }
                    SseContentBlockStart::ToolUse { id, name } => {
                        let ci = self.next_call_index;
                        self.next_call_index += 1;
                        // Reverse the tool name to the client's original
                        // once, here at block open. make_tool_delta_chunk
                        // re-emits this stored name on every
                        // input_json_delta chunk, so all deltas inherit
                        // the reversal. Names absent from the map (or a
                        // bare name with no mcp__ shape) pass through.
                        let name = self.reverse_tool_name(name, provider_id);
                        // Snapshot thinking preceding this tool_use for
                        // context_management emulation. Non-cumulative:
                        // each tool_use is paired only with the thinking
                        // that immediately preceded it. Cleared here so
                        // subsequent thinking blocks accumulate fresh for
                        // the next tool_use in this response.
                        if !id.is_empty() {
                            self.pending_cache_writes
                                .push((id.clone(), self.completed_thinking.clone()));
                            self.completed_thinking.clear();
                        }
                        self.open_block = Some(OpenBlockKind::ToolUse {
                            upstream_index: index,
                            id,
                            name,
                            call_index: ci,
                        });
                    }
                    SseContentBlockStart::RedactedThinking { data } => {
                        // No per-token deltas follow a redacted_thinking
                        // block. Emit it immediately as a synthesized
                        // reasoning_details entry; the open_block stays
                        // None so the next block_start opens cleanly.
                        let di = self.next_detail_index;
                        self.next_detail_index += 1;
                        let detail = super::context_management::make_redacted_thinking_detail(
                            Uuid::new_v4().to_string(),
                            di,
                            data,
                        );
                        // Accumulate into completed_thinking so a subsequent
                        // ToolUse block's pending_cache_writes entry includes
                        // it. The shared helper above is the structural
                        // enforcement that the streaming detail shape matches
                        // the non-streaming `extract_tool_thinking` output.
                        self.completed_thinking.push(detail.clone());
                        return Ok(Some(ChatChunk {
                            id: self.id.clone(),
                            model: self.model.clone(),
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
                        }));
                    }
                    SseContentBlockStart::Other(value) => {
                        // Open an Unknown block and seed opaque capture
                        // with the start payload. The matching Anthropic
                        // ingress reconstructs the block from
                        // `chunk.opaque_events`.
                        self.open_unknown_block(index, &value, provider_id);
                    }
                }
                Ok(None)
            }

            SseEvent::ContentBlockDelta { index, delta } => {
                if !self.index_matches(index, "delta", provider_id) {
                    return Ok(None);
                }
                if matches!(self.open_block, Some(OpenBlockKind::Unknown { .. })) {
                    self.capture_unknown_delta(&delta, provider_id);
                    return Ok(None);
                }
                use super::types::SseDelta;
                match delta {
                    SseDelta::TextDelta { text } => Ok(Some(self.make_text_chunk(text))),
                    SseDelta::ThinkingDelta { thinking } => {
                        // Strategy A: accumulate text on the open block;
                        // emit a chunk carrying the live `reasoning`
                        // string ONLY (no structured ReasoningDetail).
                        // The aggregated detail with both text and
                        // signature lands at content_block_stop.
                        if let Some(OpenBlockKind::Thinking { accumulated, .. }) =
                            &mut self.open_block
                        {
                            accumulated.push_str(&thinking);
                        }
                        Ok(Some(self.make_thinking_string_chunk(thinking)))
                    }
                    SseDelta::SignatureDelta { signature } => {
                        // Strategy A: stash on the open block; do NOT
                        // emit a per-event chunk (the signature lands
                        // on the aggregated detail at content_block_stop).
                        if let Some(OpenBlockKind::Thinking { signature: sig, .. }) =
                            &mut self.open_block
                        {
                            *sig = Some(signature);
                        }
                        Ok(None)
                    }
                    SseDelta::InputJsonDelta { partial_json } => {
                        Ok(Some(self.make_tool_delta_chunk(partial_json)))
                    }
                    // Unknown delta inside a typed block is upstream-
                    // malformed; drop without canonical emission. The
                    // Unknown-block branch above is the only place
                    // opaque deltas are captured.
                    SseDelta::Other(_) => Ok(None),
                }
            }

            SseEvent::ContentBlockStop { index } => {
                if !self.index_matches(index, "stop", provider_id) {
                    return Ok(None);
                }
                // Strategy A terminal: emit ONE aggregated structured
                // detail per thinking block carrying both text and
                // signature, matching the non-streaming
                // `walk_content_blocks` shape so replay round-trips.
                let terminal_chunk = match self.open_block.take() {
                    Some(OpenBlockKind::Thinking {
                        accumulated,
                        signature,
                        detail_id,
                        detail_index,
                        ..
                    }) => {
                        // Edge case: empty thinking block (no text AND
                        // no signature). Skip emission so replay does
                        // not push an empty Thinking block that
                        // Anthropic would 400 on.
                        if accumulated.is_empty() && signature.is_none() {
                            None
                        } else {
                            // Build the aggregated detail via the shared
                            // helper so the streaming terminal shape stays
                            // structurally identical to the non-streaming
                            // `extract_tool_thinking` output. We clone it
                            // into completed_thinking before the chunk
                            // consumes it.
                            let detail = super::context_management::make_thinking_detail(
                                detail_id,
                                detail_index,
                                accumulated,
                                signature.unwrap_or_default(),
                            );
                            self.completed_thinking.push(detail.clone());
                            Some(ChatChunk {
                                id: self.id.clone(),
                                model: self.model.clone(),
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
                            })
                        }
                    }
                    Some(OpenBlockKind::Unknown { .. }) => {
                        // Append the stop sentinel and emit the per-block
                        // INFO summary. Capture state is consumed;
                        // pending_opaque rides on the next emitted chunk.
                        if let Some(mut capture) = self.current_capture.take() {
                            capture.record_stop(provider_id, &mut self.pending_opaque);
                        }
                        None
                    }
                    _ => None,
                };
                Ok(terminal_chunk)
            }

            SseEvent::MessageDelta { delta, usage } => {
                let finish_reason = map_stop_reason(delta.stop_reason.as_deref());
                // Lift the matched stop sequence so the Anthropic ingress
                // can render `stop_reason:"stop_sequence"` +
                // `stop_sequence:"<value>"` instead of the lossy
                // `end_turn` mapping. Mirrors the non-streaming path in
                // `response::normalize`. Only meaningful when
                // `stop_reason == "stop_sequence"`.
                let matched_stop_sequence = match delta.stop_reason.as_deref() {
                    Some("stop_sequence") => delta.stop_sequence.clone(),
                    _ => None,
                };
                // Anthropic emits input usage only on message_start, so
                // the closing chunk must carry it forward for OpenAI
                // clients to see full prompt_tokens.
                let captured = self.captured_input_usage.clone();
                let usage_delta = if usage.is_some() || captured.is_some() {
                    let cap = captured.as_ref();
                    // Prefer delta when present and non-zero; fall back
                    // to captured. Some(0) is "no info", not
                    // "authoritative zero" -- placeholder restatements
                    // must not blow away non-zero captured numbers.
                    let pick = |delta: Option<u32>, cap_v: Option<u32>| -> Option<u32> {
                        match (delta, cap_v) {
                            (Some(d), _) if d > 0 => Some(d),
                            (_, Some(c)) if c > 0 => Some(c),
                            // Both arms above guarded; the rest are
                            // zero-or-absent on both sides.
                            _ => None,
                        }
                    };
                    let cache_creation_input_tokens = pick(
                        usage.as_ref().and_then(|u| u.cache_creation_input_tokens),
                        cap.and_then(|c| c.cache_creation_input_tokens),
                    );
                    let cache_read_input_tokens = pick(
                        usage.as_ref().and_then(|u| u.cache_read_input_tokens),
                        cap.and_then(|c| c.cache_read_input_tokens),
                    );
                    // prompt_tokens = raw input + cache fields. Anthropic
                    // spec: input_tokens carries the raw input only; cache
                    // fields are separate. Sum them at the OpenAI seam.
                    // Picks the raw input from delta (real Anthropic does
                    // not currently emit input_tokens on message_delta;
                    // routectl-rendered upstreams now also emit raw) and
                    // falls back to message_start's captured value.
                    let raw_input = pick(
                        usage.as_ref().and_then(|u| u.input_tokens),
                        cap.map(|c| c.input_tokens),
                    );
                    let prompt_tokens = match (
                        raw_input,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    ) {
                        (None, None, None) => None,
                        (i, c1, c2) => Some(
                            i.unwrap_or(0)
                                .saturating_add(c1.unwrap_or(0))
                                .saturating_add(c2.unwrap_or(0)),
                        ),
                    };
                    let completion_tokens = usage.as_ref().and_then(|u| u.output_tokens);
                    let total_tokens = match (prompt_tokens, completion_tokens) {
                        (Some(p), Some(c)) => Some(p.saturating_add(c)),
                        (Some(p), None) => Some(p),
                        (None, Some(c)) => Some(c),
                        (None, None) => None,
                    };
                    // Per-TTL merge via the same `pick` so a delta with
                    // partial/empty `cache_creation` doesn't wholesale-
                    // replace the richer message_start object.
                    let delta_cc = usage.as_ref().and_then(|u| u.cache_creation.as_ref());
                    let cap_cc = cap.and_then(|c| c.cache_creation.as_ref());
                    let cache_creation_5m = pick(
                        delta_cc.and_then(|c| c.ephemeral_5m_input_tokens),
                        cap_cc.and_then(|c| c.ephemeral_5m_input_tokens),
                    );
                    let cache_creation_1h = pick(
                        delta_cc.and_then(|c| c.ephemeral_1h_input_tokens),
                        cap_cc.and_then(|c| c.ephemeral_1h_input_tokens),
                    );
                    let cache_creation =
                        if cache_creation_5m.is_some() || cache_creation_1h.is_some() {
                            Some(CacheCreation {
                                ephemeral_5m_input_tokens: cache_creation_5m,
                                ephemeral_1h_input_tokens: cache_creation_1h,
                            })
                        } else {
                            None
                        };
                    Some(UsageDelta {
                        prompt_tokens,
                        completion_tokens,
                        total_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                        cache_creation,
                        // Server-tool counts arrive on message_delta only
                        // (not message_start), so there is nothing
                        // captured to merge -- lift the delta value
                        // straight through.
                        server_tool_use: usage.as_ref().and_then(|u| u.server_tool_use.clone()),
                        ..Default::default()
                    })
                } else {
                    None
                };
                // Emit a chunk if either side carries information; an
                // empty MessageDelta (no stop_reason and no usage) is
                // a keepalive in spirit -- skip.
                if finish_reason.is_none() && usage_delta.is_none() {
                    return Ok(None);
                }
                Ok(Some(ChatChunk {
                    id: self.id.clone(),
                    model: self.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta::default(),
                        finish_reason,
                        matched_stop_sequence,
                    }],
                    usage: usage_delta,
                    opaque_events: Vec::new(),
                    upstream_meta: None,
                }))
            }

            SseEvent::MessageStop => {
                // Reset the per-turn thinking accumulator. pending_cache_writes
                // is intentionally NOT cleared here -- it is drained by the
                // stream() caller after the SSE pipeline finishes so the
                // post-stream cache-write tail can consume it.
                self.completed_thinking.clear();
                // Safety-net flush: if `pending_opaque` was never drained
                // (no canonical chunk emitted between the unknown block
                // closing and end-of-stream), emit a synthetic empty
                // carrier chunk so the buffered events still reach the
                // ingress. The wrapper drains pending into the chunk's
                // `opaque_events` after this returns.
                if self.pending_opaque.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(self.empty_carrier_chunk()))
                }
            }
            SseEvent::Ping => Ok(None),
            // Anthropic spec: a 200 response can carry an in-band error
            // event mid-stream (e.g. `overloaded_error`, `rate_limit_error`).
            // `error` is the inner object `{"type": ..., "message": ...}`.
            // Preserve `error.type` and map it to the synthetic status the
            // sync path would carry, so `failure_class::classify` and the
            // terminal-error classifier see the same structured facts as a
            // non-stream failure (streaming vs sync handling converges).
            SseEvent::Error { error } => {
                let err_type = error
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("api_error");
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("upstream signaled error event mid-stream");
                let status = crate::anthropic_error::anthropic_error_type_to_status(err_type);
                Err(Error::upstream_full(
                    provider_id,
                    status,
                    format!("{err_type}: {message}"),
                    None,
                    Some(err_type.to_string()),
                    None,
                ))
            }
            // Forward-compat catchall for unknown top-level event tags.
            // Top-level Other events are not captured into opaque_events
            // (the carrier is keyed on content_block lifecycle only); a
            // future event tag would need its own handling. For now,
            // sink-drain so the stream does not crash; emit a DEBUG so
            // a future Anthropic top-level event type is observable to
            // operators.
            SseEvent::Other(v) => {
                let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                tracing::debug!(
                    provider = %provider_id,
                    event_type = %event_type,
                    "anthropic SSE: unknown top-level event; sink-draining",
                );
                Ok(None)
            }
        }
    }

    // ------------------------------------------------------------------
    // Chunk constructors
    // ------------------------------------------------------------------

    /// Opening chunk carrying only `delta.role="assistant"`. Non-final,
    /// so `usage` and `finish_reason` stay absent (both skip-serialize
    /// when None), matching the peer lanes' opening chunk shape.
    fn role_chunk(&self) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
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

    fn make_text_chunk(&self, text: String) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
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

    /// Live thinking-string chunk for Strategy A. Carries only the
    /// plain `reasoning` string; the structured `ReasoningDetail` is
    /// deferred to `make_thinking_terminal_chunk` at
    /// `content_block_stop`. Streaming UIs reading `delta.reasoning`
    /// see thinking text incrementally; replay code reading
    /// `delta.reasoning_details` sees one fully-paired entry per
    /// block.
    fn make_thinking_string_chunk(&self, thinking: String) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
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

    /// Reverse a streamed `tool_use` name to the client's original via
    /// the per-request reverse map. A name present in the map is restored.
    /// A name with the `mcp__` shape but absent from the map is left as-is
    /// and bumps a debug-level unmatched-reverse counter (the cloak
    /// renamed nothing matching, so the client sent this name verbatim).
    /// Empty map = no-op.
    fn reverse_tool_name(&self, name: String, provider_id: &str) -> String {
        if self.tool_reverse.is_empty() {
            return name;
        }
        if let Some(original) = self.tool_reverse.get(&name) {
            return original.clone();
        }
        if name.starts_with("mcp__") {
            tracing::debug!(
                provider = %provider_id,
                "anthropic SSE tool_use name has mcp__ shape but is absent from the \
                 cloak reverse map; leaving unchanged",
            );
        }
        name
    }

    fn make_tool_delta_chunk(&self, partial_json: String) -> ChatChunk {
        let (tool_id, tool_name, call_index) = match &self.open_block {
            Some(OpenBlockKind::ToolUse {
                id,
                name,
                call_index,
                ..
            }) => (id.clone(), name.clone(), *call_index),
            _ => (String::new(), String::new(), 0),
        };

        let tool_call_delta: Value = json!({
            "index": call_index,
            "id": tool_id,
            "type": "function",
            "function": {"name": tool_name, "arguments": partial_json}
        });

        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
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

    /// Synthetic empty chunk used as the carrier on `MessageStop` when
    /// `pending_opaque` was never drained (no canonical chunk closed
    /// the unknown block before end-of-stream). The wrapper drains
    /// `pending_opaque` into this chunk's `opaque_events` after
    /// `dispatch_event` returns, so the buffered events still reach
    /// the matching ingress instead of being silently dropped.
    /// Choices are empty by design: there is no canonical content to
    /// translate; OpenAI-shape ingresses see a noop chunk.
    fn empty_carrier_chunk(&self) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: Vec::new(),
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }
}

// In-stream error and ping contract tests. Compiled as a child module
// of `sse` (via `#[path]`) so they retain access to private items
// while keeping this file under the project's 800-LOC ceiling.
#[cfg(test)]
#[path = "sse_event_tests.rs"]
mod sse_event_tests;

// Larger usage-accounting tests live in `sse_usage_tests.rs` so this
// file stays under the project's 800-LOC ceiling. Compiled as a child
// module of `sse` (via `#[path]`) so the tests retain access to
// private items like `CapturedInputUsage::prompt_tokens`.
#[cfg(test)]
#[path = "sse_usage_tests.rs"]
mod sse_usage_tests;

// Context-management SSE accumulator tests. Drives SseState
// through thinking + tool_use sequences and asserts pending_cache_writes
// and completed_thinking invariants.
#[cfg(test)]
#[path = "sse_context_management_tests.rs"]
mod sse_context_management_tests;
