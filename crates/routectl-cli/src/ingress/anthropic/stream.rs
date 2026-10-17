use serde_json::{Map, Value, json};

use routectl_core::{ChatChunk, Error, OpeningUsage, ReasoningDetail, Result, UsageDelta};

use crate::ingress::{
    IngressStreamState, ResponseEdits, SseEvent, StreamErrorClass, StreamRequestContext,
};

use super::stream_edits::{
    declare_estimated_start_usage, declare_minted_start_id, declare_opening_carry,
    declare_stop_drop, declare_straggler_drop, declare_usage_fill,
};
use super::stream_opaque::replay_opaque_events;
use super::{
    AnthropicStreamState, OpenBlockKind, ToolBlockState, cache_fields_into,
    openai_finish_to_anthropic_stop, random_msg_id,
};

/// SSE `event:` field name for Anthropic's terminal error event. Per
/// the Anthropic Messages SSE spec, `event: error` is itself terminal
/// (no further events follow). Module-level constant mirrors
/// `DONE_SENTINEL` in `openai.rs`. Kept private to this file because
/// the string is dialect-specific (the OpenAI ingress emits unnamed
/// frames, never a named `error` event).
const ANTHROPIC_ERROR_EVENT: &str = "error";

/// Maximum permitted `tool_call.index` value the Anthropic ingress
/// will allocate while translating an OpenAI-compat upstream stream
/// into Anthropic content blocks. Caps the per-stream `tool_blocks`
/// Vec growth so a malformed or adversarial upstream chunk carrying
/// `index: 1_000_000` cannot allocate gigabytes per stream. 4096 is
/// well above any legitimate parallel-tool-call breadth observed on
/// agentic harnesses.
const MAX_TOOL_CALL_INDEX: usize = 4096;

pub(super) fn anthropic_state_mut(s: &mut dyn IngressStreamState) -> &mut AnthropicStreamState {
    s.as_any_mut()
        .downcast_mut::<AnthropicStreamState>()
        .expect("AnthropicIngress::render_chunk got a non-Anthropic stream state")
}

#[cfg(test)]
pub(super) fn render_chunk_internal(
    chunk: ChatChunk,
    state: &mut AnthropicStreamState,
) -> Result<Vec<SseEvent>> {
    render_chunk_declared(chunk, state, &mut ResponseEdits::default())
}

/// Render one chunk, declaring into `edits` every response edit the render
/// makes on purpose.
pub(super) fn render_chunk_declared(
    chunk: ChatChunk,
    state: &mut AnthropicStreamState,
    edits: &mut ResponseEdits,
) -> Result<Vec<SseEvent>> {
    let events = render_chunk_events(chunk, state, edits)?;
    declare_minted_start_id(state.minted_start_id, edits);
    Ok(events)
}

/// Declare the edits the `message_start` already sent made: a minted id,
/// and the estimate standing in for an upstream first-event usage.
pub(super) fn declare_sent_start_edits(state: &AnthropicStreamState, edits: &mut ResponseEdits) {
    if !state.started {
        return;
    }
    declare_minted_start_id(state.minted_start_id, edits);
    declare_estimated_start_usage(state.estimated_start_usage, edits);
}

