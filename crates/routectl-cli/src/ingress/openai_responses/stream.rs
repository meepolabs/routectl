//! OpenAI Responses SSE lifecycle renderer.
//!
//! This is the INVERSE of the openai-responses EGRESS SSE reader
//! (`routectl_providers::openai_responses::sse`): the egress consumes
//! `response.*` events into canonical `ChatChunk`s; this renderer
//! PRODUCES those same events from canonical `ChatChunk` deltas.
//!
//! The canonical chunk stream is delta-only -- it carries `delta.content`
//! (text), `delta.reasoning_details`, `delta.tool_calls`, a terminal
//! `finish_reason`, and `usage`, but NO explicit item start/end signals.
//! The Responses SSE protocol is a fully bracketed lifecycle:
//!
//! ```text
//! response.created
//!   -> response.output_item.added (message)
//!        -> response.content_part.added (output_text)
//!             -> response.output_text.delta*
//!        -> response.output_text.done
//!        -> response.content_part.done
//!   -> response.output_item.done
//!   ... (reasoning / function_call items) ...
//! response.completed
//! ```
//!
//! So this state machine SYNTHESIZES the item/part brackets: it opens an
//! item the first time a kind appears, routes subsequent deltas to it,
//! and closes it (emitting the `*.done` + `output_item.done`) when a
//! different item supersedes it or at EOS. This mirrors how the
//! anthropic ingress (`ingress::anthropic::stream`) opens/closes
//! `content_block_start`/`stop` from the same delta stream.
//!
//! Index scheme:
//! - `sequence_number` increments by one for EVERY emitted event across
//!   the whole stream (Responses protocol requirement).
//! - `output_index` is a dense per-item counter (message / reasoning /
//!   each function_call gets the next value).
//! - message text uses `content_index` 0 for its single `output_text`
//!   part.
//!
//! Completed-body parity: the renderer accumulates the streamed text,
//! reasoning details, and tool calls into a canonical `Message`, then
//! builds the `response.completed` body via the non-stream
//! renderer (`render::render_responses_response`). That guarantees the
//! terminal body matches the non-stream render byte-for-byte. The
//! per-item shapes emitted on `output_item.added` / `.done` reuse the
//! same `render` helpers (`output_text_block`, `function_call_item`,
//! `reasoning_item`).

use serde_json::{Map, Value, json};
use uuid::Uuid;

use routectl_core::{
    ChatChunk, ChatResponse, ChunkDelta, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Result, Role, Usage, UsageDelta, is_responses_family, schema::Choice,
};

use crate::ingress::{IngressStreamState, SseEvent, StreamErrorClass};

use super::render::{
    function_call_item, output_text_block, render_responses_response, status_from_finish_reason,
};
use super::{OpenOutputItem, ResponsesStreamState, ToolCallBuffer};

/// Per-stream cap on the number of buffered function-call indices. A
/// legitimate turn emits a small handful of parallel tool calls; an
/// adversarial upstream could stream thousands of distinct indices to
/// drive the buffer toward OOM. 4096 mirrors the anthropic ingress cap.
const MAX_TOOL_CALL_INDEX: usize = 4096;

/// Downcast the boxed stream state to the concrete Responses state.
/// Panics on a mismatched concrete type -- a wiring bug (the handler
/// pairs `new_stream_state` with `render_chunk` for the same adapter),
/// never a runtime input condition.
pub(super) fn state_mut(s: &mut dyn IngressStreamState) -> &mut ResponsesStreamState {
    s.as_any_mut()
        .downcast_mut::<ResponsesStreamState>()
        .expect("ResponsesIngress::render_chunk got a non-Responses stream state")
}

// ---------------------------------------------------------------------------
// render_chunk
// ---------------------------------------------------------------------------

