//! OpenAI Responses API ingress (`POST /v1/responses`).
//!
//! Translates an OpenAI Responses request body into the canonical
//! `ChatRequest`. This is roughly the INVERSE of the openai-responses
//! EGRESS (`routectl_providers::openai_responses`): the egress turns a
//! canonical request into a Responses wire body, while this ingress
//! reads a Responses wire body (what a Codex client sends) and produces
//! the canonical hub shape.
//!
//! The Responses API has no role-tagged message envelope: each `input[]`
//! entry is a top-level tagged union (`message` / `reasoning` /
//! `function_call` / `function_call_output`). `parse::translate_request`
//! flattens that union back into canonical `messages[]` plus a
//! `system` lifted from the top-level `instructions` field.
//!
//! Statefulness contract (see `parse.rs`): routectl is stateless. A
//! request carrying `previous_response_id` is rejected with a 4xx
//! because it relies on server-side conversation state routectl never
//! holds; answering anyway would be a silent wrong answer. A request
//! carrying `store: true` is accepted -- the current turn is
//! self-contained -- but the persistence is ignored with a WARN.
//!
//! Slices: this module is built in stages. SLICE 1 (this file) delivers
//! the module foundation, `parse_request`, and the statefulness
//! contract. The non-streaming renderer (SLICE 2) and the SSE stream
//! state machine (SLICE 3) replace the temporary stubs below.

use std::any::Any;

use axum::http::HeaderMap;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Result};
use serde_json::Value;

use super::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamErrorClass,
    StreamRequestContext,
};

mod parse;
#[cfg(test)]
#[path = "parse_tests.rs"]
mod parse_tests;
mod render;
mod stream;

use parse::translate_request;

/// The reasoning-detail format tag routectl uses for Responses-shape
/// reasoning history. Mirrors the egress constant
/// `routectl_providers::openai_responses::OPENAI_RESPONSES_FORMAT`
/// (`pub(crate)` over there, so it cannot be imported across the crate
/// boundary). The ingress stamps inbound `reasoning` items with this tag
/// so the egress's reasoning-replay path recognizes them on the next
/// turn. Kept in lockstep with the egress spelling by hand; a divergence
/// would silently break reasoning replay.
pub(super) const OPENAI_RESPONSES_FORMAT: &str = "openai-responses-v1";

/// OpenAI Responses ingress adapter.
#[derive(Debug, Default)]
pub struct ResponsesIngress;

// ---------------------------------------------------------------------------
// Streaming state
// ---------------------------------------------------------------------------

/// Per-stream state for the Responses SSE renderer.
///
/// The Responses streaming protocol is event-named (like Anthropic, not
/// like OpenAI Chat Completions): the server emits
/// `response.created`, `response.output_item.added`,
/// `response.output_text.delta`, `response.reasoning_summary_text.delta`,
/// `response.function_call_arguments.delta`,
/// `response.output_item.done`, `response.completed`, etc. -- every
/// event carries a monotonic `sequence_number` and most carry an
/// `output_index`. The renderer runs a state machine that maps
/// canonical `ChatChunk`s onto that event sequence, mirroring the egress
/// `sse.rs` reader in reverse and the anthropic ingress
/// `AnthropicStreamState` in spirit.
///
/// The canonical delta stream is single-choice and delta-only: at most
/// one message/reasoning item is "open" at a time, and tool calls
/// buffer by index then flush together (mirroring how the anthropic
/// ingress buffers `tool_blocks` and flushes them at the terminal
/// chunk). Item boundaries are synthesized: a new kind supersedes and
/// closes the prior open item; everything left open flushes at EOS.
#[derive(Debug, Default)]
pub struct ResponsesStreamState {
    /// Have we emitted the opening `response.created` event yet? Set on
    /// the first chunk.
    started: bool,
    /// True once the terminal `response.completed` / `response.failed`
    /// event was emitted. Idempotency guard for `render_eos` /
    /// `render_error_eos`.
    finished: bool,
    /// Monotonic `sequence_number` counter stamped on every emitted
    /// event (the Responses protocol requires it to increase by one per
    /// event across the whole stream).
    sequence_number: u64,
    /// Next `output_index` to allocate for a new output item.
    next_output_index: u64,
    /// The single currently-open text/reasoning output item, if any.
    /// Tool calls are buffered separately in `tool_buffers` and flushed
    /// together, so they do not occupy this slot.
    open: Option<OpenOutputItem>,
    /// Buffered function-call items keyed by the canonical tool_call
    /// `index`. Accumulates id/name/arguments across deltas; flushed as
    /// `output_item.added` -> `function_call_arguments.delta` -> `.done`
    /// -> `output_item.done` at the terminal chunk / EOS.
    tool_buffers: Vec<ToolCallBuffer>,
    /// Response id echoed on every event (`resp_...`); cached from the
    /// first chunk or synthesized when the upstream omitted one.
    response_id: Option<String>,
    /// Model label echoed on `response.created` / `response.completed`.
    response_model: Option<String>,
    /// `created_at` echoed on the response object. Captured as 0 when
    /// the canonical chunk carries no timestamp (canonical chunks do
    /// not model `created`), matching the non-stream renderer's handling
    /// of a zero `ChatResponse.created`.
    created_at: i64,
    /// Buffered `finish_reason` from a terminal chunk, flushed into the
    /// `response.completed` status at EOS (mirrors the anthropic
    /// ingress `pending_finish_reason`).
    pending_finish_reason: Option<String>,
    /// Accumulated usage from the terminal/usage chunk, rendered into
    /// the `response.completed` body.
    pending_usage: Option<routectl_core::UsageDelta>,
    /// Accumulated assistant text, replayed into the `response.completed`
    /// body so it matches the non-stream render byte-for-byte. Cumulative
    /// across the whole stream (the non-stream render concatenates all
    /// assistant text into one message).
    text_accumulator: String,
    /// Text streamed into the CURRENT open message item, reset each time
    /// a new message item opens. Drives the per-item `output_text.done`
    /// body so a superseded item closes with only its own text.
    current_text: String,
    /// Accumulated reasoning details (summary / text / encrypted),
    /// replayed into the completed body's `reasoning` items.
    reasoning_accumulator: Vec<routectl_core::ReasoningDetail>,
}