fn render_chunk_events(
    chunk: ChatChunk,
    state: &mut AnthropicStreamState,
    edits: &mut ResponseEdits,
) -> Result<Vec<SseEvent>> {
    let mut events: Vec<SseEvent> = Vec::new();

    // Monotonic terminal-state guard: once `message_stop` has fired,
    // any further chunk is a misbehaving-upstream straggler. Dropping
    // them prevents content_block_* or message_delta events from
    // landing on the wire AFTER `message_stop` (Anthropic protocol
    // violation). Operators see one WARN per occurrence so the
    // upstream's wire bug is visible without burying log volume.
    if state.finished {
        tracing::warn!(
            "anthropic ingress: dropping chunk arriving after message_stop \
             (chunk_id={}, has_delta={}, has_finish={}, has_usage={}, has_opaque_events={})",
            chunk.id,
            !chunk.choices.is_empty(),
            chunk
                .choices
                .first()
                .and_then(|c| c.finish_reason.as_deref())
                .is_some(),
            chunk.usage.is_some(),
            !chunk.opaque_events.is_empty(),
        );
        declare_straggler_drop(&chunk, edits);
        return Ok(events);
    }
    declare_opening_carry(&chunk, edits);

    // Cache id/model from the first chunk that carries them; both
    // are tolerated as missing on the wire.
    if !chunk.id.is_empty() && state.msg_id.is_none() {
        state.msg_id = Some(chunk.id.clone());
    }
    if !chunk.model.is_empty() && state.msg_model.is_none() {
        state.msg_model = Some(chunk.model.clone());
    }

    // Emit message_start once. When the chunk that opens the client stream
    // carries the upstream's own first-event usage, that count is the
    // opening; otherwise the request-seeded estimate is.
    if !state.started {
        let opening = chunk
            .upstream_meta
            .as_ref()
            .and_then(|meta| meta.opening_usage.as_ref());
        declare_estimated_start_usage(opening.is_none(), edits);
        emit_message_start(state, opening, &mut events);
        state.started = true;
    }

    // Replay opaque SSE events FIRST. The egress attaches them at
    // block boundaries, so the canonical text/thinking block (if any)
    // is conceptually closed before an opaque `content_block_start`
    // lands. Re-emit the captured raw bytes verbatim -- do NOT
    // round-trip through serde_json (re-encoding would lose
    // byte-for-byte fidelity for unknown nested types).
    if !chunk.opaque_events.is_empty() {
        replay_opaque_events(&chunk.opaque_events, state, &mut events);
    }

    // Walk the (single) choice's delta. Anthropic responses are
    // single-choice; n>1 would produce nonsense here.
    if let Some(choice) = chunk.choices.first() {
        emit_delta_events(&choice.delta, state, &mut events)?;

        if let Some(fr) = choice.finish_reason.as_deref() {
            flush_tool_blocks(state, &mut events);
            close_open_block(state, &mut events);
            if chunk.usage.is_some() {
                // Inline usage: emit combined delta + stop now.
                close_lingering_opaque_blocks(state, &mut events);
                emit_message_delta(
                    Some(fr),
                    choice.matched_stop_sequence.as_deref(),
                    chunk.usage.as_ref(),
                    &mut events,
                    edits,
                );
                emit_message_stop(state, &mut events);
            } else if state.pending_finish_reason.is_some() {
                // Second finish_reason on a stream that already buffered
                // one (without an intervening usage chunk to flush).
                // First-wins: preserve the original stop_reason and log
                // a WARN. Last-wins would silently rewrite the
                // upstream's protocol violation in our wire output.
                let finish_reason_safe = routectl_core::sanitize_for_log(fr);
                tracing::warn!(
                    "anthropic ingress: dropping second finish_reason \
                     (existing={:?}, new={finish_reason_safe}); upstream emitted two \
                     finish_reason chunks without an intervening usage \
                     chunk -- preserving the first",
                    state.pending_finish_reason,
                );
                declare_stop_drop(choice.matched_stop_sequence.is_some(), edits);
            } else {
                // Defer message_delta + message_stop until the usage
                // chunk arrives (or render_eos runs). OpenAI / OpenRouter
                // emit usage in a separate trailing chunk; emitting the
                // delta without usage now and a second delta after
                // message_stop is a protocol violation (the trailing
                // delta arrives post-stop).
                state.pending_finish_reason = Some(fr.to_string());
                state.pending_matched_stop_sequence = choice.matched_stop_sequence.clone();
            }
        }
    } else if let Some(usage) = chunk.usage.as_ref() {
        if let Some(fr) = state.pending_finish_reason.take() {
            // Finalize the buffered finish_reason now that we have usage.
            let matched = state.pending_matched_stop_sequence.take();
            close_lingering_opaque_blocks(state, &mut events);
            emit_message_delta(
                Some(&fr),
                matched.as_deref(),
                Some(usage),
                &mut events,
                edits,
            );
            emit_message_stop(state, &mut events);
        } else {
            // Mid-stream usage update (rare; some hosts emit interim
            // usage). Forward as a usage-only message_delta without
            // terminating the stream. (The outer `state.finished`
            // guard above handles the post-stop case; this branch only
            // fires while the stream is still active.)
            emit_message_delta(None, None, Some(usage), &mut events, edits);
        }
    }

    Ok(events)
}

