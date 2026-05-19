//! OpenAI Responses SSE event state machine.
//!
//! The Responses API streams a typed sequence of `response.*` events
//! over SSE. Each event carries an `output_index` (when applicable)
//! that identifies WHICH output item the event applies to; output
//! items can interleave on the wire (a reasoning item can stream
//! summary deltas while a message item starts emitting text deltas in
//! parallel). The state machine here keeps a `HashMap<u32, BlockState>`
//! keyed on `output_index` so deltas route to the correct block.
//!
//! Block states (mirrors codex's per-item dispatch):
//! - `Text`     -- assistant message item; text deltas flow through
//! - `Reasoning` -- chain-of-thought item; summary + content deltas
//!   accumulate, encrypted_content flushes on item.done
//! - `ToolUse`  -- function_call item; argument deltas accumulate
//!
//! Indices (`call_index`, `detail_index`) are assigned dense (0, 1, 2,
//! ...) per stream via `next_call_index` / `next_detail_index`
//! counters on `ResponsesStreamState` so OpenAI SSE clients see stable
//! `tool_calls[].index` / `reasoning_details[].index`.
//!
//! Reference: `codex-rs/codex-api/src/sse/responses.rs:297-431` --
//! the event dispatch surface in codex.

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use routectl_core::{
    schema::{ChunkChoice, ChunkDelta, UsageDelta},
    ChatChunk, Error, ReasoningDetail, ReasoningDetailKind, Result, Role,
};

/// Discriminant returned by `BlockState::tag` for log-context strings.
const BLOCK_TAG_TEXT: &str = "text";
const BLOCK_TAG_REASONING: &str = "reasoning";
const BLOCK_TAG_TOOL_USE: &str = "tool_use";

/// Cap on the number of distinct `output_index` entries the per-stream
/// `ResponsesStreamState::blocks` map will hold. Legitimate Responses
/// streams emit a small handful of output items per turn (typically
/// reasoning, message, and a few tool calls). An adversarial or
/// compromised upstream could stream thousands of distinct indices
/// to drive the map toward OOM. 512 is comfortably above any
/// practical request and well below memory-pressure territory for
/// the per-task heap.
const MAX_OUTPUT_BLOCKS: usize = 512;

use super::response::{map_finish_reason, upstream_error_from_failed};
use super::response_types::{ResponsesResponse, ResponsesStreamEvent};
use super::OPENAI_RESPONSES_FORMAT;

/// Per-output-item streaming state.
#[derive(Debug, Clone)]
enum BlockState {
    /// Assistant message item streaming text deltas.
    Text {
        #[allow(dead_code)]
        item_id: String,
    },
    /// Reasoning item. Accumulates summary + content text for forward-
    /// compat tracing; the actual chunks are emitted as the deltas
    /// arrive. `encrypted_content` is captured at item.added (if
    /// pre-known) or item.done (more common) and emitted then.
    /// `item_id` is the upstream-stable id (e.g. "rs_1") used to stamp
    /// every emitted `ReasoningDetail.id` so multi-turn replay groups
    /// details back into one Reasoning input item.
    Reasoning {
        item_id: String,
        detail_index: u32,
        #[allow(dead_code)]
        summary_text: String,
        #[allow(dead_code)]
        content_text: String,
        encrypted_content: Option<String>,
    },
    /// Function-call item. `arguments` accumulator preserved across
    /// deltas so a re-emit on item.done is possible if needed; today
    /// the wire deltas already arrive as concatenable strings and we
    /// emit per-delta partial-arguments chunks.
    ToolUse {
        #[allow(dead_code)]
        item_id: String,
        call_id: String,
        name: String,
        call_index: u32,
        #[allow(dead_code)]
        arguments: String,
    },
}

impl BlockState {
    /// Short tag for log context. Cheap (no allocation) and stable
    /// across the lifetime of the state machine.
    fn tag(&self) -> &'static str {
        match self {
            BlockState::Text { .. } => BLOCK_TAG_TEXT,
            BlockState::Reasoning { .. } => BLOCK_TAG_REASONING,
            BlockState::ToolUse { .. } => BLOCK_TAG_TOOL_USE,
        }
    }
}

