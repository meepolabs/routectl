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
use std::borrow::Cow;
use std::collections::BTreeMap;

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use routectl_core::{
    CacheCreation, ChatChunk, ChatRequest, ChatResponse, ReasoningDetail, Result,
    is_responses_family, reasoning_envelope,
};

use super::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamErrorClass,
    StreamRequestContext,
};

/// The format tag the canonical layer uses for Anthropic-shape
/// reasoning details (from the Anthropic-API egress on the upstream
/// side). The Anthropic ingress renderer no longer filters by format
/// when emitting thinking blocks (every dialect's reasoning_details
/// surface to cc), but this constant is still used in tests that build
/// canonical responses with Anthropic-format details to assert renderer
/// behavior.
#[cfg(test)]
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Anthropic Messages ingress adapter.
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
    /// Opaque-block index mapping: upstream_index -> ingress_index.
    /// Anthropic egresses surface unknown `content_block` types
    /// via `chunk.opaque_events`; the ingress replays those events
    /// value-preserving (semantically lossless for valid JSON -- the
    /// captured `serde_json::Value` is re-serialized, not echoed as
    /// the exact upstream byte slice) but allocates fresh ingress
    /// indexes from `next_index` so canonical and opaque blocks share
    /// a single coherent index sequence on the wire. BTreeMap (not
    /// HashMap) for deterministic iteration order during debug logging.
    opaque_index_map: BTreeMap<u32, usize>,
    /// Resolved model from the originating request. Used as fallback
    /// in `message_start` when upstream chunks carry no model string.
    /// Populated from the `StreamRequestContext` when the state is built
    /// via `IngressAdapter::new_stream_state`; `None` on the `Default`
    /// path (tests, library consumers with no request context).
    pub(super) req_model: Option<String>,
    /// Local input-token estimate for the originating request (see
    /// `ingress::token_estimate`). Emitted as `usage.input_tokens` on the
    /// synthesized `message_start` so the pre-inversion fast path reports
    /// a live context meter instead of zero. The terminal
    /// `message_delta` carries the authoritative upstream count and
    /// overwrites this within seconds. Defaults to 0 on the `Default`
    /// path.
    pub(super) input_tokens_estimate: u64,
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
    anthropic_state_mut, close_lingering_opaque_blocks, close_open_block, emit_message_delta,
    emit_message_start, emit_message_stop, flush_tool_blocks, new_state, render_chunk_internal,
    render_error_eos_internal,
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
        // content_filter has no OpenAI->Anthropic vocabulary match;
        // "refusal" is the closest Anthropic stop_reason. Emitting the
        // raw "content_filter" would be rejected by strict clients.
        "content_filter" => "refusal",
        // Forward-compat: any value the egress passed through verbatim
        // (i.e. an Anthropic stop_reason that doesn't have an OpenAI
        // analogue) must survive the ingress reverse mapping or it
        // gets squashed to "end_turn" and the caller's error handling
        // breaks.
        other => other,
    }
}

/// Insert Anthropic's three cache-accounting fields into a usage map
/// when present. Both the non-streaming `render.rs` and the streaming
/// `stream.rs` usage emitters write the identical block; this helper
/// keeps them in lockstep so a future cache-field add lands in one
/// place. Accepts the three values directly (not the `Usage` /
/// `UsageDelta` struct) so it works for both call sites without a
/// trait or generic. The `input_tokens` / `output_tokens` fields are
/// NOT covered here -- they differ between the two sites (different
/// defaulting) and stay inline at each call site.
pub(super) fn cache_fields_into(
    map: &mut Map<String, Value>,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    cache_creation: Option<&CacheCreation>,
) {
    if let Some(n) = cache_creation_input_tokens {
        map.insert("cache_creation_input_tokens".into(), json!(n));
    }
    if let Some(n) = cache_read_input_tokens {
        map.insert("cache_read_input_tokens".into(), json!(n));
    }
    if let Some(c) = cache_creation {
        let mut cc = Map::new();
        if let Some(n) = c.ephemeral_5m_input_tokens {
            cc.insert("ephemeral_5m_input_tokens".into(), json!(n));
        }
        if let Some(n) = c.ephemeral_1h_input_tokens {
            cc.insert("ephemeral_1h_input_tokens".into(), json!(n));
        }
        map.insert("cache_creation".into(), Value::Object(cc));
    }
}