pub(super) fn render_chunk_internal(
    chunk: ChatChunk,
    state: &mut ResponsesStreamState,
) -> Result<Vec<SseEvent>> {
    let mut events: Vec<SseEvent> = Vec::new();

    // Post-completion straggler guard: once the terminal event fired,
    // any further chunk would land after `response.completed` (a
    // protocol violation). Drop with a WARN so the misbehaving upstream
    // is visible without breaking the wire.
    if state.finished {
        tracing::warn!(
            "openai-responses ingress: dropping chunk after terminal event \
             (has_delta={}, has_finish={}, has_usage={})",
            !chunk.choices.is_empty(),
            chunk
                .choices
                .first()
                .and_then(|c| c.finish_reason.as_deref())
                .is_some(),
            chunk.usage.is_some(),
        );
        return Ok(events);
    }

    capture_identity(&chunk, state);
    ensure_created(state, &mut events);

    if let Some(choice) = chunk.choices.first() {
        emit_delta_events(&choice.delta, state, &mut events);
        if let Some(fr) = choice.finish_reason.as_deref() {
            stash_finish(fr, chunk.usage.as_ref(), state);
        }
    }
    if chunk.choices.is_empty()
        && let Some(usage) = chunk.usage.as_ref()
    {
        // Trailing usage-only chunk (OpenAI-compat upstreams emit
        // usage in a separate final chunk). Stash it for the
        // completed body; render_eos flushes the terminal event.
        state.pending_usage = Some(usage.clone());
    }

    Ok(events)
}

/// Cache the response id / model / created_at from the first chunk that
/// carries them. All are tolerated as missing on the wire (canonical
/// chunks may omit id/model; canonical has no `created` on a chunk).
fn capture_identity(chunk: &ChatChunk, state: &mut ResponsesStreamState) {
    if !chunk.id.is_empty() && state.response_id.is_none() {
        state.response_id = Some(chunk.id.clone());
    }
    if !chunk.model.is_empty() && state.response_model.is_none() {
        state.response_model = Some(chunk.model.clone());
    }
}

/// Emit `response.created` (+ `response.in_progress`) exactly once.
fn ensure_created(state: &mut ResponsesStreamState, events: &mut Vec<SseEvent>) {
    if state.started {
        return;
    }
    state.started = true;
    let created = response_skeleton(state, "in_progress", Vec::new(), None);
    push_response_event(state, events, "response.created", created.clone());
    push_response_event(state, events, "response.in_progress", created);
}

/// Route one canonical delta onto the open item(s). Text and reasoning
/// open/close incrementally; tool calls buffer for a terminal flush.
fn emit_delta_events(
    delta: &ChunkDelta,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
) {
    if let Some(text) = delta.content.as_deref()
        && !text.is_empty()
    {
        emit_text_delta(text, state, events);
    }

    for d in &delta.reasoning_details {
        emit_reasoning_detail(d, state, events);
    }

    if let Some(tcs) = delta.tool_calls.as_ref() {
        for tc in tcs {
            buffer_tool_call(tc, state);
        }
    }
}

// ---------------------------------------------------------------------------
// Text item lifecycle
// ---------------------------------------------------------------------------

fn emit_text_delta(text: &str, state: &mut ResponsesStreamState, events: &mut Vec<SseEvent>) {
    let output_index = ensure_text_item(state, events);
    state.text_accumulator.push_str(text);
    state.current_text.push_str(text);
    push_event(
        state,
        events,
        "response.output_text.delta",
        json!({
            "output_index": output_index,
            "content_index": 0,
            "delta": text,
        }),
    );
}

/// Open a `message` item (+ its `output_text` content part) if one is
/// not already open, closing any superseded item first. Returns the
/// message item's `output_index`.
fn ensure_text_item(state: &mut ResponsesStreamState, events: &mut Vec<SseEvent>) -> u64 {
    if let Some(OpenOutputItem::Text { output_index }) = state.open {
        return output_index;
    }
    close_open_item(state, events);
    let output_index = alloc_output_index(state);
    state.open = Some(OpenOutputItem::Text { output_index });
    state.current_text.clear();
    push_event(
        state,
        events,
        "response.output_item.added",
        json!({
            "output_index": output_index,
            "item": {
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": [],
            },
        }),
    );
    push_event(
        state,
        events,
        "response.content_part.added",
        json!({
            "output_index": output_index,
            "content_index": 0,
            "part": output_text_block(""),
        }),
    );
    output_index
}

// ---------------------------------------------------------------------------
// Reasoning item lifecycle
// ---------------------------------------------------------------------------