/// Emit the stream's `message_start`. With `opening`, its usage is the
/// upstream's first-event input and cache breakdown field for field (the
/// cache fields stay disjoint from `input_tokens`); without it, the
/// request-seeded `input_tokens_estimate`.
pub(super) fn emit_message_start(
    state: &mut AnthropicStreamState,
    opening: Option<&OpeningUsage>,
    events: &mut Vec<SseEvent>,
) {
    state.minted_start_id = state.msg_id.is_none();
    state.estimated_start_usage = opening.is_none();
    let usage = opening.map_or_else(
        || json!({"input_tokens": state.input_tokens_estimate, "output_tokens": 0}),
        opening_usage_json,
    );
    let msg = json!({
        "type": "message_start",
        "message": {
            "id": state.msg_id.clone().unwrap_or_else(|| format!("msg_{}", random_msg_id())),
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": state.msg_model.clone()
                .or_else(|| state.req_model.clone())
                .unwrap_or_default(),
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": usage,
        }
    });
    events.push(SseEvent::named(
        "message_start",
        serde_json::to_string(&msg).unwrap_or_default(),
    ));
}

fn opening_usage_json(opening: &OpeningUsage) -> Value {
    let mut usage = Map::new();
    usage.insert("input_tokens".into(), json!(opening.input_tokens));
    usage.insert("output_tokens".into(), json!(0));
    cache_fields_into(
        &mut usage,
        opening.cache_creation_input_tokens,
        opening.cache_read_input_tokens,
        opening.cache_creation.as_ref(),
    );
    Value::Object(usage)
}

fn emit_delta_events(
    delta: &routectl_core::ChunkDelta,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) -> Result<()> {
    // Text content -> text_delta on the current text block.
    if let Some(text) = delta.content.as_deref()
        && !text.is_empty()
    {
        let idx = ensure_block(state, OpenBlockKind::Text, events);
        push_block_delta(events, idx, json!({"type": "text_delta", "text": text}));
    }

    // Reasoning details -> thinking_delta / signature_delta /
    // redacted_thinking blocks. Emits for ALL dialect formats
    // (deepseek-v1, vllm-reasoning-v1, openai-responses-v1,
    // openrouter, raw-think-tag-v1, anthropic-claude-v1). Anthropic
    // wire spec carries no format tag, just kind + payload. For
    // non-Anthropic formats the signature is null/empty; the
    // openai-compat egress's wire_lift/thinking extraction picks
    // these blocks back up on multi-turn echo so providers requiring
    // reasoning_content echo-back (DeepSeek-v4+, recent vLLM) get
    // a clean round-trip.
    for d in &delta.reasoning_details {
        match d.kind {
            routectl_core::ReasoningDetailKind::Text => {
                // Pass the upstream detail index so two distinct
                // thinking blocks (e.g. provider emits index=0 then
                // index=1 in the same response) emit as two separate
                // Anthropic content blocks. `ensure_block` compares
                // the full `OpenBlockKind` via PartialEq, so an
                // index change forces a content_block_stop +
                // content_block_start pair.
                let detail_index = d.index.unwrap_or(0);
                let idx = ensure_block(state, OpenBlockKind::Thinking { detail_index }, events);
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str())
                    && !text.is_empty()
                {
                    push_block_delta(
                        events,
                        idx,
                        json!({"type": "thinking_delta", "thinking": text}),
                    );
                }
                if let Some(sig) = d.payload.get("signature").and_then(|v| v.as_str()) {
                    push_block_delta(
                        events,
                        idx,
                        json!({"type": "signature_delta", "signature": sig}),
                    );
                }
            }
            routectl_core::ReasoningDetailKind::Encrypted => {
                // Redacted thinking: no incremental delta path; emit a
                // stand-alone block_start + block_stop with the data.
                close_open_block(state, events);
                let idx = state.next_index;
                state.next_index += 1;
                // Foreign-scheme artifacts ride out self-describing so
                // the echo back is replayable; Anthropic-sourced bytes
                // pass through verbatim. See `encrypted_detail_data`.
                let data = super::encrypted_detail_data(d);
                events.push(SseEvent::named(
                    "content_block_start",
                    serde_json::to_string(&json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "redacted_thinking", "data": data},
                    }))
                    .unwrap_or_default(),
                ));
                events.push(SseEvent::named(
                    "content_block_stop",
                    serde_json::to_string(&json!({
                        "type": "content_block_stop",
                        "index": idx,
                    }))
                    .unwrap_or_default(),
                ));
            }
            routectl_core::ReasoningDetailKind::Summary
            | routectl_core::ReasoningDetailKind::Other(_) => {
                // OpenAI Responses summary text -> thinking_delta.
                // Lets cc display the summary AND keeps the text in
                // the round-trip history for any preserve-mode echo.
                // An unrecognized kind (Other) gets the identical
                // best-effort forward: same lane-class rationale as the
                // non-stream renderer's Summary arm.
                let detail_index = d.index.unwrap_or(0);
                let idx = ensure_block(state, OpenBlockKind::Thinking { detail_index }, events);
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str())
                    && !text.is_empty()
                {
                    push_block_delta(
                        events,
                        idx,
                        json!({"type": "thinking_delta", "thinking": text}),
                    );
                }
            }
        }
        // Suppress unused warning when ReasoningDetail comes in handy
        // for downstream readers.
        let _: &ReasoningDetail = d;
    }

    // Tool calls -> tool_use blocks with input_json_delta.
    if let Some(tcs) = delta.tool_calls.as_ref() {
        for tc in tcs {
            apply_tool_call_delta(tc, state, events)?;
        }
    }

    Ok(())
}