/// One open Responses output item, tagged with the canonical channel
/// feeding it, carrying the dense `output_index` it was allocated.
#[derive(Debug, Clone)]
enum OpenOutputItem {
    /// An assistant `message` item streaming `output_text` deltas. The
    /// message-level `output_index` and the `content_index` of its
    /// single text part are tracked so deltas and the closing
    /// `output_text.done` / `content_part.done` carry the right indices.
    Text { output_index: u64 },
    /// A `reasoning` item streaming summary / text deltas. `detail_id`
    /// groups emitted details (matches slice 2's id-grouping); the
    /// summary/text detail payloads accumulate so the closing
    /// `output_item.done` carries the full reasoning item. `summary_index`
    /// / `content_index` are per-item part counters: they advance once per
    /// streamed-native Summary / Text detail so each delta carries the
    /// part index a strict Responses client keys on, matching the
    /// completed body's `summary[]` / `content[]` ordering.
    Reasoning {
        output_index: u64,
        detail_id: Option<String>,
        summary_index: u64,
        content_index: u64,
    },
}

/// Buffered function-call item under construction across argument
/// deltas. Mirrors the anthropic ingress `ToolBlockState`.
#[derive(Debug, Default, Clone)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

impl IngressStreamState for ResponsesStreamState {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

impl IngressAdapter for ResponsesIngress {
    fn id(&self) -> &'static str {
        "openai-responses"
    }

    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        // Responses clients (Codex, the OpenAI SDK) parse the flat
        // OpenAI error envelope `{"error":{"message","type","code"}}`.
        ErrorEnvelopeShape::OpenAi
    }

    fn parse_request(&self, headers: &HeaderMap, body: &[u8]) -> Result<ChatRequest> {
        // Materialize the wire body once from the raw request bytes.
        // The Responses dialect keeps its Value-based walk (its
        // pre-deserialization mutations are load-bearing forward-compat
        // surface) -- neutral vs the prior extractor-owned parse. A
        // top-level syntax error surfaces as `Error::Json`.
        let body: Value = serde_json::from_slice(body)?;
        // Trace-level ingress body for triage; inherits the parent
        // span's request_id and honors ROUTECTL_LOG_REDACT_PROMPTS=1.
        // Mirrors the openai / anthropic ingress.
        routectl_core::trace_ingress_body("openai-responses", &body);
        // Companion structural summary -- one TRACE line of stable,
        // prompt-content-free fields for the smart-heartbeat validator.
        routectl_core::trace_structural_summary("ingress", "ingress", "openai-responses", &body);
        translate_request(headers, body)
    }

    fn render_response(&self, resp: ChatResponse) -> Result<Value> {
        render::render_responses_response(resp)
    }

    fn new_stream_state(&self, _ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        Box::new(ResponsesStreamState::default())
    }

    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        stream::render_chunk_internal(chunk, stream::state_mut(state))
    }

    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        stream::render_eos_internal(stream::state_mut(state))
    }

    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        stream::render_error_eos_internal(stream::state_mut(state), error, class)
    }
}