fn emit_reasoning_detail(
    d: &ReasoningDetail,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
) {
    // Every detail rides into the completed body via the accumulator
    // (the non-stream renderer filters to the Responses format when
    // building it).
    state.reasoning_accumulator.push(d.clone());

    // Only Responses-family reasoning participates in the streamed
    // lifecycle. A foreign-format detail (normal when the turn went to a
    // non-Responses upstream) must NOT open an item or emit a delta:
    // gating only the delta would still leak an `output_index` gap via
    // `ensure_reasoning_item` and absent it from the completed body.
    if !is_responses_family(d.format.as_deref()) {
        return;
    }

    let output_index = ensure_reasoning_item(d.id.clone(), state, events);
    let event_name = match d.kind {
        ReasoningDetailKind::Summary => "response.reasoning_summary_text.delta",
        ReasoningDetailKind::Text => "response.reasoning_text.delta",
        // Encrypted details have no incremental delta channel; the
        // signature rides out on the item.done body (mirrors the egress
        // `handle_item_done`). Nothing to stream here.
        ReasoningDetailKind::Encrypted => return,
        // Unrecognized kind: no delta channel is defined for it
        // either, same reasoning as Encrypted.
        ReasoningDetailKind::Other(_) => return,
    };
    let Some(text) = d.payload.get("text").and_then(Value::as_str) else {
        return;
    };
    // Advance the per-item part counter only for a detail that actually
    // streams, so the streamed part indices match the completed body's
    // `summary[]` / `content[]` positions one-for-one.
    let data = match d.kind {
        ReasoningDetailKind::Summary => json!({
            "output_index": output_index,
            "summary_index": next_reasoning_summary_index(state),
            "delta": text,
        }),
        _ => json!({
            "output_index": output_index,
            "content_index": next_reasoning_content_index(state),
            "delta": text,
        }),
    };
    push_event(state, events, event_name, data);
}

/// Take the open reasoning item's `summary_index` and advance it. The
/// open item is always `Reasoning` here (set by `ensure_reasoning_item`);
/// falls back to 0 defensively.
const fn next_reasoning_summary_index(state: &mut ResponsesStreamState) -> u64 {
    if let Some(OpenOutputItem::Reasoning { summary_index, .. }) = &mut state.open {
        let idx = *summary_index;
        *summary_index += 1;
        idx
    } else {
        0
    }
}

/// Take the open reasoning item's `content_index` and advance it. See
/// `next_reasoning_summary_index`.
const fn next_reasoning_content_index(state: &mut ResponsesStreamState) -> u64 {
    if let Some(OpenOutputItem::Reasoning { content_index, .. }) = &mut state.open {
        let idx = *content_index;
        *content_index += 1;
        idx
    } else {
        0
    }
}

/// Open a `reasoning` item if one is not already open, closing any
/// superseded item first. Re-uses the open item when the detail id
/// matches (the non-stream renderer groups reasoning details by id).
fn ensure_reasoning_item(
    detail_id: Option<String>,
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
) -> u64 {
    if let Some(OpenOutputItem::Reasoning {
        output_index,
        detail_id: open_id,
        ..
    }) = &state.open
        && open_id == &detail_id
    {
        return *output_index;
    }
    close_open_item(state, events);
    let output_index = alloc_output_index(state);
    state.open = Some(OpenOutputItem::Reasoning {
        output_index,
        detail_id: detail_id.clone(),
        summary_index: 0,
        content_index: 0,
    });
    let mut item = Map::new();
    item.insert("type".into(), Value::String("reasoning".into()));
    if let Some(id) = detail_id {
        item.insert("id".into(), Value::String(id));
    }
    item.insert("summary".into(), Value::Array(Vec::new()));
    push_event(
        state,
        events,
        "response.output_item.added",
        json!({
            "output_index": output_index,
            "item": Value::Object(item),
        }),
    );
    output_index
}

// ---------------------------------------------------------------------------
// Function-call buffering + flush
// ---------------------------------------------------------------------------

fn buffer_tool_call(tc: &Value, state: &mut ResponsesStreamState) {
    let call_index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    if call_index >= MAX_TOOL_CALL_INDEX {
        tracing::warn!(
            call_index,
            max = MAX_TOOL_CALL_INDEX,
            "openai-responses ingress: tool_call index exceeds cap; dropping",
        );
        return;
    }
    while state.tool_buffers.len() <= call_index {
        state.tool_buffers.push(ToolCallBuffer::default());
    }
    let buf = &mut state.tool_buffers[call_index];
    if let Some(id) = tc.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        buf.id = id.to_string();
    }
    let func = tc.get("function");
    if let Some(name) = func.and_then(|f| f.get("name")).and_then(Value::as_str)
        && !name.is_empty()
    {
        buf.name = name.to_string();
    }
    if let Some(args) = func
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
    {
        buf.arguments.push_str(args);
    }
}

