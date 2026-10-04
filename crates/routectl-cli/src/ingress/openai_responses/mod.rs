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
//! Statefulness contract (see `parse.rs` + `store.rs`): the server
//! wiring (`ResponsesIngress::with_store`) resolves
//! `previous_response_id` against a bounded in-memory response store
//! and persists `store: true` turns into it, matching the official
//! API's server-side conversation semantics (full-context replay, not
//! last-output-only). The store is process-local: a restart starts
//! cold and a chain to a pre-restart id fails with a clear 400. A
//! storeless build (`Default` -- library consumers, unit tests) keeps
//! the historical stateless contract: chaining is a hard 400 and
//! `store: true` warns that nothing was kept.
//!
//! Layout: this file holds the adapter surface, request parsing, and the
//! statefulness contract. `render.rs` holds the non-streaming renderer
//! and `stream.rs` the SSE stream state machine.

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
#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
mod store;
mod stream;

pub use store::ResponsesStore;

use std::sync::Arc;

use parse::translate_request;

/// OpenAI Responses ingress adapter.
///
/// When built with `with_store` (the server wiring), the adapter
/// resolves `previous_response_id` against the bounded response store
/// and persists `store: true` turns into it. The `Default` (storeless)
/// form keeps the historical stateless contract: chaining fails with a
/// clear 400 and `store: true` warns that nothing was kept -- the shape
/// library consumers and unit tests use.
#[derive(Debug, Default)]
pub struct ResponsesIngress {
    store: Option<Arc<ResponsesStore>>,
}

impl ResponsesIngress {
    pub fn with_store(store: Arc<ResponsesStore>) -> Self {
        Self {
            store: Some(store),
        }
    }
}

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
#[derive(Debug)]
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
    /// Counter for minted item ids (`msg_N`); official item ids are
    /// per-response sequential. The egress forwards upstream item ids
    /// when the upstream assigns them (reasoning items on the chatgpt
    /// lane always carry one), so this mints only what the upstream
    /// left id-less.
    next_item_id: u64,
    /// The minted id of the most recent open message item, patched
    /// into the completed body's message item so a client replaying
    /// the completed output sees the same id the stream events
    /// carried.
    last_message_id: Option<String>,
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
    /// The canonical request this stream serves, seeded by the adapter's
    /// `new_stream_state`. Drives the request-parameter echo on the
    /// `response.created` / `response.completed` bodies and the
    /// store-insert context at terminal flush. `None` in tests that
    /// build the state directly.
    req: Option<Arc<routectl_core::ChatRequest>>,
    /// The response store to persist a `store: true` turn into at
    /// terminal flush. `None` (storeless adapter / tests) -> no write.
    store: Option<Arc<ResponsesStore>>,
}

/// Manual `Default` because `Arc<ChatRequest>` has no `Default`: a
/// default state carries no request and no store.
impl Default for ResponsesStreamState {
    fn default() -> Self {
        Self {
            started: false,
            finished: false,
            sequence_number: 0,
            next_output_index: 0,
            next_item_id: 0,
            last_message_id: None,
            open: None,
            tool_buffers: Vec::new(),
            response_id: None,
            response_model: None,
            created_at: 0,
            pending_finish_reason: None,
            pending_usage: None,
            text_accumulator: String::new(),
            current_text: String::new(),
            reasoning_accumulator: Vec::new(),
            req: None,
            store: None,
        }
    }
}

/// One open Responses output item, tagged with the canonical channel
/// feeding it, carrying the dense `output_index` it was allocated.
#[derive(Debug, Clone)]
enum OpenOutputItem {
    /// An assistant `message` item streaming `output_text` deltas. The
    /// message-level `output_index` and the `content_index` of its
    /// single text part are tracked so deltas and the closing
    /// `output_text.done` / `content_part.done` carry the right indices.
    Text {
        output_index: u64,
        /// Minted item id (`msg_N`), carried on every event for this
        /// item and patched into the completed body.
        message_id: String,
    },
    /// A `reasoning` item streaming summary / text deltas. `detail_id`
    /// groups emitted details (matching the non-stream renderer's
    /// id-grouping); the
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
        translate_request(headers, body, self.store.as_deref())
    }

    fn render_response(&self, resp: ChatResponse) -> Result<bytes::Bytes> {
        self.render_response_with_request(&routectl_core::ChatRequest::default(), resp)
    }

    fn render_response_with_request(
        &self,
        req: &routectl_core::ChatRequest,
        resp: ChatResponse,
    ) -> Result<bytes::Bytes> {
        let wire = render::render_responses_response(req, resp)?;
        // Persist a `store: true` turn so a later
        // `previous_response_id` / `GET /v1/responses/{id}` resolves.
        // The context is the canonical request's own messages (prior
        // turns + this turn's input, no output).
        if req.routectl_internal.responses_store
            && let Some(store) = self.store.as_ref()
            && let Some(id) = wire.get("id").and_then(Value::as_str)
        {
            store.insert(id.to_string(), wire.clone(), req.messages.to_vec());
        }
        crate::ingress::render_value_to_bytes(self.id(), wire)
    }

    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        Box::new(ResponsesStreamState {
            req: Some(ctx.req.clone()),
            store: self.store.clone(),
            ..Default::default()
        })
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
