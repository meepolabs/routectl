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
//! - `ToolUse`  -- function_call item; argument deltas stream
//!   through, or, when no delta carried them, the final arguments from
//!   `function_call_arguments.done` / `output_item.done` are emitted once.
//!   On the codex lane a call to a tool whose empty optional arguments the
//!   `empty_optional` filter can remove is held back instead: its arguments
//!   are emitted once, filtered, when a done event (or completion) settles
//!   them
//! - `Passthrough` -- any other item type (e.g. `custom_tool_call`); the
//!   whole item rides one `OpaqueSseEvent::OutputItem` chunk, emitted at
//!   `output_item.done` (or flushed at completion when no done event came),
//!   and only for a client whose dialect can carry it
//!
//! Indices (`call_index`, `detail_index`) are assigned dense (0, 1, 2,
//! ...) per stream via `next_call_index` / `next_detail_index`
//! counters on `ResponsesStreamState` so OpenAI SSE clients see stable
//! `tool_calls[].index` / `reasoning_details[].index`.
//!
//! Reference: `codex-rs/codex-api/src/sse/responses.rs:297-431` --
//! the event dispatch surface in codex.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use routectl_core::{
    ChatChunk, Error, OpaqueSseEvent, ReasoningDetail, ReasoningDetailKind, RequestObservation,
    RequestProvenance, ResponsesEnvelope, ResponsesItemMembers, Result, Role, ToolDef,
    UpstreamMeta, UsageInputSource,
    schema::{ChunkChoice, ChunkDelta, UsageDelta},
    wire_check::{IngressDialect, WireTap},
};

/// Discriminant returned by `BlockState::tag` for log-context strings.
const BLOCK_TAG_TEXT: &str = "text";
const BLOCK_TAG_REASONING: &str = "reasoning";
const BLOCK_TAG_TOOL_USE: &str = "tool_use";
const BLOCK_TAG_PASSTHROUGH: &str = "passthrough";

/// Per-stream cap on the number of distinct `output_index` entries
/// the openai-responses SSE state machine's block map will hold.
/// Legitimate Responses streams emit a small handful of output items
/// per turn; an adversarial upstream could stream thousands of distinct
/// indices to drive the map toward OOM. 4096 is well above any
/// legitimate breadth.
pub(super) const MAX_OUTPUT_BLOCKS: usize = 4096;

use super::empty_optional::EmptyOptionalFilter;
use super::response::{cache_details, map_finish_reason, upstream_error_from_failed};
use super::response_types::{
    OUTPUT_FUNCTION_CALL_ITEM_KEYS, OUTPUT_MESSAGE_ITEM_KEYS, ResponsesResponse,
    ResponsesStreamEvent, item_members, reasoning_item_members,
};
use super::{AuthKind, lane_format_tag};

/// The item event an output item's members were read from.
#[derive(Debug, Clone, Copy)]
enum ItemEvent {
    Added,
    Done,
}

/// Per-output-item streaming state.
#[derive(Debug, Clone)]
enum BlockState {
    /// Assistant message item streaming text deltas.
    Text { item_id: String },
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
    /// Function-call item. `arguments` holds every argument byte
    /// already emitted downstream. Empty means nothing has been sent
    /// yet, so the final arguments carried by
    /// `function_call_arguments.done` (or, failing that, by the item on
    /// `output_item.done`) are emitted in one chunk. `finalized` is
    /// set once a done event has settled the arguments; any delta
    /// arriving after that would append to an already-complete JSON
    /// value, so it is dropped. `zero_arg_deferred` records an empty
    /// `function_call_arguments.done` left for `output_item.done` to
    /// settle; a stream that completes without that event settles it as
    /// `{}` at completion. `buffered` holds the deltas back: `arguments`
    /// then accumulates bytes not yet sent, emitted filtered and whole once
    /// a done event or the completion settles them.
    ToolUse {
        #[allow(dead_code)]
        item_id: String,
        call_id: String,
        name: String,
        call_index: u32,
        arguments: String,
        finalized: bool,
        zero_arg_deferred: bool,
        buffered: bool,
    },
    /// An output item of a type this parser does not model, held as the
    /// item.added payload until its done event (or completion) settles it.
    Passthrough { item: Value },
}