/// Emit the full lifecycle for every buffered function-call item:
/// `output_item.added` -> `function_call_arguments.delta` ->
/// `function_call_arguments.done` -> `output_item.done`.
fn flush_tool_calls(state: &mut ResponsesStreamState, events: &mut Vec<SseEvent>) {
    let buffers = std::mem::take(&mut state.tool_buffers);
    for buf in buffers
        .into_iter()
        .filter(|b| !b.id.is_empty() || !b.name.is_empty())
    {
        close_open_item(state, events);
        let output_index = alloc_output_index(state);
        let item = function_call_item(&tool_call_value(&buf));
        push_event(
            state,
            events,
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        );
        if !buf.arguments.is_empty() {
            push_event(
                state,
                events,
                "response.function_call_arguments.delta",
                json!({"output_index": output_index, "delta": buf.arguments}),
            );
        }
        push_event(
            state,
            events,
            "response.function_call_arguments.done",
            json!({"output_index": output_index, "arguments": buf.arguments}),
        );
        let done_item = function_call_item(&tool_call_value(&buf));
        push_event(
            state,
            events,
            "response.output_item.done",
            json!({"output_index": output_index, "item": done_item}),
        );
    }
}

/// Build the canonical OpenAI-shape tool_call value the
/// `render::function_call_item` helper consumes.
fn tool_call_value(buf: &ToolCallBuffer) -> Value {
    json!({
        "id": buf.id,
        "type": "function",
        "function": {"name": buf.name, "arguments": buf.arguments},
    })
}

// ---------------------------------------------------------------------------
// Item close (emit the *.done bracket for the open text/reasoning item)
// ---------------------------------------------------------------------------

fn close_open_item(state: &mut ResponsesStreamState, events: &mut Vec<SseEvent>) {
    match state.open.take() {
        Some(OpenOutputItem::Text { output_index }) => {
            let text = std::mem::take(&mut state.current_text);
            push_event(
                state,
                events,
                "response.output_text.done",
                json!({
                    "output_index": output_index,
                    "content_index": 0,
                    "text": text,
                }),
            );
            push_event(
                state,
                events,
                "response.content_part.done",
                json!({
                    "output_index": output_index,
                    "content_index": 0,
                    "part": output_text_block(&text),
                }),
            );
            push_event(
                state,
                events,
                "response.output_item.done",
                json!({
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [output_text_block(&text)],
                    },
                }),
            );
        }
        Some(OpenOutputItem::Reasoning {
            output_index,
            detail_id,
            ..
        }) => {
            push_event(
                state,
                events,
                "response.output_item.done",
                json!({
                    "output_index": output_index,
                    "item": reasoning_done_item(detail_id, &state.reasoning_accumulator),
                }),
            );
        }
        None => {}
    }
}

/// Assemble the `reasoning` item body for `output_item.done` from the
/// accumulated details that share this item's id. Mirrors the non-stream
/// renderer's reasoning grouping (summary -> `summary[]`, text/encrypted ->
/// `content[]` + item-level `encrypted_content`).
fn reasoning_done_item(detail_id: Option<String>, details: &[ReasoningDetail]) -> Value {
    let mut summary: Vec<Value> = Vec::new();
    let mut content: Vec<Value> = Vec::new();
    let mut encrypted: Option<String> = None;
    for d in details
        .iter()
        .filter(|d| d.id == detail_id && is_responses_family(d.format.as_deref()))
    {
        match d.kind {
            ReasoningDetailKind::Summary => {
                if let Some(t) = d.payload.get("text").and_then(Value::as_str) {
                    summary.push(json!({"type": "summary_text", "text": t}));
                }
            }
            ReasoningDetailKind::Text => {
                if let Some(t) = d.payload.get("text").and_then(Value::as_str) {
                    content.push(json!({"type": "reasoning_text", "text": t}));
                }
            }
            ReasoningDetailKind::Encrypted => {
                if let Some(sig) = d.payload.get("encrypted_content").and_then(Value::as_str) {
                    if encrypted.is_none() {
                        encrypted = Some(sig.to_string());
                    } else {
                        content
                            .push(json!({"type": "reasoning_encrypted", "encrypted_content": sig}));
                    }
                }
            }
            // Same reasoning as `accumulate_reasoning_detail`: no slot
            // exists for an arbitrary shape, so an unrecognized kind
            // contributes nothing.
            ReasoningDetailKind::Other(_) => {}
        }
    }
    let mut item = Map::new();
    item.insert("type".into(), Value::String("reasoning".into()));
    if let Some(id) = detail_id {
        item.insert("id".into(), Value::String(id));
    }
    item.insert("summary".into(), Value::Array(summary));
    if !content.is_empty() {
        item.insert("content".into(), Value::Array(content));
    }
    if let Some(sig) = encrypted {
        item.insert("encrypted_content".into(), Value::String(sig));
    }
    Value::Object(item)
}