/// Persistent state across all SSE events for one streaming response.
#[derive(Debug, Default)]
pub(crate) struct ResponsesStreamState {
    /// Set from `response.created.response.id` (or the first event
    /// that carries an id). Threaded onto every emitted chunk so
    /// OpenAI SSE clients can correlate.
    pub(crate) response_id: String,
    /// Set from `response.created.response.model`.
    pub(crate) model: String,
    /// Per-output-item state. Keyed on `output_index`.
    blocks: HashMap<u32, BlockState>,
    /// Dense counter for OpenAI-shape `tool_calls[].index`.
    next_call_index: u32,
    /// Dense counter for `reasoning_details[].index`.
    next_detail_index: u32,
    /// True once `response.created` has been processed; used to emit
    /// the empty role chunk exactly once (parity with anthropic_api's
    /// message_start signal).
    created_emitted: bool,
    /// Sticky flag: set to `true` the first time `handle_item_added`
    /// sees a `function_call` item, and never reset for the lifetime
    /// of the stream. Consulted by `handle_completed` so the terminal
    /// `finish_reason` maps to `tool_calls` even if the
    /// `response.completed` body's `output` array is empty (a wire
    /// pattern the chatgpt-oauth backend has been observed emitting).
    /// We can't read this off `self.blocks` because `handle_item_done`
    /// reaps blocks per item-done event, so by `response.completed`
    /// the map is empty. Bug F (cc-via-* 2026-05-18).
    saw_function_call: bool,
}

impl ResponsesStreamState {
    /// Process one SSE event. Returns:
    ///   - `Ok(chunks)`: zero-or-more chunks to forward (empty for
    ///     housekeeping events like in_progress/output_text.done)
    ///   - `Err(_)`: a fatal stream error -- `response.failed` or a
    ///     malformed event payload. The caller terminates the stream.
    pub(crate) fn process_event(
        &mut self,
        provider_id: &str,
        event: ResponsesStreamEvent,
    ) -> Result<Vec<ChatChunk>> {
        let kind = event.kind.clone();
        match kind.as_str() {
            "response.created" => Ok(self.handle_created(&event)),
            "response.in_progress" => Ok(Vec::new()),
            "response.output_item.added" => Ok(self.handle_item_added(provider_id, &event)),
            "response.output_text.delta" => Ok(self.handle_text_delta(provider_id, &event)),
            "response.output_text.done" => Ok(Vec::new()),
            "response.reasoning_summary_text.delta" => {
                Ok(self.handle_reasoning_summary_delta(&event))
            }
            "response.reasoning_text.delta" => Ok(self.handle_reasoning_text_delta(&event)),
            "response.reasoning_summary_part.added" => Ok(Vec::new()),
            "response.function_call_arguments.delta" => Ok(self.handle_function_call_delta(&event)),
            "response.function_call_arguments.done" => Ok(Vec::new()),
            "response.output_item.done" => Ok(self.handle_item_done(&event)),
            "response.completed" => Ok(self.handle_completed(&event)),
            "response.incomplete" => Ok(self.handle_incomplete(&event)),
            "response.failed" => Err(self.handle_failed(provider_id, &event)),
            "response.cancelled" => Ok(self.handle_cancelled(&event)),
            other => {
                // Forward compat: a new event kind ships without a
                // rebuild. DEBUG (not WARN) because OpenAI adds new
                // event kinds frequently and WARN would flood the log.
                tracing::debug!(
                    provider = provider_id,
                    event_type = other,
                    "openai-responses: skipping unknown stream event"
                );
                Ok(Vec::new())
            }
        }
    }

    // ------------------------------------------------------------------
    // Event handlers
    // ------------------------------------------------------------------

