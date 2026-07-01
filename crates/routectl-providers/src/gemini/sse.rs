//! Gemini `streamGenerateContent` SSE state machine.
//!
//! With `?alt=sse` the streaming endpoint emits a sequence of
//! `data: {GenerateContentResponse}` events. Each event is a PARTIAL
//! response whose `candidates[0].content.parts` carry incremental text,
//! thought (reasoning), and/or functionCall content; the final event(s)
//! carry `finishReason` and `usageMetadata`. This module translates that
//! event sequence into canonical `ChatChunk`s, mirroring the
//! openai_responses SSE drain:
//!
//!   - one opening role chunk (emitted on the first event),
//!   - per-part text / reasoning / tool-call delta chunks,
//!   - a terminal chunk carrying `finish_reason` + usage when a
//!     `finishReason` lands.
//!
//! Reference: <https://ai.google.dev/api/generate-content>

use serde_json::{Value, json};

use routectl_core::{
    ChatChunk, Error, ReasoningDetail, ReasoningDetailKind, Result, Role,
    schema::{ChunkChoice, ChunkDelta, UsageDelta},
};

use super::GEMINI_FORMAT;
use super::response::map_finish_reason;
use super::types::{GenerateContentResponse, ResponsePart, UsageMetadata};

/// Per-stream cap on the number of distinct functionCall blocks the
/// state machine will assign an index to. A legitimate turn emits a
/// small handful of tool calls; an adversarial upstream could stream
/// thousands to drive unbounded `next_call_index` growth and flood the
/// downstream tool_calls array. 4096 is well above any legitimate
/// breadth. Mirrors the openai_responses `MAX_OUTPUT_BLOCKS` guard.
pub(super) const MAX_OUTPUT_BLOCKS: u32 = 4096;

/// Persistent state across all SSE events for one streaming response.
#[derive(Debug, Default)]
pub struct GeminiStreamState {
    /// Response id (Gemini's `responseId`), threaded onto every chunk.
    response_id: String,
    /// Returned model id (`modelVersion`).
    model: String,
    /// Emitted once, before any content deltas.
    role_emitted: bool,
    /// Dense counter for OpenAI-shape `tool_calls[].index`.
    next_call_index: u32,
    /// Dense counter for `reasoning_details[].index`.
    next_detail_index: u32,
    /// Sticky: set once any functionCall part is seen, so the terminal
    /// chunk maps to `tool_calls` even if the finishReason says STOP.
    saw_function_call: bool,
    /// Set once a terminal chunk (finishReason and/or usage) has fired.
    /// Guards against a trailing usage-only event emitting a second
    /// terminal chunk or post-terminal content.
    terminal_emitted: bool,
}

impl GeminiStreamState {
    /// Process one partial `GenerateContentResponse`. Returns zero-or-more
    /// chunks to forward.
    pub(crate) fn parse_event(
        &mut self,
        provider_id: &str,
        event: GenerateContentResponse,
    ) -> Result<Vec<ChatChunk>> {
        let mut chunks = Vec::new();

        if let Some(id) = event.response_id.as_deref()
            && !id.is_empty()
        {
            self.response_id = id.to_string();
        }
        if let Some(model) = event.model_version.as_deref()
            && !model.is_empty()
        {
            self.model = model.to_string();
        }

        // Once the stream has terminated, ignore any trailing events
        // (e.g. a usage-only keepalive after the finishReason event) so
        // we never emit a second terminal chunk or post-terminal content.
        if self.terminal_emitted {
            return Ok(chunks);
        }

        if !self.role_emitted {
            self.role_emitted = true;
            chunks.push(self.role_chunk());
        }

        let candidate = event.candidates.into_iter().next();
        let finish_reason_raw = candidate.as_ref().and_then(|c| c.finish_reason.clone());

        if let Some(cand) = candidate {
            let parts = cand.content.map(|c| c.parts).unwrap_or_default();
            for part in &parts {
                chunks.extend(self.part_chunks(provider_id, part));
            }
        }

        // A finishReason (and/or usageMetadata) marks the terminal event.
        if finish_reason_raw.is_some() || event.usage_metadata.is_some() {
            self.terminal_emitted = true;
            chunks.push(self.terminal_chunk(finish_reason_raw.as_deref(), event.usage_metadata));
        }

        Ok(chunks)
    }