impl BlockState {
    /// Short tag for log context. Cheap (no allocation) and stable
    /// across the lifetime of the state machine.
    const fn tag(&self) -> &'static str {
        match self {
            Self::Text { .. } => BLOCK_TAG_TEXT,
            Self::Reasoning { .. } => BLOCK_TAG_REASONING,
            Self::ToolUse { .. } => BLOCK_TAG_TOOL_USE,
            Self::Passthrough { .. } => BLOCK_TAG_PASSTHROUGH,
        }
    }
}

/// Persistent state across all SSE events for one streaming response.
#[derive(Debug, Default)]
pub struct ResponsesStreamState {
    /// Set from `response.created.response.id` (or the first event
    /// that carries an id). Threaded onto every emitted chunk so
    /// OpenAI SSE clients can correlate.
    pub(crate) response_id: String,
    /// Set from `response.created.response.model`.
    pub(crate) model: String,
    /// Per-output-item state. Keyed on `output_index`.
    blocks: HashMap<u32, BlockState>,
    /// `output_index` of the message item the latest text chunk belonged
    /// to. A text chunk for a different item carries that item's id, so
    /// interleaved message items never borrow each other's id.
    last_text_index: Option<u32>,
    /// Dense counter for OpenAI-shape `tool_calls[].index`.
    next_call_index: u32,
    /// Dense counter for `reasoning_details[].index`.
    next_detail_index: u32,
    /// True once `response.created` has been processed; used to emit
    /// the opening `delta.role="assistant"` chunk exactly once per
    /// stream, before any content delta.
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
    /// Lane that minted this stream. Selects the reasoning format tag
    /// stamped on every emitted detail, so the streaming path and the
    /// non-streaming path agree tag-for-tag on the same lane.
    auth_kind: AuthKind,
    /// Ingress dialect of the client this stream answers. Decides whether
    /// a passthrough item is emitted or dropped.
    provenance: RequestProvenance,
    /// The client request's observation slot, installed only around the
    /// synchronous drop count so the loss reaches that request's usage row.
    observation: Option<RequestObservation>,
    /// Set once this stream counted an output item it could not deliver,
    /// so the class counts once per stream even with no observation slot.
    unrepresentable_counted: bool,
    /// Set once an item.added past the block cap was WARNed; later ones
    /// log at DEBUG so an over-producing upstream cannot flood the log.
    cap_warned: bool,
    /// The request's empty-optional lookup, present only on the codex lane.
    /// A call to a tool it can filter is buffered.
    empty_optional: Option<EmptyOptionalFilter>,
    /// The request's wire tap, for declaring a removal on a compared
    /// response leg.
    wire_tap: Option<WireTap>,
}

impl ResponsesStreamState {
    /// The stream state for one request: `tools` are the tools it declared
    /// and `wire_tap` its tap, read only on the codex lane.
    #[must_use]
    pub(crate) fn new(
        auth_kind: AuthKind,
        provenance: RequestProvenance,
        observation: Option<RequestObservation>,
        tools: &[ToolDef],
        wire_tap: Option<WireTap>,
    ) -> Self {
        let empty_optional = auth_kind
            .is_codex_lane()
            .then(|| EmptyOptionalFilter::from_tools(tools));
        Self {
            auth_kind,
            provenance,
            observation,
            empty_optional,
            wire_tap,
            ..Self::default()
        }
    }

    /// Process one SSE event. Returns:
    ///   - `Ok(chunks)`: zero-or-more chunks to forward (empty for
    ///     housekeeping events like in_progress/output_text.done)
    ///   - `Err(_)`: a fatal stream error -- `response.failed` or a
    ///     malformed event payload. The caller terminates the stream.
    pub(crate) fn parse_event(
        &mut self,
        provider_id: &str,
        event: ResponsesStreamEvent,
    ) -> Result<Vec<ChatChunk>> {
        let kind = event.kind.clone();
        let carried = self.event_carriage(&kind, &event);
        let result = self.dispatch_event(provider_id, &kind, event);
        if is_terminal_event(&kind) {
            self.report_removed_empty_optionals(provider_id);
        }
        result.map(|chunks| self.attach_carriage(chunks, carried))
    }

