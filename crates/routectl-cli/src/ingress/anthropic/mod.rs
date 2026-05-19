//! Anthropic Messages API ingress (`POST /v1/messages`).
//!
//! Translates Anthropic Messages requests to the canonical `ChatRequest`
//! and back. The canonical type is already designed to absorb Anthropic
//! shape losslessly (typed `ContentPart`, typed `SystemContent`, typed
//! `ToolDef`, top-level `cache_control`, `anthropic_beta`), so the
//! request side is mostly a serde pass-through with a small fix-up
//! step for `thinking`, `metadata`, and `top_k`.
//!
//! Streaming convention: Anthropic emits a sequence of named events
//! (`message_start`, `content_block_start`, `content_block_delta`,
//! `content_block_stop`, `message_delta`, `message_stop`, `ping`).
//! `render_chunk` runs a state machine that tracks the currently
//! open content block and emits the right sequence as canonical
//! chunks arrive. See `AnthropicStreamState`.

use std::any::Any;

use axum::http::HeaderMap;
use serde_json::Value;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Result};

use super::{IngressAdapter, IngressStreamState, SseEvent};

/// The format tag the canonical layer uses for Anthropic-shape
/// reasoning details (from the Anthropic-API egress on the upstream
/// side). The Anthropic ingress renderer no longer filters by format
/// when emitting thinking blocks (every dialect's reasoning_details
/// surface to cc), but this constant is still used in tests that build
/// canonical responses with Anthropic-format details to assert renderer
/// behavior.
#[cfg(test)]
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

#[derive(Debug, Default)]
pub struct AnthropicIngress;

// ---------------------------------------------------------------------------
// Streaming state
// ---------------------------------------------------------------------------

/// Identity of the currently-open Anthropic content block. Tagged
/// with the canonical reasoning-detail index for `Thinking` so two
/// distinct upstream thinking blocks (`detail_index=0` then
/// `detail_index=1`) emit as two separate Anthropic blocks rather
/// than getting merged into one. Without this, multi-block thinking
/// streams reserialize as a single block and break protocol fidelity
/// with downstream consumers that count blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlockKind {
    Text,
    Thinking { detail_index: u32 },
    ToolUse,
}