fn apply_tool_call_delta(
    tc: &Value,
    state: &mut AnthropicStreamState,
    _events: &mut Vec<SseEvent>,
) -> Result<()> {
    // OpenAI shape: {index, id?, type, function: {name?, arguments?}}.
    let call_index = tc
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    if call_index > MAX_TOOL_CALL_INDEX {
        return Err(Error::Streaming(format!(
            "anthropic ingress: tool_call index {call_index} exceeds maximum of {MAX_TOOL_CALL_INDEX}"
        )));
    }

    // Allocate a new tool_use block on first sight of this call_index.
    while state.tool_blocks.len() <= call_index {
        state.tool_blocks.push(ToolBlockState::default());
    }
    let block = &mut state.tool_blocks[call_index];
    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
        block.id = id.to_string();
    }
    if let Some(name) = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
    {
        block.name = name.to_string();
    }

    if let Some(args) = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        && !args.is_empty()
    {
        block.partial_json.push_str(args);
    }
    Ok(())
}

pub(super) fn flush_tool_blocks(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    let buffered = std::mem::take(&mut state.tool_blocks);
    for block in buffered
        .into_iter()
        .filter(|b| !b.id.is_empty() || !b.name.is_empty())
    {
        close_open_block(state, events);
        let idx = state.next_index;
        state.next_index += 1;
        state.open = Some((idx, OpenBlockKind::ToolUse));
        events.push(SseEvent::named(
            "content_block_start",
            serde_json::to_string(&json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": block.id,
                    "name": block.name,
                    "input": {},
                },
            }))
            .unwrap_or_default(),
        ));
        if !block.partial_json.is_empty() {
            push_block_delta(
                events,
                idx,
                json!({"type": "input_json_delta", "partial_json": block.partial_json}),
            );
        }
    }
}

/// Open a content block of `kind` if one isn't already open. Returns
/// the block index in either case (whether it was already open or
/// just opened). Returning the index removes the
/// `state.open.unwrap().0` foot-gun at every call site.
fn ensure_block(
    state: &mut AnthropicStreamState,
    kind: OpenBlockKind,
    events: &mut Vec<SseEvent>,
) -> usize {
    if let Some((idx, k)) = state.open
        && k == kind
    {
        return idx;
    }
    close_open_block(state, events);
    let idx = state.next_index;
    state.next_index += 1;
    state.open = Some((idx, kind));
    let block = match kind {
        OpenBlockKind::Text => json!({"type": "text", "text": ""}),
        OpenBlockKind::Thinking { .. } => json!({"type": "thinking", "thinking": ""}),
        OpenBlockKind::ToolUse => json!({"type": "tool_use", "id": "", "name": "", "input": {}}),
    };
    events.push(SseEvent::named(
        "content_block_start",
        serde_json::to_string(&json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": block,
        }))
        .unwrap_or_default(),
    ));
    idx
}