/// The `redacted_thinking.data` bytes to emit for an `Encrypted`
/// reasoning detail, self-describing when the artifact's scheme would
/// otherwise be lost.
///
/// The Anthropic wire has no slot for a reasoning artifact's item id or
/// its scheme, so a Responses-family artifact flattened here loses both
/// and comes back next turn as a bare blob indistinguishable from a
/// native `redacted_thinking` -- unreplayable on the lane that issued
/// it. Wrapping restores the round trip with no server-side state.
///
/// The carve-out is load-bearing: anything NOT in the Responses family
/// -- an Anthropic-sourced artifact above all -- is emitted BYTE-VERBATIM.
/// An Anthropic signature is what makes same-model replay work on that
/// lane, and it is platform-portable same-model and silently ignored
/// cross-model, so it is never rejected; wrapping it would corrupt a
/// mechanism that works today. Family membership comes from the shared
/// `is_responses_family` classifier, never from a second local notion of
/// what is Anthropic.
///
/// An artifact with no recoverable id still wraps, id-less, so its
/// scheme survives: one lane family validates content and ignores the id
/// entirely, making a scheme-only envelope fully replayable there.
///
/// The field name differs by dialect -- Anthropic and the OpenRouter
/// passthrough use `data`, OpenAI Responses uses `encrypted_content` --
/// so both aliases are read.
fn encrypted_detail_data(d: &ReasoningDetail) -> Cow<'_, str> {
    let blob = d
        .payload
        .get("data")
        .and_then(|v| v.as_str())
        .or_else(|| d.payload.get("encrypted_content").and_then(|v| v.as_str()))
        .unwrap_or_default();

    let format = d.format.as_deref();
    // An empty blob carries nothing to replay, so an envelope around it
    // would only be rejected on the way back.
    if blob.is_empty() || !is_responses_family(format) {
        return Cow::Borrowed(blob);
    }
    let scheme_tag = format.unwrap_or_default();
    Cow::Owned(reasoning_envelope::wrap(scheme_tag, d.id.as_deref(), blob))
}

impl IngressAdapter for AnthropicIngress {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        ErrorEnvelopeShape::Anthropic
    }

    fn parse_request(&self, headers: &HeaderMap, body: &[u8]) -> Result<ChatRequest> {
        // Materialize the wire body once from the raw request bytes. A
        // top-level syntax error surfaces as `Error::Json` (the handler
        // renders it as a 400 malformed body).
        let body: Value = serde_json::from_slice(body)?;
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

    fn render_response(&self, resp: ChatResponse) -> Result<bytes::Bytes> {
        crate::ingress::render_value_to_bytes(self.id(), render_messages_response(resp))
    }

    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        Box::new(new_state(ctx))
    }

    fn early_frame(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        // Warm-hold first body byte: flush the synthesized message_start
        // (carrying the local input-token estimate seeded on the state) and
        // mark the state started so the real first-content chunk dedups it
        // via the `!state.started` guard in `render_chunk_internal`.
        let s = anthropic_state_mut(state);
        let mut events = Vec::new();
        if !s.started {
            emit_message_start(s, &mut events);
            s.started = true;
        }
        events
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
            // An upstream stream that produced zero chunks still needs a
            // protocol-valid frame sequence: emit a synthetic
            // `message_start` before the terminal `message_stop` so
            // SDK consumers don't see a bare `message_stop` (which the
            // spec forbids).
            if !s.started {
                emit_message_start(s, &mut events);
                s.started = true;
            }
            flush_tool_blocks(s, &mut events);
            close_open_block(s, &mut events);
            close_lingering_opaque_blocks(s, &mut events);
            // Flush a deferred finish_reason (no usage chunk arrived).
            if let Some(fr) = s.pending_finish_reason.take() {
                let matched = s.pending_matched_stop_sequence.take();
                emit_message_delta(Some(&fr), matched.as_deref(), None, &mut events);
            }
            emit_message_stop(s, &mut events);
        }
        events
    }

    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        render_error_eos_internal(anthropic_state_mut(state), error, class)
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