    fn part_chunks(&mut self, provider_id: &str, part: &ResponsePart) -> Vec<ChatChunk> {
        let mut out = Vec::new();
        let is_thought = part.thought == Some(true);

        if let Some(text) = &part.text {
            if is_thought {
                out.push(self.reasoning_chunk(text, part.thought_signature.as_deref()));
            } else if !text.is_empty() {
                out.push(self.text_chunk(text.clone()));
            }
        }

        if let Some(fc) = &part.function_call {
            self.saw_function_call = true;
            if self.next_call_index >= MAX_OUTPUT_BLOCKS {
                tracing::debug!(
                    provider = %provider_id,
                    cap = MAX_OUTPUT_BLOCKS,
                    "gemini: functionCall beyond cap; skipping"
                );
                return out;
            }
            let call_index = self.next_call_index;
            self.next_call_index += 1;
            let args_str = serde_json::to_string(&fc.args).unwrap_or_else(|_| "{}".to_string());
            out.push(self.tool_chunk(&fc.name, call_index, args_str));
        }

        out
    }

    // ------------------------------------------------------------------
    // Chunk constructors
    // ------------------------------------------------------------------

    fn role_chunk(&self) -> ChatChunk {
        self.chunk_with_delta(ChunkDelta {
            role: Some(Role::Assistant),
            ..Default::default()
        })
    }

    fn text_chunk(&self, text: String) -> ChatChunk {
        self.chunk_with_delta(ChunkDelta {
            content: Some(text),
            ..Default::default()
        })
    }

    fn reasoning_chunk(&mut self, text: &str, signature: Option<&str>) -> ChatChunk {
        let detail_index = self.next_detail_index;
        self.next_detail_index += 1;
        let mut payload = json!({ "text": text });
        if let Some(sig) = signature {
            payload["thought_signature"] = json!(sig);
        }
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: None,
            format: Some(GEMINI_FORMAT.to_string()),
            index: Some(detail_index),
            payload,
        };
        self.chunk_with_delta(ChunkDelta {
            reasoning: Some(text.to_string()),
            reasoning_details: vec![detail],
            ..Default::default()
        })
    }

    fn tool_chunk(&self, name: &str, call_index: u32, args: String) -> ChatChunk {
        let tool_call_delta: Value = json!({
            "index": call_index,
            "id": format!("call_{call_index}"),
            "type": "function",
            "function": {"name": name, "arguments": args}
        });
        self.chunk_with_delta(ChunkDelta {
            tool_calls: Some(vec![tool_call_delta]),
            ..Default::default()
        })
    }

    fn terminal_chunk(
        &self,
        finish_reason: Option<&str>,
        usage_meta: Option<UsageMetadata>,
    ) -> ChatChunk {
        let finish = map_finish_reason(finish_reason, self.saw_function_call);
        let usage = usage_meta.map(|m| usage_delta(&m));
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: finish,
                matched_stop_sequence: None,
            }],
            usage,
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }

    fn chunk_with_delta(&self, delta: ChunkDelta) -> ChatChunk {
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }
}

/// Map Gemini `usageMetadata` to a canonical streaming `UsageDelta`.
fn usage_delta(meta: &UsageMetadata) -> UsageDelta {
    UsageDelta {
        prompt_tokens: Some(meta.prompt_token_count),
        completion_tokens: Some(meta.candidates_token_count),
        total_tokens: Some(meta.total_token_count),
        reasoning_tokens: (meta.thoughts_token_count > 0).then_some(meta.thoughts_token_count),
        cache_read_input_tokens: (meta.cached_content_token_count > 0)
            .then_some(meta.cached_content_token_count),
        ..Default::default()
    }
}

/// Parse one raw `data:` SSE payload into a partial `GenerateContentResponse`.
/// A parse error returns `Err(Error::Streaming)` so the stream terminates --
/// a malformed event on the streaming surface is not recoverable.
pub fn parse_data_line(provider_id: &str, data: &str) -> Result<GenerateContentResponse> {
    serde_json::from_str(data).map_err(|e| {
        Error::Streaming(format!(
            "gemini provider `{provider_id}`: bad SSE json: {e}"
        ))
    })
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
