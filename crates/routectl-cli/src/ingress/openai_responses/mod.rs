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
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Result};
use serde_json::Value;

use super::{ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamErrorClass};

mod parse;
#[cfg(test)]
#[path = "parse_tests.rs"]
mod parse_tests;
mod render;

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
/// `output_index`. The SLICE 3 renderer runs a state machine that maps
/// canonical `ChatChunk`s onto that event sequence, mirroring the egress
/// `sse.rs` reader in reverse and the anthropic ingress
/// `AnthropicStreamState` in spirit.
///
/// SLICE 3 fills these fields. They are declared now so the trait object
/// the handler boxes has a stable concrete type across slices; the
/// streaming slice will refine the exact shape.
#[derive(Debug, Default)]
#[allow(dead_code)] // SLICE 3: the streaming renderer reads/writes these.
pub struct ResponsesStreamState {
    /// Have we emitted the opening `response.created` event yet? Set on
    /// the first chunk.
    started: bool,
    /// True once the terminal `response.completed` event was emitted.
    finished: bool,
    /// Monotonic `sequence_number` counter stamped on every emitted
    /// event (the Responses protocol requires it to increase by one per
    /// event across the whole stream).
    sequence_number: u64,
    /// Next `output_index` to allocate for a new output item.
    next_output_index: u64,
    /// Per-output-index bookkeeping: which canonical channel
    /// (text / reasoning / function_call) currently owns each open
    /// output item, so deltas route to the right `output_index` and a
    /// `response.output_item.done` closes the matching item.
    open_items: Vec<OpenOutputItem>,
    /// Response id echoed on every event (`resp_...`); cached from the
    /// first chunk or synthesized when the upstream omitted one.
    response_id: Option<String>,
    /// Model label echoed on `response.created` / `response.completed`.
    response_model: Option<String>,
}

/// Identity of one open Responses output item, tagged with the canonical
/// channel feeding it. SLICE 3 grows this as the stream state machine
/// needs (e.g. buffered partial tool-call arguments).
#[derive(Debug, Clone)]
#[allow(dead_code)] // SLICE 3: populated by the streaming renderer.
enum OpenOutputItem {
    /// An assistant text item (`output_text` deltas).
    Text { output_index: u64 },
    /// A reasoning item (`reasoning_summary_text` deltas).
    Reasoning { output_index: u64 },
    /// A function-call item (`function_call_arguments` deltas), keyed by
    /// the canonical tool-call index it is buffering.
    FunctionCall {
        output_index: u64,
        tool_call_index: u64,
    },
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
    fn id(&self) -> &str {
        "openai-responses"
    }

    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        // Responses clients (Codex, the OpenAI SDK) parse the flat
        // OpenAI error envelope `{"error":{"message","type","code"}}`.
        ErrorEnvelopeShape::OpenAi
    }

    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
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

    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        Box::new(ResponsesStreamState::default())
    }

    fn render_chunk(
        &self,
        _chunk: ChatChunk,
        _state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        // SLICE 3: the streaming renderer maps a canonical ChatChunk onto
        // Responses SSE events via ResponsesStreamState. Stubbed for now.
        Err(Error::Internal(
            "openai-responses ingress: render_chunk not yet implemented (slice 3)".into(),
        ))
    }

    fn render_eos(&self, _state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        // SLICE 3: emit the terminal `response.completed` event.
        Vec::new()
    }

    fn render_error_eos(
        &self,
        _state: &mut dyn IngressStreamState,
        _error: &dyn std::fmt::Display,
        _class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        // SLICE 3: emit a terminal `response.failed` / error event so
        // SDK consumers see a clean failure rather than truncation.
        Vec::new()
    }
}