pub(super) fn close_open_block(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    if let Some((idx, _)) = state.open.take() {
        events.push(SseEvent::named(
            "content_block_stop",
            serde_json::to_string(&json!({
                "type": "content_block_stop",
                "index": idx,
            }))
            .unwrap_or_default(),
        ));
    }
}

/// Close every opaque content block whose `content_block_start` was
/// replayed to the client but whose upstream `content_block_stop`
/// never arrived. Invariant: before any `message_stop`, every opaque
/// block start replayed on the wire must be matched by a stop -- a
/// strict Anthropic SDK / claude-code consumer rejects an unmatched
/// block. Opaque blocks live in `opaque_index_map` (keyed by upstream
/// index, ascending) rather than `state.open`, so `close_open_block`
/// does not reach them; this helper drains that map.
///
/// Consumes the map via `std::mem::take`, so calling it more than once
/// per stream is a harmless no-op: the terminal chunk paths and
/// `render_eos` can each invoke it without risking a duplicate
/// `content_block_stop`.
pub(super) fn close_lingering_opaque_blocks(
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) {
    for (_, ingress_index) in std::mem::take(&mut state.opaque_index_map) {
        events.push(SseEvent::named(
            "content_block_stop",
            format!("{{\"type\":\"content_block_stop\",\"index\":{ingress_index}}}"),
        ));
    }
}

fn push_block_delta(events: &mut Vec<SseEvent>, idx: usize, delta: Value) {
    events.push(SseEvent::named(
        "content_block_delta",
        serde_json::to_string(&json!({
            "type": "content_block_delta",
            "index": idx,
            "delta": delta,
        }))
        .unwrap_or_default(),
    ));
}

pub(super) fn emit_message_delta(
    finish_reason: Option<&str>,
    matched_stop_sequence: Option<&str>,
    usage: Option<&UsageDelta>,
    events: &mut Vec<SseEvent>,
    edits: &mut ResponseEdits,
) {
    let mut delta = Map::new();
    // When the upstream surfaced a matched stop sequence, override the
    // OpenAI mapping with the native `stop_sequence` shape. Otherwise
    // fall back to the lossy `finish_reason -> stop_reason` mapping
    // and emit a null `stop_sequence`.
    let (stop_reason_value, stop_sequence_value) = if let Some(seq) = matched_stop_sequence {
        (
            Value::String("stop_sequence".into()),
            Value::String(seq.to_string()),
        )
    } else {
        (
            finish_reason.map_or(Value::Null, |fr| {
                Value::String(openai_finish_to_anthropic_stop(fr).into())
            }),
            Value::Null,
        )
    };
    delta.insert("stop_reason".into(), stop_reason_value);
    delta.insert("stop_sequence".into(), stop_sequence_value);

    let mut payload = Map::new();
    payload.insert("type".into(), Value::String("message_delta".into()));
    payload.insert("delta".into(), Value::Object(delta));

    if let Some(u) = usage {
        let mut wire_usage = Map::new();
        // Anthropic's wire semantics: `input_tokens` is the RAW input
        // portion (cache_creation and cache_read are separate fields).
        // Canonical `prompt_tokens` is the summed total, so subtract
        // cache fields to recover the raw value. Real Anthropic emits
        // `input_tokens` on message_delta mirroring the message_start
        // value, and downstream consumers need it because routectl's
        // `message_start` carries only the selected opening (an upstream
        // first-event count or an estimate), not the terminal count.
        //
        // Always emit both `input_tokens` and `output_tokens` on the
        // closing delta. Anthropic requires both fields; when the
        // upstream UsageDelta omits either, default to 0 rather than
        // omitting the key entirely.
        declare_usage_fill(u, edits);
        let raw_input = u
            .prompt_tokens
            .unwrap_or(0)
            .saturating_sub(u.cache_creation_input_tokens.unwrap_or(0))
            .saturating_sub(u.cache_read_input_tokens.unwrap_or(0));
        wire_usage.insert("input_tokens".into(), json!(raw_input));
        wire_usage.insert(
            "output_tokens".into(),
            json!(u.completion_tokens.unwrap_or(0)),
        );
        cache_fields_into(
            &mut wire_usage,
            u.cache_creation_input_tokens,
            u.cache_read_input_tokens,
            u.cache_creation.as_ref(),
        );
        // Guard: only attach `usage` to the payload when the map carries
        // at least one key. Both `input_tokens` and `output_tokens` are
        // always present above, so this is defensive -- it prevents a
        // degenerate empty `usage: {}` object should the construction
        // logic change.
        if !wire_usage.is_empty() {
            payload.insert("usage".into(), Value::Object(wire_usage));
        }
    }

    events.push(SseEvent::named(
        "message_delta",
        serde_json::to_string(&Value::Object(payload)).unwrap_or_default(),
    ));
}