/// Buffer the terminal `finish_reason` (and any inline usage) for the
/// `response.completed` body emitted at EOS. First-wins on a duplicate
/// finish_reason, matching the anthropic ingress.
fn stash_finish(fr: &str, usage: Option<&UsageDelta>, state: &mut ResponsesStreamState) {
    if state.pending_finish_reason.is_none() {
        state.pending_finish_reason = Some(fr.to_string());
    } else {
        tracing::warn!(
            existing = ?state.pending_finish_reason,
            new = fr,
            "openai-responses ingress: dropping second finish_reason; preserving the first",
        );
    }
    if let Some(u) = usage {
        state.pending_usage = Some(u.clone());
    }
}

// ---------------------------------------------------------------------------
// render_eos -- terminal response.completed
// ---------------------------------------------------------------------------

pub(super) fn render_eos_internal(state: &mut ResponsesStreamState) -> Vec<SseEvent> {
    if state.finished {
        return Vec::new();
    }
    let mut events: Vec<SseEvent> = Vec::new();

    // An empty stream still owes the client a protocol-valid envelope.
    ensure_created(state, &mut events);

    close_open_item(state, &mut events);
    let finished_output = completed_output(state);
    flush_tool_calls(state, &mut events);

    let finish_reason = state.pending_finish_reason.clone();
    let (status, incomplete_details) = status_from_finish_reason(finish_reason.as_deref());
    let mut body = response_skeleton(state, &status, finished_output, state.pending_usage.clone());
    if let (Some(obj), Some(details)) = (body.as_object_mut(), incomplete_details) {
        obj.insert("incomplete_details".into(), details);
    }
    // A "failed" status is delivered via the distinct `response.failed`
    // event -- real Responses SDKs and the egress reader key off the event
    // NAME, so `response.completed(status=failed)` is non-conformant. The
    // accumulated usage and partial output already sit in `body`; only the
    // event name diverges (branch around the shared skeleton, not inside).
    let event_name = if status == "failed" {
        "response.failed"
    } else {
        "response.completed"
    };
    push_response_event(state, &mut events, event_name, body);
    state.finished = true;
    events
}

/// Build the `output[]` for the completed body from the accumulated
/// canonical message, via the non-stream renderer so the body
/// matches the non-stream render byte-for-byte.
fn completed_output(state: &ResponsesStreamState) -> Vec<Value> {
    let resp = accumulated_response(state);
    let rendered = render_responses_response(resp).unwrap_or_else(|_| json!({"output": []}));
    rendered
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Reconstruct a canonical `ChatResponse` from the accumulated stream so
/// the non-stream renderer produces the completed body. Tool calls become
/// `message.tool_calls`; text becomes the message content; reasoning
/// details ride on the message.
fn accumulated_response(state: &ResponsesStreamState) -> ChatResponse {
    let content = if state.text_accumulator.is_empty() {
        MessageContent::Null
    } else {
        MessageContent::Text(state.text_accumulator.clone())
    };
    let tool_calls: Vec<Value> = state
        .tool_buffers
        .iter()
        .filter(|b| !b.id.is_empty() || !b.name.is_empty())
        .map(tool_call_value)
        .collect();
    let message = Message {
        refusal: None,
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details: state.reasoning_accumulator.clone(),
        name: None,
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
    };
    ChatResponse {
        id: response_id(state),
        model: state.response_model.clone().unwrap_or_default(),
        created: state.created_at,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message,
            finish_reason: state.pending_finish_reason.clone(),
            matched_stop_sequence: None,
        }],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    }
}

// ---------------------------------------------------------------------------
// render_error_eos -- terminal response.failed
// ---------------------------------------------------------------------------

