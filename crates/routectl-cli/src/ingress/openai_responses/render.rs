//! Canonical `ChatResponse` -> OpenAI Responses non-stream `response`
//! object.
//!
//! This is the EXACT INVERSE of the openai-responses egress response
//! PARSER (`routectl_providers::openai_responses::response::translate`):
//! the egress reads an upstream Responses body into canonical, this
//! renders a canonical `ChatResponse` back into the Responses wire body
//! a Codex / OpenAI-SDK client deserializes.
//!
//! Top-level shape: `{object:"response", id, created_at, model, status,
//! output[], usage}`.
//!
//! `status` + `incomplete_details` invert `map_finish_reason`:
//!
//! ```text
//! stop / tool_calls -> "completed"
//! length            -> "incomplete" + reason "max_output_tokens"
//! content_filter    -> "incomplete" + reason "content_filter"
//! error             -> "failed"
//! unknown / None    -> "completed" (a normal finished turn)
//! ```
//!
//! `output[]` items render in arrival order from the first choice's
//! message:
//!
//! ```text
//! assistant text       -> `message` item with `output_text` blocks
//! refusal part         -> `refusal` block on the message item
//! forward-compat Other -> re-emitted `{type, ...extras}` block
//! tool_calls           -> one `function_call` item each
//! reasoning_details    -> `reasoning` item(s) grouped by id
//! ```
//!
//! `usage` inverts `translate_usage`:
//!
//! ```text
//! prompt_tokens           -> input_tokens
//! completion_tokens       -> output_tokens
//! total_tokens            -> total_tokens
//! cache_read_input_tokens -> input_tokens_details.cached_tokens
//! reasoning_tokens        -> output_tokens_details.reasoning_tokens
//! ```
//!
//! Reasoning reconstruction mirrors the egress request-side replay
//! (`messages.rs::lift_reasoning_details`) so a round-trip is stable:
//! details are grouped by `id`; the FIRST Encrypted detail per id is the
//! item-level `encrypted_content` signature, any further Encrypted
//! details become inner `reasoning_encrypted` content blocks.

use serde_json::{Map, Value, json};

use routectl_core::{
    ChatResponse, ContentPart, KnownContentPart, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Result, is_responses_family,
};