    /// Count, log and declare the empty optional arguments this stream
    /// removed, at most once: called when the stream ends, whichever way.
    pub(crate) fn report_removed_empty_optionals(&mut self, provider_id: &str) {
        let Some(filter) = self.empty_optional.as_mut() else {
            return;
        };
        let removed = filter.take_removed();
        super::response::report_empty_optionals_removed(
            provider_id,
            self.observation.as_ref(),
            self.wire_tap.as_ref(),
            &removed,
        );
    }

    fn dispatch_event(
        &mut self,
        provider_id: &str,
        kind: &str,
        event: ResponsesStreamEvent,
    ) -> Result<Vec<ChatChunk>> {
        match kind {
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
            "response.function_call_arguments.delta" => {
                Ok(self.handle_function_call_delta(provider_id, &event))
            }
            "response.function_call_arguments.done" => {
                Ok(self.handle_function_call_args_done(provider_id, &event))
            }
            // The item's own done event carries the whole input; the
            // incremental preview is not forwarded.
            "response.custom_tool_call_input.delta" => Ok(Vec::new()),
            "response.output_item.done" => Ok(self.handle_item_done(provider_id, &event)),
            "response.completed" => self.handle_completed(provider_id, &event),
            "response.incomplete" => self.handle_incomplete(provider_id, &event),
            "response.failed" => Err(self.handle_failed(provider_id, &event)),
            "response.cancelled" => Err(self.handle_cancelled(provider_id, &event)),
            other => {
                // Forward compat: a new event kind ships without a
                // rebuild. DEBUG (not WARN) because OpenAI adds new
                // event kinds frequently and WARN would flood the log.
                // A Responses client gets an unknown event itself, through
                // the carriage `parse_event` attaches.
                if !(self.carries_unmodeled() && carriage::is_carried_whole(other)) {
                    tracing::debug!(
                        provider = provider_id,
                        event_type = %routectl_core::upstream_text_for_log(other),
                        "openai-responses: skipping unknown stream event"
                    );
                }
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
        let created_at = event
            .response
            .as_ref()
            .and_then(|resp| resp.get("created_at"))
            .and_then(Value::as_i64);
        self.created_emitted = true;
        // Opening role chunk: an OpenAI-Chat stream opens with a single
        // `delta.role="assistant"` chunk before any content delta. Emitted
        // once per stream (guarded by `created_emitted`), matching every
        // peer egress lane.
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
            opaque_events: Vec::new(),
            upstream_meta: created_at.map(|created_at| {
                UpstreamMeta::from_responses_envelope(ResponsesEnvelope::of_created_at(created_at))
                    .with_wire_dialect(IngressDialect::Responses)
            }),
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
        // The stream remains usable for items below the cap.
        if !self.blocks.contains_key(&idx) && self.blocks.len() >= MAX_OUTPUT_BLOCKS {
            self.skip_item_beyond_cap(provider_id, idx, item_type);
            return Vec::new();
        }

        let carrier = self.item_members_chunk(item, ItemEvent::Added);
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
                    .map(std::string::ToString::to_string);
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
                let buffered = self
                    .empty_optional
                    .as_ref()
                    .is_some_and(|filter| filter.filters(&name));
                self.blocks.insert(
                    idx,
                    BlockState::ToolUse {
                        item_id,
                        call_id,
                        name,
                        call_index,
                        arguments: String::new(),
                        finalized: false,
                        zero_arg_deferred: false,
                        buffered,
                    },
                );
                // The sticky `saw_function_call` flag was already set
                // above (pre-cap) so this branch does not duplicate it.
            }
            _ => {
                self.blocks
                    .insert(idx, BlockState::Passthrough { item: item.clone() });
            }
        }
        carrier.into_iter().collect()
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
        let switches_item = self.last_text_index != Some(output_index);
        let opening_item_id = match self.blocks.get(&output_index) {
            Some(BlockState::Text { item_id }) => {
                (switches_item && !item_id.is_empty()).then(|| item_id.clone())
            }
            Some(state) => {
                tracing::debug!(
                    provider = provider_id,
                    output_index,
                    block = state.tag(),
                    "openai-responses: output_text.delta for non-Text block; ignoring"
                );
                return Vec::new();
            }
            None => {
                tracing::debug!(
                    provider = provider_id,
                    output_index,
                    "openai-responses: output_text.delta for unknown block; ignoring"
                );
                return Vec::new();
            }
        };
        self.last_text_index = Some(output_index);
        let mut chunk = self.text_chunk(delta.to_string());
        chunk.upstream_meta = opening_item_id.map(|id| {
            UpstreamMeta::from_responses_envelope(ResponsesEnvelope::of_message_item_id(id))
                .with_wire_dialect(IngressDialect::Responses)
        });
        vec![chunk]
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

    fn handle_function_call_delta(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Vec<ChatChunk> {
        let Some(delta) = event.delta.as_deref() else {
            return Vec::new();
        };
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        let (call_id, name, call_index) = match self.blocks.get_mut(&idx) {
            Some(BlockState::ToolUse {
                finalized: true, ..
            }) => {
                tracing::debug!(
                    provider = provider_id,
                    output_index = idx,
                    "openai-responses: function_call argument delta after done event; dropping"
                );
                return Vec::new();
            }
            Some(BlockState::ToolUse {
                call_id,
                name,
                call_index,
                arguments,
                buffered,
                ..
            }) => {
                arguments.push_str(delta);
                if *buffered {
                    return Vec::new();
                }
                (call_id.clone(), name.clone(), *call_index)
            }
            _ => return Vec::new(),
        };
        vec![self.tool_delta_chunk(call_id, name, call_index, delta.to_string())]
    }

    fn handle_function_call_args_done(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Vec<ChatChunk> {
        let (Some(idx), Some(final_args)) = (event.output_index, event.arguments.as_deref()) else {
            return Vec::new();
        };
        // An empty payload with nothing streamed yet settles nothing:
        // output_item.done may still carry the real arguments, and it
        // commits `{}` itself when it does not.
        if final_args.is_empty()
            && let Some(BlockState::ToolUse {
                arguments,
                zero_arg_deferred,
                ..
            }) = self.blocks.get_mut(&idx)
            && arguments.is_empty()
        {
            *zero_arg_deferred = true;
            return Vec::new();
        }
        self.finalize_tool_arguments(provider_id, idx, final_args)
            .into_iter()
            .collect()
    }

    /// Emit a ToolUse block's arguments from a done-event payload.
    /// Some upstreams send no `function_call_arguments.delta` at all
    /// and carry the arguments only on the done events; without this
    /// the client sees `finish_reason=tool_calls` with no tool call.
    /// When deltas already streamed bytes those are kept as-is, so the
    /// arguments are never emitted twice.
    fn finalize_tool_arguments(
        &mut self,
        provider_id: &str,
        idx: u32,
        final_args: &str,
    ) -> Option<ChatChunk> {
        let Some(BlockState::ToolUse {
            call_id,
            name,
            call_index,
            arguments,
            finalized,
            buffered,
            ..
        }) = self.blocks.get_mut(&idx)
        else {
            return None;
        };
        let was_finalized = std::mem::replace(finalized, true);
        let final_args = super::response::zero_arg_as_empty_object(final_args);
        if *buffered {
            if was_finalized {
                return None;
            }
            if arguments.is_empty() {
                arguments.push_str(final_args);
            }
            let held = (
                call_id.clone(),
                name.clone(),
                *call_index,
                arguments.clone(),
            );
            return Some(self.emit_buffered_arguments(held));
        }
        if !arguments.is_empty() {
            if arguments.as_str() != final_args {
                tracing::debug!(
                    provider = provider_id,
                    output_index = idx,
                    streamed_len = arguments.len(),
                    final_len = final_args.len(),
                    "openai-responses: final function_call arguments differ from streamed deltas; keeping streamed"
                );
            }
            return None;
        }
        arguments.push_str(final_args);
        let (call_id, name, call_index) = (call_id.clone(), name.clone(), *call_index);
        Some(self.tool_delta_chunk(call_id, name, call_index, final_args.to_string()))
    }

    /// The one chunk carrying a buffered call's arguments, without the empty
    /// optional arguments the filter removes. Held deltas win over a done
    /// payload, as they do for a call whose deltas streamed.
    fn emit_buffered_arguments(
        &mut self,
        (call_id, name, call_index, held): (String, String, u32, String),
    ) -> ChatChunk {
        let arguments = match self.empty_optional.as_mut() {
            Some(filter) => filter.apply(&name, &held).into_owned(),
            None => held,
        };
        self.tool_delta_chunk(call_id, name, call_index, arguments)
    }

    fn handle_item_done(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Vec<ChatChunk> {
        let Some(idx) = event.output_index else {
            return Vec::new();
        };
        if matches!(self.blocks.get(&idx), Some(BlockState::Passthrough { .. })) {
            return self.settle_passthrough(provider_id, idx, event.item.as_ref());
        }
        let mut chunks = Vec::new();
        if matches!(self.blocks.get(&idx), Some(BlockState::ToolUse { .. })) {
            let final_args = match event.item.as_ref().and_then(|v| v.get("arguments")) {
                Some(Value::String(s)) => s.as_str(),
                Some(other) => {
                    tracing::debug!(
                        provider = provider_id,
                        output_index = idx,
                        arguments_type = json_type_name(other),
                        "openai-responses: function_call item.done arguments are not a string"
                    );
                    ""
                }
                None => {
                    tracing::debug!(
                        provider = provider_id,
                        output_index = idx,
                        "openai-responses: function_call item.done without arguments"
                    );
                    ""
                }
            };
            chunks.extend(self.finalize_tool_arguments(provider_id, idx, final_args));
        }
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
            .map(std::string::ToString::to_string);

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
        if matches!(
            self.blocks.get(&idx),
            Some(
                BlockState::Text { .. } | BlockState::ToolUse { .. } | BlockState::Reasoning { .. }
            )
        ) && let Some(item) = event.item.as_ref()
        {
            chunks.extend(self.item_members_chunk(item, ItemEvent::Done));
        }
        self.blocks.remove(&idx);
        chunks
    }

    /// The chunk carrying the unmodeled members a `message`,
    /// `function_call` or `reasoning` item has on this item event, keyed by
    /// the identity the Responses render emits the item under. `None` for
    /// any other item, an item with no such member, or a client of another
    /// dialect, which has nowhere to put them.
    fn item_members_chunk(&self, item: &Value, event: ItemEvent) -> Option<ChatChunk> {
        if !super::client_carries_unmodeled_items(self.provenance) {
            return None;
        }
        let identity = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut by_item = ResponsesItemMembers::default();
        let (by_identity, identity, members) = match item_type_of(item) {
            "message" => (
                &mut by_item.message,
                identity("id"),
                item_members(item, OUTPUT_MESSAGE_ITEM_KEYS),
            ),
            "function_call" => (
                &mut by_item.function_call,
                identity("call_id"),
                item_members(item, OUTPUT_FUNCTION_CALL_ITEM_KEYS),
            ),
            "reasoning" => (
                &mut by_item.reasoning,
                identity("id"),
                reasoning_item_members(item),
            ),
            _ => return None,
        };
        if members.is_empty() {
            return None;
        }
        by_identity.insert(identity, members);
        let mut envelope = ResponsesEnvelope::default();
        match event {
            ItemEvent::Added => envelope.added_item_members = by_item,
            ItemEvent::Done => envelope.item_members = by_item,
        }
        Some(
            self.carrier_chunk(
                UpstreamMeta::from_responses_envelope(envelope)
                    .with_wire_dialect(IngressDialect::Responses),
            ),
        )
    }

    fn handle_completed(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Result<Vec<ChatChunk>> {
        // Guard: a `response.completed` event with no `response` field
        // is a malformed payload. Treat it as an upstream error so
        // callers see an explicit failure rather than a silent empty-
        // output completion. `unwrap_or_default_resp` used to paper over
        // this; the explicit check makes the contract observable.
        let resp_value = match event.response.clone() {
            Some(v) => v,
            None => {
                return Err(Error::upstream(
                    provider_id,
                    0,
                    "openai-responses: response.completed without response payload".to_string(),
                ));
            }
        };
        let unsettled_items = self.flush_unsettled_passthrough(provider_id, &resp_value);
        let mut resp = ResponsesResponse::deserialize(&resp_value).unwrap_or_default_resp();
        let response_members = std::mem::take(&mut resp.extras);

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
            let cache = cache_details(u.input_tokens_details.as_ref());
            let reasoning_tokens = u
                .output_tokens_details
                .as_ref()
                .and_then(|v| v.get("reasoning_tokens"))
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as u32);
            UsageDelta {
                prompt_tokens: Some(u.input_tokens),
                completion_tokens: Some(u.output_tokens),
                total_tokens: Some(u.total_tokens),
                reasoning_tokens,
                cache_creation_input_tokens: cache.write_total,
                cache_read_input_tokens: cache.read,
                cache_creation: cache.write_split,
                extras: u.carried_members(),
                ..Default::default()
            }
        });

        if !resp.id.is_empty() {
            self.response_id = resp.id;
        }
        if !resp.model.is_empty() {
            self.model = resp.model;
        }

        let mut chunks = self.flush_unsettled_calls(provider_id, &resp_value);
        chunks.extend(unsettled_items);
        chunks.push(ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason,
                matched_stop_sequence: None,
            }],
            upstream_meta: self.completed_meta(usage_delta.is_some(), response_members),
            usage: usage_delta,
            opaque_events: Vec::new(),
        });
        Ok(chunks)
    }

    /// The terminal chunk's carrier: the usage provenance when it carries
    /// usage, and the completed body's unmodeled top-level members for a
    /// client of this dialect.
    fn completed_meta(
        &self,
        has_usage: bool,
        response_members: serde_json::Map<String, Value>,
    ) -> Option<UpstreamMeta> {
        let usage_meta = has_usage
            .then(|| UpstreamMeta::from_usage_input_source(UsageInputSource::ExplicitFinal));
        let members_meta = (self.carries_unmodeled() && !response_members.is_empty()).then(|| {
            let mut envelope = ResponsesEnvelope::default();
            envelope.extras = response_members;
            UpstreamMeta::from_responses_envelope(envelope)
        });
        let meta = match (usage_meta, members_meta) {
            (Some(usage), Some(members)) => usage.merge(members),
            (usage, members) => usage.or(members)?,
        };
        Some(meta.with_wire_dialect(IngressDialect::Responses))
    }

    /// Settle, in output order, every call no done event settled. The
    /// terminal body's copy of the call at the same `output_index` supplies
    /// the arguments; held deltas still win over it, as they do over a done
    /// payload. Without a body copy, a call whose empty
    /// `function_call_arguments.done` deferred to an `output_item.done` that
    /// never arrived settles as `{}` and a buffered call settles its held
    /// arguments; any other call has nothing to send.
    fn flush_unsettled_calls(&mut self, provider_id: &str, body: &Value) -> Vec<ChatChunk> {
        let body_output = body.get("output").and_then(Value::as_array);
        let mut unsettled: Vec<(u32, Option<String>)> = self
            .blocks
            .iter()
            .filter_map(|(idx, block)| match block {
                BlockState::ToolUse {
                    finalized: false,
                    zero_arg_deferred,
                    buffered,
                    arguments,
                    call_id,
                    ..
                } => {
                    let body_args = body_output
                        .and_then(|items| items.get(*idx as usize))
                        .and_then(|item| body_call_arguments(item, call_id));
                    let owed = body_args.is_some()
                        || *zero_arg_deferred
                        || (*buffered && !arguments.is_empty());
                    owed.then_some((*idx, body_args))
                }
                _ => None,
            })
            .collect();
        unsettled.sort_unstable_by_key(|(idx, _)| *idx);
        unsettled
            .into_iter()
            .filter_map(|(idx, body_args)| {
                let final_args = body_args.as_deref().unwrap_or("");
                self.finalize_tool_arguments(provider_id, idx, final_args)
            })
            .collect()
    }

    /// Settle the passthrough block at `idx`: emit the finalized item when
    /// the client's dialect can carry it, otherwise count the loss. The block
    /// leaves the map here, so a later completion never settles it twice.
    fn settle_passthrough(
        &mut self,
        provider_id: &str,
        idx: u32,
        final_item: Option<&Value>,
    ) -> Vec<ChatChunk> {
        let Some(BlockState::Passthrough { item: added }) = self.blocks.remove(&idx) else {
            return Vec::new();
        };
        let item = final_item
            .filter(|v| same_item_type(v, &added))
            .cloned()
            .unwrap_or(added);
        if !super::client_carries_unmodeled_items(self.provenance) {
            self.drop_unrepresentable_item(provider_id, idx, item_type_of(&item));
            return Vec::new();
        }
        vec![self.output_item_chunk(item)]
    }

    /// Settle, in output order, every passthrough block that got no
    /// `output_item.done`, preferring the completed body's copy of the item
    /// at the same `output_index` over the item.added payload.
    fn flush_unsettled_passthrough(&mut self, provider_id: &str, body: &Value) -> Vec<ChatChunk> {
        let mut unsettled: Vec<u32> = self
            .blocks
            .iter()
            .filter_map(|(idx, block)| {
                matches!(block, BlockState::Passthrough { .. }).then_some(*idx)
            })
            .collect();
        unsettled.sort_unstable();
        let body_output = body.get("output").and_then(Value::as_array);
        unsettled
            .into_iter()
            .flat_map(|idx| {
                let body_item = body_output.and_then(|items| items.get(idx as usize));
                self.settle_passthrough(provider_id, idx, body_item)
            })
            .collect()
    }

    /// An item.added past [`MAX_OUTPUT_BLOCKS`] is never tracked, so the
    /// client never receives it, whatever its dialect.
    fn skip_item_beyond_cap(&mut self, provider_id: &str, idx: u32, item_type: &str) {
        if std::mem::replace(&mut self.cap_warned, true) {
            tracing::debug!(
                provider = provider_id,
                output_index = idx,
                item_type = %routectl_core::upstream_text_for_log(item_type),
                "openai-responses: output_item.added beyond cap; skipping"
            );
        } else {
            tracing::warn!(
                provider = provider_id,
                output_index = idx,
                cap = MAX_OUTPUT_BLOCKS,
                item_type = %routectl_core::upstream_text_for_log(item_type),
                "openai-responses: output_item.added beyond cap; skipping"
            );
        }
        if std::mem::replace(&mut self.unrepresentable_counted, true) {
            return;
        }
        let _observation = crate::translation_drop_metrics::ObservationScope::enter_slot(
            self.observation.as_ref(),
        );
        // TRANSLATION-DROP: lane=openai-responses class=output_item_unrepresentable test=an_item_past_the_block_cap_is_counted_and_warned
        crate::translation_drop_metrics::record_translation_drop(
            super::LANE,
            "output_item_unrepresentable",
        );
    }

    /// A finalized unmodeled item for a client whose dialect has no shape
    /// for it. The loss is census-only: this reaches only a translation lane,
    /// whose tap never compares the response leg.
    fn drop_unrepresentable_item(&mut self, provider_id: &str, idx: u32, item_type: &str) {
        tracing::warn!(
            provider = provider_id,
            output_index = idx,
            item_type = %routectl_core::upstream_text_for_log(item_type),
            "openai-responses: dropping unmodeled output item the client dialect cannot carry"
        );
        if std::mem::replace(&mut self.unrepresentable_counted, true) {
            return;
        }
        let _observation = crate::translation_drop_metrics::ObservationScope::enter_slot(
            self.observation.as_ref(),
        );
        // TRANSLATION-DROP: lane=openai-responses class=output_item_unrepresentable test=a_non_responses_client_loses_the_streamed_item_and_counts_it_once
        crate::translation_drop_metrics::record_translation_drop(
            super::LANE,
            "output_item_unrepresentable",
        );
    }

    fn handle_incomplete(
        &mut self,
        provider_id: &str,
        event: &ResponsesStreamEvent,
    ) -> Result<Vec<ChatChunk>> {
        // Same code path as `completed` -- the body carries
        // status="incomplete" and the translator does the right thing.
        self.handle_completed(provider_id, event)
    }

    fn handle_failed(&mut self, provider_id: &str, event: &ResponsesStreamEvent) -> Error {
        // Try the structured ResponseFailed envelope first. The structured
        // path lifts a codex usage-limit reset hint via
        // `upstream_error_from_failed`; this bare fallback has no
        // structured error object, so retry_after stays None.
        if let Some(resp_value) = event.response.clone()
            && let Ok(resp) = serde_json::from_value::<ResponsesResponse>(resp_value)
        {
            return upstream_error_from_failed(provider_id, &resp);
        }
        Error::upstream(
            provider_id,
            0,
            "openai-responses: response.failed".to_string(),
        )
    }

    fn handle_cancelled(&mut self, provider_id: &str, event: &ResponsesStreamEvent) -> Error {
        // Cancelled mirrors failed: surface as Err::upstream so callers
        // can distinguish cancellation from clean completion and retry.
        // Previously this delegated to handle_completed and emitted a
        // normal terminal chunk with finish_reason="error" -- that was
        // inconsistent with how complete() treats cancellation (Err),
        // and hid the event from callers who gate retries on Err.
        if let Some(resp_value) = event.response.clone()
            && let Ok(resp) = serde_json::from_value::<ResponsesResponse>(resp_value)
        {
            return upstream_error_from_failed(provider_id, &resp);
        }
        Error::upstream(
            provider_id,
            0,
            "openai-responses: response.cancelled".to_string(),
        )
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
            opaque_events: Vec::new(),
            upstream_meta: None,
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
            format: Some(lane_format_tag(self.auth_kind).to_string()),
            index: Some(detail_index),
            payload: json!({"text": text}),
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
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }

    fn reasoning_text_chunk(&self, detail_id: &str, detail_index: u32, text: String) -> ChatChunk {
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some(stable_or_minted_id(detail_id)),
            format: Some(lane_format_tag(self.auth_kind).to_string()),
            index: Some(detail_index),
            payload: json!({"text": text}),
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
            opaque_events: Vec::new(),
            upstream_meta: None,
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
            format: Some(lane_format_tag(self.auth_kind).to_string()),
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
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }

    /// A chunk with an empty delta whose only payload is `meta`.
    fn carrier_chunk(&self, meta: UpstreamMeta) -> ChatChunk {
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: Some(meta),
        }
    }

    fn output_item_chunk(&self, item: Value) -> ChatChunk {
        ChatChunk {
            id: self.response_id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: vec![OpaqueSseEvent::OutputItem { item }],
            upstream_meta: None,
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
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// JSON type name of `v` for log context; never the value itself.
const fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The arguments of the terminal body's `item` when it is the held call
/// `call_id`; arguments that are not a string settle as empty, as on
/// `output_item.done`.
fn body_call_arguments(item: &Value, call_id: &str) -> Option<String> {
    let is_held_call = item_type_of(item) == "function_call"
        && item.get("call_id").and_then(Value::as_str) == Some(call_id);
    is_held_call.then(|| {
        item.get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })
}

fn item_type_of(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Whether `candidate` is an item of the same type as `held`, so a done event
/// or completed body copy can replace the held item.added payload.
fn same_item_type(candidate: &Value, held: &Value) -> bool {
    candidate.is_object() && item_type_of(candidate) == item_type_of(held)
}

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

/// Terminal predicate for the Responses wire: every turn closes with exactly
/// one of these. `incomplete` is success-with-cutoff; `failed` and
/// `cancelled` are explicit upstream failures -- all four end the stream.
pub fn is_terminal_event(kind: &str) -> bool {
    matches!(
        kind,
        "response.completed" | "response.incomplete" | "response.failed" | "response.cancelled"
    )
}

/// Parse one raw `data:` SSE payload into a typed event. A parse error
/// returns `Err(Error::Streaming)` so the stream terminates -- a
/// malformed event on the Responses surface is not a recoverable
/// condition (codex itself returns a Stream error in this case at
/// `codex-rs/codex-api/src/sse/responses.rs:473-479`).
pub fn parse_data_line(provider_id: &str, data: &str) -> Result<ResponsesStreamEvent> {
    let mut event: ResponsesStreamEvent = serde_json::from_str(data).map_err(|e| {
        Error::Streaming(format!(
            "openai-responses provider `{provider_id}`: bad SSE json: {e}"
        ))
    })?;
    event.raw = Some(data.into());
    Ok(event)
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
            extras: serde_json::Map::new(),
        })
    }
}

#[path = "sse_carriage.rs"]
mod carriage;

pub use carriage::{EVENT_IDENTITY_MEMBERS, MANAGED_EVENT_MEMBERS};

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "sse_carriage_tests.rs"]
mod carriage_tests;

#[cfg(test)]
#[path = "sse_tool_args_tests.rs"]
mod tool_args_tests;

#[cfg(test)]
#[path = "sse_passthrough_tests.rs"]
mod passthrough_tests;