/// Emit a terminal `response.failed` event so a Responses client sees a
/// clean failure rather than a truncated stream. `response.failed` (not
/// `response.completed`) is the discriminator: the egress reader treats
/// it as a fatal `Err` (`sse::handle_failed`), so a client distinguishes
/// it from success. The message is sanitized via `sanitize_for_log` to
/// strip control chars that would break SSE framing.
pub(super) fn render_error_eos_internal(
    state: &mut ResponsesStreamState,
    error: &dyn std::fmt::Display,
    class: &StreamErrorClass,
) -> Vec<SseEvent> {
    if state.finished {
        return Vec::new();
    }
    let mut events: Vec<SseEvent> = Vec::new();
    // A client that never saw `response.created` would reject a bare
    // `response.failed`; open the envelope first if needed.
    ensure_created(state, &mut events);
    close_open_item(state, &mut events);

    let msg = routectl_core::sanitize_for_log(&error.to_string());
    let mut body = response_skeleton(state, "failed", Vec::new(), None);
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "error".into(),
            json!({"type": class.openai_type, "code": class.openai_code, "message": msg}),
        );
    }
    push_response_event(state, &mut events, "response.failed", body);
    state.finished = true;
    events
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

const fn alloc_output_index(state: &mut ResponsesStreamState) -> u64 {
    let idx = state.next_output_index;
    state.next_output_index += 1;
    idx
}

fn response_id(state: &ResponsesStreamState) -> String {
    state
        .response_id
        .clone()
        .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple()))
}

/// The `response` object embedded in `response.created` / `.completed` /
/// `.failed`. `created`/`completed`/`failed` share the envelope shape;
/// only `status`, `output`, and `usage` differ.
fn response_skeleton(
    state: &mut ResponsesStreamState,
    status: &str,
    output: Vec<Value>,
    usage: Option<UsageDelta>,
) -> Value {
    // Pin the response id once so every event echoes the same value
    // even when the upstream omitted one (we mint it on first use).
    let id = response_id(state);
    if state.response_id.is_none() {
        state.response_id = Some(id.clone());
    }
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(id));
    obj.insert("object".into(), Value::String("response".into()));
    obj.insert("created_at".into(), json!(state.created_at));
    obj.insert(
        "model".into(),
        Value::String(state.response_model.clone().unwrap_or_default()),
    );
    obj.insert("status".into(), Value::String(status.into()));
    obj.insert("output".into(), Value::Array(output));
    if let Some(u) = usage {
        obj.insert(
            "usage".into(),
            super::render::render_usage(&usage_from_delta(&u)),
        );
    }
    Value::Object(obj)
}

/// Lift a streaming `UsageDelta` into the canonical `Usage` the
/// `render::render_usage` helper consumes, so the streamed usage object
/// matches the non-stream render.
fn usage_from_delta(u: &UsageDelta) -> Usage {
    Usage {
        prompt_tokens: u.prompt_tokens.unwrap_or(0),
        completion_tokens: u.completion_tokens.unwrap_or(0),
        total_tokens: u.total_tokens.unwrap_or(0),
        reasoning_tokens: u.reasoning_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation: u.cache_creation.clone(),
        server_tool_use: u.server_tool_use.clone(),
        extras: Default::default(),
    }
}

/// Push an event whose `data` is `{type, sequence_number, ...extras}`.
fn push_event(
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    event_name: &str,
    mut extras: Value,
) {
    let seq = next_seq(state);
    let obj = extras
        .as_object_mut()
        .expect("push_event extras must be a JSON object");
    obj.insert("type".into(), Value::String(event_name.into()));
    obj.insert("sequence_number".into(), json!(seq));
    match serde_json::to_string(&extras) {
        Ok(json) => events.push(SseEvent::named(event_name, json)),
        Err(e) => tracing::error!(
            "openai-responses ingress: failed to serialize SSE event {event_name}: {e}"
        ),
    }
}

/// Push an event whose payload is `{type, sequence_number, response}`.
fn push_response_event(
    state: &mut ResponsesStreamState,
    events: &mut Vec<SseEvent>,
    event_name: &str,
    response: Value,
) {
    push_event(state, events, event_name, json!({"response": response}));
}

const fn next_seq(state: &mut ResponsesStreamState) -> u64 {
    let seq = state.sequence_number;
    state.sequence_number += 1;
    seq
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