    fn handle_created(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        if let Some(resp) = event.response.as_ref() {
            if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                self.response_id = id.to_string();
            }
            if let Some(model) = resp.get("model").and_then(|v| v.as_str()) {
                self.model = model.to_string();
            }
        }
        if self.created_emitted {
            return Vec::new();
        }
        self.created_emitted = true;
        // Parity with anthropic_api `message_start`: emit an empty
        // role chunk so OpenAI SSE clients see the start-of-stream
        // signal before any content deltas.
        vec![ChatChunk {
            id: self.response_id.clone(),
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
        }]
    }

    fn handle_item_added(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Vec<ChatChunk> {
        let Some(idx) = event.output_index else {
            tracing::debug!(
                provider = provider_id,
                "openai-responses: output_item.added without output_index"
            );
            return Vec::new();
        };
        let Some(item) = event.item.as_ref() else {
            return Vec::new();
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Set the sticky `saw_function_call` flag BEFORE the
        // bounded-growth cap check below. If the cap-skip fires on
        // a function_call item we still must remember the fact so
        // `handle_completed` maps the terminal status to
        // `tool_calls` (Bug F). Without this ordering, an
        // adversarial-or-extreme stream that pushes >512 reasoning
        // items before a function_call would silently report
        // `finish_reason="stop"`.
        if item_type == "function_call" {
            self.saw_function_call = true;
        }

        // Bounded-growth guard: AWS / OpenAI legitimate responses
        // emit a small handful of output items per turn (typically
        // 1-5: optional reasoning + message + N tool calls). An
        // adversarial upstream could stream thousands of distinct
        // `output_index` values to grow the blocks map unboundedly.
        // Skip past the cap with a debug log; the stream remains
        // usable for items below the cap.
        if !self.blocks.contains_key(&idx) && self.blocks.len() >= MAX_OUTPUT_BLOCKS {
            tracing::debug!(
                provider = provider_id,
                output_index = idx,
                cap = MAX_OUTPUT_BLOCKS,
                "openai-responses: output_item.added beyond cap; skipping"
            );
            return Vec::new();
        }

        match item_type {
            "message" => {
                self.blocks.insert(idx, BlockState::Text { item_id });
            }
            "reasoning" => {
                let detail_index = self.next_detail_index;
                self.next_detail_index += 1;
                let encrypted_content = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                self.blocks.insert(
                    idx,
                    BlockState::Reasoning {
                        item_id,
                        detail_index,
                        summary_text: String::new(),
                        content_text: String::new(),
                        encrypted_content,
                    },
                );
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let call_index = self.next_call_index;
                self.next_call_index += 1;
                self.blocks.insert(
                    idx,
                    BlockState::ToolUse {
                        item_id,
                        call_id,
                        name,
                        call_index,
                        arguments: String::new(),
                    },
                );
                // The sticky `saw_function_call` flag was already set
                // above (pre-cap) so this branch does not duplicate it.
            }
            _ => {
                tracing::debug!(
                    provider = provider_id,
                    item_type = item_type,
                    "openai-responses: unknown output item type at item.added"
                );
            }
        }
        Vec::new()
    }

    fn handle_text_delta(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Vec<ChatChunk> {
        let Some(delta) = event.delta.as_deref() else {
            return Vec::new();
        };
        // Gate on a live Text block at the event's `output_index`. A
        // text delta for an unknown index or a non-Text block is a
        // wire-protocol bug (or a never-before-seen interleaving) and
        // routing it through the Text accumulator would corrupt the
        // assistant's output. Drop with a debug log so forward-compat
        // surprises are visible during triage without flooding WARN.
        let Some(output_index) = event.output_index else {
            tracing::debug!(
                provider = provider_id,
                "openai-responses: output_text.delta without output_index; ignoring"
            );
            return Vec::new();
        };
        let Some(state) = self.blocks.get(&output_index) else {
            tracing::debug!(
                provider = provider_id,
                output_index,
                "openai-responses: output_text.delta for unknown block; ignoring"
            );
            return Vec::new();
        };
        if !matches!(state, BlockState::Text { .. }) {
            tracing::debug!(
                provider = provider_id,
                output_index,
                block = state.tag(),
                "openai-responses: output_text.delta for non-Text block; ignoring"
            );
            return Vec::new();
        }
        vec![self.text_chunk(delta.to_string())]
    }

    fn handle_reasoning_summary_delta(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        let Some(delta) = event.delta.as_deref() else {
            return Vec::new();
        };
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        let (detail_index, detail_id) = match self.blocks.get_mut(&idx) {
            Some(BlockState::Reasoning {
                detail_index,
                summary_text,
                item_id,
                ..
            }) => {
                summary_text.push_str(delta);
                (*detail_index, item_id.clone())
            }
            _ => return Vec::new(),
        };
        vec![self.reasoning_summary_chunk(&detail_id, detail_index, delta.to_string())]
    }

    fn handle_reasoning_text_delta(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        let Some(delta) = event.delta.as_deref() else {
            return Vec::new();
        };
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        let (detail_index, detail_id) = match self.blocks.get_mut(&idx) {
            Some(BlockState::Reasoning {
                detail_index,
                content_text,
                item_id,
                ..
            }) => {
                content_text.push_str(delta);
                (*detail_index, item_id.clone())
            }
            _ => return Vec::new(),
        };
        vec![self.reasoning_text_chunk(&detail_id, detail_index, delta.to_string())]
    }

    fn handle_function_call_delta(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        let Some(delta) = event.delta.as_deref() else {
            return Vec::new();
        };
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        let (call_id, name, call_index) = match self.blocks.get_mut(&idx) {
            Some(BlockState::ToolUse {
                call_id,
                name,
                call_index,
                arguments,
                ..
            }) => {
                arguments.push_str(delta);
                (call_id.clone(), name.clone(), *call_index)
            }
            _ => return Vec::new(),
        };
        vec![self.tool_delta_chunk(call_id, name, call_index, delta.to_string())]
    }

    fn handle_item_done(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        let mut chunks = Vec::new();
        // The final item shape may include the `encrypted_content`
        // signature even when item.added didn't carry it (typical
        // shape: signature is computed server-side after reasoning
        // completes). Pull it from event.item.encrypted_content.
        let server_encrypted = event
            .item
            .as_ref()
            .and_then(|v| v.get("encrypted_content"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(BlockState::Reasoning {
            detail_index,
            encrypted_content,
            item_id,
            ..
        }) = self.blocks.get(&idx)
        {
            let detail_index = *detail_index;
            let detail_id = item_id.clone();
            let sig = server_encrypted.or_else(|| encrypted_content.clone());
            if let Some(sig) = sig {
                chunks.push(self.reasoning_signature_chunk(&detail_id, detail_index, sig));
            }
        }
        self.blocks.remove(&idx);
        chunks
    }

    fn handle_completed(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        // `response` shape on `response.completed` mirrors the
        // non-streaming body. We deserialize via the same type so the
        // finish_reason + usage logic stays exactly aligned with the
        // non-streaming translator.
        let resp_value = event.response.clone().unwrap_or(Value::Null);
        let resp: ResponsesResponse = serde_json::from_value(resp_value).unwrap_or_default_resp();

        // `has_function_call` decides whether a `completed` status
        // maps to `tool_calls` (when present) or `stop` (when absent).
        // Two sources, logically OR'd:
        //   (a) walk `resp.output` from the terminal `response.completed`
        //       event body -- matches the non-streaming translator's
        //       `walk_output` invariant.
        //   (b) the `saw_function_call` sticky flag, set in
        //       `handle_item_added` when a `function_call` item first
        //       opens.
        // (a) alone is not enough: the chatgpt-oauth backend has been
        // seen to emit `response.completed` with an empty / missing
        // `response.output` field on streaming responses, so a turn
        // with N function_calls would report `finish_reason="stop"`.
        // We can't consult `self.blocks` for the same fact -- the
        // map is reaped per item-done event so by `response.completed`
        // it is empty. Bug F (cc-via-* 2026-05-18).
        let has_function_call_in_body = resp.output.iter().any(|i| {
            matches!(
                i,
                super::response_types::ResponseOutputItem::FunctionCall { .. }
            )
        });
        let has_function_call = has_function_call_in_body || self.saw_function_call;
        let incomplete_reason = resp
            .incomplete_details
            .as_ref()
            .and_then(|d| d.reason.as_deref());
        let finish_reason =
            map_finish_reason(resp.status.as_deref(), incomplete_reason, has_function_call);

        let usage_delta = resp.usage.as_ref().map(|u| {
            let cache_read = u
                .input_tokens_details
                .as_ref()
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            let reasoning_tokens = u
                .output_tokens_details
                .as_ref()
                .and_then(|v| v.get("reasoning_tokens"))
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            UsageDelta {
                prompt_tokens: Some(u.input_tokens),
                completion_tokens: Some(u.output_tokens),
                total_tokens: Some(u.total_tokens),
                reasoning_tokens,
                cache_read_input_tokens: cache_read,
                ..Default::default()
            }
        });

        if !resp.id.is_empty() {
            self.response_id = resp.id;
        }
        if !resp.model.is_empty() {
            self.model = resp.model;
        }

        vec![ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason,
                matched_stop_sequence: None,
            }],
            usage: usage_delta,
        }]
    }

    fn handle_incomplete(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        // Same code path as `completed` -- the body carries
        // status="incomplete" and the translator does the right thing.
        self.handle_completed(event)
    }

    fn handle_failed(&mut self, provider_id: &str, event: &ResponsesStreamEvent) -> Error {
        // Try the structured ResponseFailed envelope first.
        if let Some(resp_value) = event.response.clone() {
            if let Ok(resp) = serde_json::from_value::<ResponsesResponse>(resp_value) {
                return upstream_error_from_failed(provider_id, &resp);
            }
        }
        Error::upstream(
            provider_id,
            0,
            "openai-responses: response.failed".to_string(),
        )
    }

    fn handle_cancelled(&mut self, event: &ResponsesStreamEvent) -> Vec<ChatChunk> {
        // Cancelled emits a terminal chunk with finish_reason="error"
        // so the client sees a clean termination. Same code path as
        // completed -- the body translator does the mapping.
        self.handle_completed(event)
    }

    // ------------------------------------------------------------------
    // Chunk constructors
    // ------------------------------------------------------------------

    fn text_chunk(&self, text: String) -> ChatChunk {
        ChatChunk {
            id: self.response_id.clone(),
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
        }
    }

    fn reasoning_summary_chunk(
        &self,
        detail_id: &str,
        detail_index: u32,
        text: String,
    ) -> ChatChunk {
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Summary,
            id: Some(stable_or_minted_id(detail_id)),
            format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
            index: Some(detail_index),
            payload: json!({"text": text.clone()}),
        };
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning: Some(text),
                    reasoning_details: vec![detail],
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
        }
    }

    fn reasoning_text_chunk(&self, detail_id: &str, detail_index: u32, text: String) -> ChatChunk {
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some(stable_or_minted_id(detail_id)),
            format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
            index: Some(detail_index),
            payload: json!({"text": text.clone()}),
        };
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning: Some(text),
                    reasoning_details: vec![detail],
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
        }
    }

    fn reasoning_signature_chunk(
        &self,
        detail_id: &str,
        detail_index: u32,
        signature: String,
    ) -> ChatChunk {
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some(stable_or_minted_id(detail_id)),
            format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
            index: Some(detail_index),
            payload: json!({"encrypted_content": signature}),
        };
        ChatChunk {
            id: self.response_id.clone(),
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
        }
    }

    fn tool_delta_chunk(
        &self,
        call_id: String,
        name: String,
        call_index: u32,
        partial_json: String,
    ) -> ChatChunk {
        let tool_call_delta: Value = json!({
            "index": call_index,
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": partial_json}
        });
        ChatChunk {
            id: self.response_id.clone(),
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
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the upstream-stable id when non-empty, otherwise mint a fresh
/// v4 UUID. Some upstreams omit the item id on early `item.added`
/// frames; minting avoids `id == ""` on the canonical ReasoningDetail
/// (which downstream consumers treat as "no id").
fn stable_or_minted_id(item_id: &str) -> String {
    if item_id.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        item_id.to_string()
    }
}

/// Parse one raw `data:` SSE payload into a typed event. A parse error
/// returns `Err(Error::Streaming)` so the stream terminates -- a
/// malformed event on the Responses surface is not a recoverable
/// condition (codex itself returns a Stream error in this case at
/// `codex-rs/codex-api/src/sse/responses.rs:473-479`).
pub(crate) fn parse_data_line(provider_id: &str, data: &str) -> Result<ResponsesStreamEvent> {
    serde_json::from_str(data).map_err(|e| {
        Error::Streaming(format!(
            "openai-responses provider `{provider_id}`: bad SSE json: {e}"
        ))
    })
}

/// Extension trait for `serde_json::Result<ResponsesResponse>` so the
/// completed-event handler can produce a default body on parse failure
/// without ever panicking. Default body has status=None which falls
/// through map_finish_reason to None.
trait UnwrapOrDefaultResp {
    fn unwrap_or_default_resp(self) -> ResponsesResponse;
}

impl UnwrapOrDefaultResp for serde_json::Result<ResponsesResponse> {
    fn unwrap_or_default_resp(self) -> ResponsesResponse {
        self.unwrap_or(ResponsesResponse {
            id: String::new(),
            _object: None,
            created_at: 0,
            status: None,
            error: None,
            incomplete_details: None,
            model: String::new(),
            output: Vec::new(),
            usage: None,
        })
    }
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