/// Render a canonical `ChatResponse` into a Responses `response` object.
pub(super) fn render_responses_response(resp: ChatResponse) -> Result<Value> {
    let mut body = Map::new();
    body.insert("object".into(), Value::String("response".into()));
    body.insert("id".into(), Value::String(resp.id));
    body.insert("created_at".into(), json!(resp.created));
    body.insert("model".into(), Value::String(resp.model));

    let first = resp.choices.first();
    let finish_reason = first.and_then(|c| c.finish_reason.as_deref());

    let (status, incomplete_details) = status_from_finish_reason(finish_reason);
    body.insert("status".into(), Value::String(status));
    if let Some(details) = incomplete_details {
        body.insert("incomplete_details".into(), details);
    }

    let output = first.map(|c| build_output(&c.message)).unwrap_or_default();
    body.insert("output".into(), Value::Array(output));

    if let Some(usage) = resp.usage.as_ref().map(render_usage) {
        body.insert("usage".into(), usage);
    }

    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// status + incomplete_details (inverse of map_finish_reason)
// ---------------------------------------------------------------------------

/// Map a canonical `finish_reason` back to the Responses `status` +
/// optional `incomplete_details`. Inverse of the egress
/// `map_finish_reason`:
///   - "stop" / "tool_calls"  -> "completed" (no incomplete_details)
///   - "length"               -> "incomplete" + reason max_output_tokens
///   - "content_filter"       -> "incomplete" + reason content_filter
///   - "error"                -> "failed"
///   - None / anything else   -> "completed" (a finished turn is the
///     sensible default; the egress only ever emits the reasons above,
///     and a client treats an unknown finished turn as completed)
pub(super) fn status_from_finish_reason(finish_reason: Option<&str>) -> (String, Option<Value>) {
    match finish_reason {
        Some("length") => (
            "incomplete".to_string(),
            Some(json!({"reason": "max_output_tokens"})),
        ),
        Some("content_filter") => (
            "incomplete".to_string(),
            Some(json!({"reason": "content_filter"})),
        ),
        Some("error") => ("failed".to_string(), None),
        _ => ("completed".to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// output[] (inverse of walk_output)
// ---------------------------------------------------------------------------

/// Build the `output[]` array from a canonical assistant `Message`.
/// Arrival order mirrors the egress walk: reasoning items first, then a
/// message item (text + refusal + forward-compat blocks), then one
/// function_call item per tool_call.
fn build_output(msg: &Message) -> Vec<Value> {
    let mut output: Vec<Value> = Vec::new();

    output.extend(build_reasoning_items(&msg.reasoning_details));

    if let Some(message_item) = build_message_item(&msg.content, msg.refusal.as_deref()) {
        output.push(message_item);
    }

    output.extend(build_function_call_items(msg));

    output
}

/// Build a single `message` output item from the canonical message
/// content and optional refusal string. Returns None when there is no
/// renderable content (a pure tool-call or pure-reasoning turn emits no
/// message item). Plain text becomes an `output_text` block; a
/// `refusal` Other or the canonical `msg.refusal` string become a
/// `refusal` block; any other Other is re-emitted verbatim.
fn build_message_item(content: &MessageContent, refusal: Option<&str>) -> Option<Value> {
    let mut blocks = build_content_blocks(content);
    if let Some(r) = refusal {
        blocks.push(json!({"type": "refusal", "refusal": r}));
    }
    if blocks.is_empty() {
        return None;
    }
    Some(json!({
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": blocks,
    }))
}

fn build_content_blocks(content: &MessageContent) -> Vec<Value> {
    match content {
        MessageContent::Text(t) if !t.is_empty() => vec![output_text_block(t)],
        MessageContent::Text(_) | MessageContent::Null => Vec::new(),
        MessageContent::Parts(parts) => parts.iter().filter_map(content_block_from_part).collect(),
    }
}

/// Translate one canonical `ContentPart` into a Responses message content
/// block. Text -> `output_text`; the `refusal` Other -> `refusal`; any
/// other Other -> verbatim `{type, ...extras}` (forward-compat); ToolUse
/// parts are skipped here (they surface as `function_call` output items,
/// matching how the egress parsed function_call into both tool_calls and
/// a ToolUse part -- the renderer uses tool_calls as the source of truth
/// to avoid emitting the call twice).
fn content_block_from_part(part: &ContentPart) -> Option<Value> {
    match part {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => Some(output_text_block(text)),
        ContentPart::Known(_) => None,
        ContentPart::Other {
            type_tag, extras, ..
        } if type_tag == "refusal" => {
            let refusal = extras
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(json!({"type": "refusal", "refusal": refusal}))
        }
        ContentPart::Other {
            type_tag, extras, ..
        } => Some(other_block(type_tag, extras)),
    }
}

/// `output_text` content block. `annotations` is required by the
/// Responses wire shape (`ResponsesOutputContent::OutputText` always
/// deserializes it via `#[serde(default)]`); emit an empty array since
/// canonical does not carry annotations.
pub(super) fn output_text_block(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": []})
}

/// Re-emit a forward-compat `ContentPart::Other` as a Responses block,
/// lifting the `type_tag` back onto `type` and spreading the verbatim
/// extras. Inverse of the egress `split_other_value`.
fn other_block(type_tag: &str, extras: &Map<String, Value>) -> Value {
    let mut obj = extras.clone();
    obj.insert("type".into(), Value::String(type_tag.to_string()));
    Value::Object(obj)
}

/// Build one `function_call` output item per OpenAI-shape tool_call. The
/// canonical tool_call shape is `{id, type:"function", function:{name,
/// arguments}}` (arguments is a JSON string); the Responses item is
/// `{type:"function_call", call_id, name, arguments}`. Inverse of the
/// egress, which built tool_calls from FunctionCall items.
fn build_function_call_items(msg: &Message) -> Vec<Value> {
    let Some(tool_calls) = msg.tool_calls.as_ref() else {
        return Vec::new();
    };
    tool_calls.iter().map(function_call_item).collect()
}

pub(super) fn function_call_item(tc: &Value) -> Value {
    let call_id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
    let func = tc.get("function");
    let name = func
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = func
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

// ---------------------------------------------------------------------------
// reasoning items (inverse of the egress reasoning walk + lift_reasoning_details)
// ---------------------------------------------------------------------------

/// Bookkeeping for one Responses `reasoning` item under construction,
/// grouped by the canonical detail `id`.
#[derive(Default)]
struct ReasoningItemBuilder {
    summary: Vec<Value>,
    content: Vec<Value>,
    encrypted_content: Option<String>,
}

/// Reconstruct `reasoning` output items from canonical `reasoning_details`.
/// Groups by `id` (preserving first-seen order); only details whose format
/// tag belongs to the Responses family participate (a foreign-format
/// reasoning history would not deserialize into the Responses reasoning
/// shape).
///
/// Per-id reconstruction mirrors `messages.rs::lift_reasoning_details`:
///   - Summary  -> `summary[] { type:"summary_text", text }`
///   - Text     -> `content[] { type:"reasoning_text", text }`
///   - Encrypted: the FIRST per id becomes the item-level
///     `encrypted_content` signature; any further Encrypted detail
///     becomes an inner `content[] { type:"reasoning_encrypted",
///     encrypted_content }` block so no signature is lost.
fn build_reasoning_items(details: &[ReasoningDetail]) -> Vec<Value> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut groups: std::collections::HashMap<Option<String>, ReasoningItemBuilder> =
        std::collections::HashMap::new();

    for d in details {
        if !is_responses_family(d.format.as_deref()) {
            continue;
        }
        let key = d.id.clone();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        let group = groups.entry(key).or_default();
        accumulate_reasoning_detail(group, d);
    }

    let mut items: Vec<Value> = Vec::with_capacity(order.len());
    for key in order {
        let group = groups.remove(&key).expect("recorded in order");
        items.push(reasoning_item(key, group));
    }
    items
}

fn accumulate_reasoning_detail(group: &mut ReasoningItemBuilder, d: &ReasoningDetail) {
    match d.kind {
        ReasoningDetailKind::Summary => {
            if let Some(text) = d.payload.get("text").and_then(Value::as_str) {
                group
                    .summary
                    .push(json!({"type": "summary_text", "text": text}));
            }
        }
        ReasoningDetailKind::Text => {
            if let Some(text) = d.payload.get("text").and_then(Value::as_str) {
                group
                    .content
                    .push(json!({"type": "reasoning_text", "text": text}));
            }
        }
        ReasoningDetailKind::Encrypted => {
            if let Some(sig) = d.payload.get("encrypted_content").and_then(Value::as_str) {
                if group.encrypted_content.is_none() {
                    group.encrypted_content = Some(sig.to_string());
                } else {
                    group
                        .content
                        .push(json!({"type": "reasoning_encrypted", "encrypted_content": sig}));
                }
            }
        }
    }
}

/// Assemble a `reasoning` output item from a grouped builder. `id` is
/// emitted only when the canonical detail carried one (the egress's
/// stable upstream `rs_...` id); `encrypted_content` is emitted only when
/// a signature was present, matching the wire shape an SDK expects.
fn reasoning_item(id: Option<String>, group: ReasoningItemBuilder) -> Value {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("reasoning".into()));
    if let Some(id) = id {
        obj.insert("id".into(), Value::String(id));
    }
    obj.insert("summary".into(), Value::Array(group.summary));
    if !group.content.is_empty() {
        obj.insert("content".into(), Value::Array(group.content));
    }
    if let Some(sig) = group.encrypted_content {
        obj.insert("encrypted_content".into(), Value::String(sig));
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// usage (inverse of translate_usage)
// ---------------------------------------------------------------------------

/// Render canonical `Usage` into the Responses `usage` object. Inverse of
/// the egress `translate_usage`: the `input_tokens_details` /
/// `output_tokens_details` sub-objects are omitted entirely when their
/// source field is None (matching the wire shape a client expects --
/// no empty detail objects).
pub(super) fn render_usage(u: &routectl_core::Usage) -> Value {
    let mut obj = Map::new();
    obj.insert("input_tokens".into(), json!(u.prompt_tokens));
    if let Some(cached) = u.cache_read_input_tokens {
        obj.insert(
            "input_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    obj.insert("output_tokens".into(), json!(u.completion_tokens));
    if let Some(reasoning) = u.reasoning_tokens {
        obj.insert(
            "output_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }
    obj.insert("total_tokens".into(), json!(u.total_tokens));
    Value::Object(obj)
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
