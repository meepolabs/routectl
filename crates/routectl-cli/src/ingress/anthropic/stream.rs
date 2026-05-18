use super::*;

pub(super) fn anthropic_state_mut(s: &mut dyn IngressStreamState) -> &mut AnthropicStreamState {
    s.as_any_mut()
        .downcast_mut::<AnthropicStreamState>()
        .expect("AnthropicIngress::render_chunk got a non-Anthropic stream state")
}

pub(super) fn render_chunk_internal(
    chunk: ChatChunk,
    state: &mut AnthropicStreamState,
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
             (chunk_id={}, has_delta={}, has_finish={}, has_usage={})",
            chunk.id,
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

    // Cache id/model from the first chunk that carries them; both
    // are tolerated as missing on the wire.
    if !chunk.id.is_empty() && state.msg_id.is_none() {
        state.msg_id = Some(chunk.id.clone());
    }
    if !chunk.model.is_empty() && state.msg_model.is_none() {
        state.msg_model = Some(chunk.model.clone());
    }

    // Emit message_start once.
    if !state.started {
        emit_message_start(state, &mut events);
        state.started = true;
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
                emit_message_delta(Some(fr), chunk.usage.as_ref(), &mut events);
                emit_message_stop(state, &mut events);
            } else if state.pending_finish_reason.is_some() {
                // Second finish_reason on a stream that already buffered
                // one (without an intervening usage chunk to flush).
                // First-wins: preserve the original stop_reason and log
                // a WARN. Last-wins would silently rewrite the
                // upstream's protocol violation in our wire output.
                tracing::warn!(
                    "anthropic ingress: dropping second finish_reason \
                     (existing={:?}, new={fr}); upstream emitted two \
                     finish_reason chunks without an intervening usage \
                     chunk -- preserving the first",
                    state.pending_finish_reason,
                );
            } else {
                // Defer message_delta + message_stop until the usage
                // chunk arrives (or render_eos runs). OpenAI / OpenRouter
                // emit usage in a separate trailing chunk; emitting the
                // delta without usage now and a second delta after
                // message_stop is a protocol violation (the trailing
                // delta arrives post-stop).
                state.pending_finish_reason = Some(fr.to_string());
            }
        }
    } else if let Some(usage) = chunk.usage.as_ref() {
        if let Some(fr) = state.pending_finish_reason.take() {
            // Finalize the buffered finish_reason now that we have usage.
            emit_message_delta(Some(&fr), Some(usage), &mut events);
            emit_message_stop(state, &mut events);
        } else {
            // Mid-stream usage update (rare; some hosts emit interim
            // usage). Forward as a usage-only message_delta without
            // terminating the stream. (The outer `state.finished`
            // guard above handles the post-stop case; this branch only
            // fires while the stream is still active.)
            emit_message_delta(None, Some(usage), &mut events);
        }
    }

    Ok(events)
}

fn emit_message_start(state: &AnthropicStreamState, events: &mut Vec<SseEvent>) {
    let msg = json!({
        "type": "message_start",
        "message": {
            "id": state.msg_id.clone().unwrap_or_else(|| format!("msg_{}", random_msg_id())),
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": state.msg_model.clone().unwrap_or_default(),
            "stop_reason": Value::Null,
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }
    });
    events.push(SseEvent::named(
        "message_start",
        serde_json::to_string(&msg).unwrap_or_default(),
    ));
}

fn emit_delta_events(
    delta: &routectl_core::ChunkDelta,
    state: &mut AnthropicStreamState,
    events: &mut Vec<SseEvent>,
) -> Result<()> {
    // Text content -> text_delta on the current text block.
    if let Some(text) = delta.content.as_deref() {
        if !text.is_empty() {
            let idx = ensure_block(state, OpenBlockKind::Text, events);
            push_block_delta(events, idx, json!({"type": "text_delta", "text": text}));
        }
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
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        push_block_delta(
                            events,
                            idx,
                            json!({"type": "thinking_delta", "thinking": text}),
                        );
                    }
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
                // `data` (Anthropic / OpenRouter passthrough) or
                // `encrypted_content` (OpenAI Responses).
                let data = d
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .or_else(|| d.payload.get("encrypted_content").and_then(|v| v.as_str()))
                    .unwrap_or_default();
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
            routectl_core::ReasoningDetailKind::Summary => {
                // OpenAI Responses summary text -> thinking_delta.
                // Lets cc display the summary AND keeps the text in
                // the round-trip history for any preserve-mode echo.
                let detail_index = d.index.unwrap_or(0);
                let idx = ensure_block(state, OpenBlockKind::Thinking { detail_index }, events);
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        push_block_delta(
                            events,
                            idx,
                            json!({"type": "thinking_delta", "thinking": text}),
                        );
                    }
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

/// Maximum permitted tool_call index in a streaming response. Caps
/// `state.tool_blocks` Vec growth so a malicious or malformed upstream
/// chunk with `index: 1_000_000` cannot allocate gigabytes per stream.
/// Anthropic-API limits are far below this; openai-compat upstreams
/// rarely exceed 16 parallel tool calls.
const MAX_TOOL_CALL_INDEX: usize = 64;

fn apply_tool_call_delta(
    tc: &Value,
    state: &mut AnthropicStreamState,
    _events: &mut Vec<SseEvent>,
) -> Result<()> {
    // OpenAI shape: {index, id?, type, function: {name?, arguments?}}.
    let call_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
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
    {
        if !args.is_empty() {
            block.partial_json.push_str(args);
        }
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
    if let Some((idx, k)) = state.open {
        if k == kind {
            return idx;
        }
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
    usage: Option<&routectl_core::UsageDelta>,
    events: &mut Vec<SseEvent>,
) {
    let mut delta = Map::new();
    delta.insert(
        "stop_reason".into(),
        finish_reason
            .map(|fr| Value::String(openai_finish_to_anthropic_stop(fr).into()))
            .unwrap_or(Value::Null),
    );
    delta.insert("stop_sequence".into(), Value::Null);

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
        // emit_message_start hardcodes {input_tokens:0, output_tokens:0}.
        if let Some(prompt) = u.prompt_tokens {
            let raw_input = prompt
                .saturating_sub(u.cache_creation_input_tokens.unwrap_or(0))
                .saturating_sub(u.cache_read_input_tokens.unwrap_or(0));
            wire_usage.insert("input_tokens".into(), json!(raw_input));
        }
        if let Some(n) = u.completion_tokens {
            wire_usage.insert("output_tokens".into(), json!(n));
        }
        if let Some(n) = u.cache_creation_input_tokens {
            wire_usage.insert("cache_creation_input_tokens".into(), json!(n));
        }
        if let Some(n) = u.cache_read_input_tokens {
            wire_usage.insert("cache_read_input_tokens".into(), json!(n));
        }
        if let Some(c) = u.cache_creation.as_ref() {
            let mut cc = Map::new();
            if let Some(n) = c.ephemeral_5m_input_tokens {
                cc.insert("ephemeral_5m_input_tokens".into(), json!(n));
            }
            if let Some(n) = c.ephemeral_1h_input_tokens {
                cc.insert("ephemeral_1h_input_tokens".into(), json!(n));
            }
            wire_usage.insert("cache_creation".into(), Value::Object(cc));
        }
        payload.insert("usage".into(), Value::Object(wire_usage));
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

// ---------------------------------------------------------------------------
// IngressAdapter impl
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