pub(super) fn emit_message_stop(state: &mut AnthropicStreamState, events: &mut Vec<SseEvent>) {
    if state.finished {
        return;
    }
    state.finished = true;
    events.push(SseEvent::named(
        "message_stop",
        serde_json::to_string(&json!({"type": "message_stop"})).unwrap_or_default(),
    ));
}

/// Build the dialect-specific terminal error event for an upstream
/// stream failure. Per Anthropic SSE spec, `event: error` is itself
/// terminal -- no further events follow. The renderer marks
/// `state.finished` so any straggler chunks are dropped by the
/// `render_chunk_internal` post-stop guard (defensive: the spawned
/// task returns immediately after this and the state is dropped
/// anyway, but keeping the invariant prevents a future refactor
/// from silently regressing).
///
/// The error message is sanitized via `routectl_core::sanitize_for_log_with_cap`
/// to filter control chars (CRLF, ANSI escapes, NULs) that would
/// otherwise break SSE framing or forge log lines on downstream
/// text-format subscribers. The caller already strips provider names,
/// upstream bodies, and tokens; this is defense in depth.
pub(super) fn render_error_eos_internal(
    state: &mut AnthropicStreamState,
    error: &dyn std::fmt::Display,
    class: &StreamErrorClass,
) -> Vec<SseEvent> {
    // Fix #5: a late error arriving after a normal clean finish (e.g.
    // an upstream transport error that fires after message_stop has
    // already been emitted) must not push a second terminal event.
    // Anthropic clients treat the `event: error` frame as terminal and
    // a duplicate after message_stop would violate the SSE protocol
    // invariant that no events follow message_stop.
    if state.finished {
        return Vec::new();
    }
    state.finished = true;
    let msg = routectl_core::sanitize_for_log_with_cap(
        &error.to_string(),
        crate::ingress::STREAM_ERROR_MESSAGE_CAP,
    );
    let payload = json!({
        "type": "error",
        "error": {
            "type": class.anthropic_type,
            "message": msg,
        }
    });
    vec![SseEvent::named(
        ANTHROPIC_ERROR_EVENT,
        serde_json::to_string(&payload)
            .expect("Value serialization is infallible for BTreeMap-backed literals"),
    )]
}

// ---------------------------------------------------------------------------
// IngressAdapter impl
// ---------------------------------------------------------------------------

/// Construct an `AnthropicStreamState` seeded from the request context:
/// the resolved model (so `message_start` can emit the correct model id
/// even when the upstream stream's first chunk carries an empty model
/// field) and the local input-token estimate (so the synthesized
/// `message_start` reports a non-zero `usage.input_tokens`). An empty
/// model string is stored as `None` so the `message_start` fallback
/// chain falls through to the synthesized-default branch. Everything
/// else defaults.
pub(super) fn new_state(ctx: &StreamRequestContext) -> AnthropicStreamState {
    AnthropicStreamState {
        req_model: (!ctx.model.is_empty()).then(|| ctx.model.clone()),
        input_tokens_estimate: ctx.input_tokens_estimate,
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