#[derive(Debug, Default)]
pub struct AnthropicStreamState {
    /// Have we emitted `message_start`? Set on first chunk.
    started: bool,
    /// True once we've emitted `message_stop`.
    finished: bool,
    /// Currently open content block (if any).
    open: Option<(usize, OpenBlockKind)>,
    /// Next block index to allocate.
    next_index: usize,
    /// Stream identifiers cached from first chunk.
    msg_id: Option<String>,
    msg_model: Option<String>,
    /// Buffered tool-use deltas keyed by tool call index. We flush them
    /// sequentially at the end of the stream so OpenAI-style interleaved
    /// tool call chunks still produce valid Anthropic block ordering.
    tool_blocks: Vec<ToolBlockState>,
    /// Finish reason seen on a chunk that arrived WITHOUT usage. Many
    /// openai-compat hosts (OpenAI, OpenRouter, vLLM when
    /// `stream_options.include_usage=true`) emit the finish_reason on
    /// one chunk and a final usage-only chunk after it. The Anthropic
    /// streaming spec emits one `message_delta` carrying BOTH stop_reason
    /// and usage, then `message_stop`. We buffer the fr until the usage
    /// chunk arrives (or `render_eos` runs) so we emit the combined
    /// message_delta + message_stop in the correct order.
    pending_finish_reason: Option<String>,
    /// Matched stop sequence captured on the same chunk as
    /// `pending_finish_reason`. Flushed together via `emit_message_delta`
    /// so the Anthropic wire `stop_sequence` field travels with the
    /// `stop_reason:"stop_sequence"` it correlates with.
    pending_matched_stop_sequence: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct ToolBlockState {
    id: String,
    name: String,
    partial_json: String,
}

impl IngressStreamState for AnthropicStreamState {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body into the canonical
/// `ChatRequest`. Most fields land via direct serde because canonical
/// types accept Anthropic shape; the explicit fix-ups handle:
///
/// - `thinking` -> `reasoning` (canonical `ReasoningConfig`).
/// - `metadata.user_id` -> `user`.
/// - `output_format` (legacy) -> `output_config.format` (current).
/// - `stop_sequences` -> `stop` (canonical name).
/// - `model` is overridden by the `x-routectl-alias` request header
///   when present; otherwise passes through verbatim. v0.6.0 moved
///   alias resolution from per-ingress maps to the router (see
///   `routectl_router::Router::resolve_v6_alias`).
///
/// Any top-level field the canonical doesn't model -- for example
/// Anthropic-only knobs `top_k`, `service_tier`, `output_config`,
/// `container`, `inference_geo`, `context_management`, `context_hint`,
/// `speed`, `diagnostics`, `mcp_servers`, plus anything Anthropic
/// adds in the future -- is swept into `provider_extras` so the
/// egress merges it back into the upstream body verbatim. This keeps
/// routectl forward-compatible: Anthropic ships a new top-level
/// field, claude-code starts emitting it, routectl forwards it
/// without a code change. Without this sweep, serde's
/// silently-drop-unknown behavior would lose the field at the ingress
/// boundary (the original `output_format` bug).
mod parse;
mod render;
mod stream;

use parse::translate_request;
use render::render_messages_response;
use stream::{
    anthropic_state_mut, close_open_block, emit_message_delta, emit_message_stop,
    flush_tool_blocks, render_chunk_internal,
};

/// Reverse of `routectl_providers::anthropic_api::response::map_stop_reason`.
///
/// The egress maps Anthropic stop_reasons -> OpenAI finish_reasons:
/// `end_turn` / `stop_sequence` -> `stop`, `max_tokens` -> `length`,
/// `tool_use` -> `tool_calls`. Anything else (incl. forward-compat
/// values like `pause_turn`, `refusal`,
/// `model_context_window_exceeded`) passes through unchanged.
///
/// The ingress side here reverses the OpenAI overlap and PASSES
/// THROUGH any value not in that set so future-proof Anthropic-only
/// stop reasons don't get clobbered to `end_turn`. This had been
/// silently rewriting `pause_turn`, `refusal`, and
/// `model_context_window_exceeded` to `end_turn`, breaking
/// claude-code's per-stop-reason error handling.
///
/// `stop_sequence` is no longer routed through this mapping: the
/// canonical `Choice.matched_stop_sequence` field carries the matched
/// marker, and both `render.rs::render_messages_response` and
/// `stream.rs::emit_message_delta` override this mapping when the
/// field is present (emitting wire `stop_reason:"stop_sequence"`
/// plus `stop_sequence:"<value>"` directly). The fallback below only
/// fires when `matched_stop_sequence` was not lifted -- openai-compat
/// upstreams where the heuristic didn't recover a match, or
/// non-Anthropic providers without native surfacing. In those cases
/// the canonical `stop` still maps to `end_turn`, which is the best
/// we can do without upstream signal.
///
/// Lives in `mod.rs` (rather than `render.rs` where it was originally
/// defined) because both the non-streaming `render::build_content_array`
/// and the streaming `stream::emit_message_delta` consume it. Hoisting
/// to the parent module avoids a cross-sibling `super::render::...`
/// import in `stream.rs`.
fn openai_finish_to_anthropic_stop(fr: &str) -> &str {
    match fr {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        // Forward-compat: any value the egress passed through verbatim
        // (i.e. an Anthropic stop_reason that doesn't have an OpenAI
        // analogue) must survive the ingress reverse mapping or it
        // gets squashed to "end_turn" and the caller's error handling
        // breaks.
        other => other,
    }
}

impl IngressAdapter for AnthropicIngress {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
        // Trace-level ingress body for triage. Same gating +
        // sensitivity story as the openai ingress. Honors
        // ROUTECTL_LOG_REDACT_PROMPTS=1.
        routectl_core::trace_ingress_body("anthropic", &body);
        // Companion structural summary -- a single TRACE line of
        // stable, prompt-content-free fields the operator's
        // smart-heartbeat validator can grep without fighting the
        // 16 KB body cap. See StructuralSummary on field stability.
        routectl_core::trace_structural_summary("ingress", "ingress", "anthropic", &body);
        translate_request(headers, body)
    }

    fn render_response(&self, resp: ChatResponse) -> Result<Value> {
        Ok(render_messages_response(resp))
    }

    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        Box::new(AnthropicStreamState::default())
    }

    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        render_chunk_internal(chunk, anthropic_state_mut(state))
    }

    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        let s = anthropic_state_mut(state);
        let mut events = Vec::new();
        if !s.finished {
            flush_tool_blocks(s, &mut events);
            close_open_block(s, &mut events);
            // Flush a deferred finish_reason (no usage chunk arrived).
            if let Some(fr) = s.pending_finish_reason.take() {
                let matched = s.pending_matched_stop_sequence.take();
                emit_message_delta(Some(&fr), matched.as_deref(), None, &mut events);
            }
            emit_message_stop(s, &mut events);
        }
        events
    }
}

/// Synthesized message id for `message_start` frames when the
/// upstream chunk omitted one. UUID-based to avoid collisions on hosts
/// with no monotonic clock or under burst load (the prior
/// `SystemTime::now()` nanosecond version could collide on
/// concurrent requests).
fn random_msg_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
