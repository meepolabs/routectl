use serde_json::{json, Map, Value};

use routectl_core::{ChatResponse, ContentPart, Message, MessageContent};

use super::openai_finish_to_anthropic_stop;

// ---------------------------------------------------------------------------
// Response rendering
// ---------------------------------------------------------------------------

/// Render canonical `ChatResponse` into an Anthropic Messages response
/// body shape. Mirrors what `api.anthropic.com /v1/messages` would
/// emit: `{id, type:"message", role:"assistant", model, content[],
/// stop_reason, stop_sequence, usage}`.
pub(super) fn render_messages_response(resp: ChatResponse) -> Value {
    let mut body = Map::new();
    body.insert("id".into(), Value::String(resp.id));
    body.insert("type".into(), Value::String("message".into()));
    body.insert("role".into(), Value::String("assistant".into()));
    body.insert("model".into(), Value::String(resp.model));

    let content = resp
        .choices
        .first()
        .map(|c| build_content_array(&c.message))
        .unwrap_or_default();
    body.insert("content".into(), Value::Array(content));

    let stop_reason = resp
        .choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(|fr| openai_finish_to_anthropic_stop(fr).to_string());
    body.insert(
        "stop_reason".into(),
        stop_reason.map(Value::String).unwrap_or(Value::Null),
    );
    body.insert("stop_sequence".into(), Value::Null);

    let usage = resp.usage.as_ref().map(|u| {
        // Anthropic's `input_tokens` is the RAW input portion; cache
        // fields are separate. Canonical `prompt_tokens` is the summed
        // total (OpenAI semantics), so subtract cache fields to recover
        // the raw input that the Anthropic spec wants on the wire.
        let raw_input = u
            .prompt_tokens
            .saturating_sub(u.cache_creation_input_tokens.unwrap_or(0))
            .saturating_sub(u.cache_read_input_tokens.unwrap_or(0));
        let mut usage_obj = json!({
            "input_tokens": raw_input,
            "output_tokens": u.completion_tokens,
            "cache_creation_input_tokens": u.cache_creation_input_tokens,
            "cache_read_input_tokens": u.cache_read_input_tokens,
            "cache_creation": u.cache_creation.as_ref().map(|c| json!({
                "ephemeral_5m_input_tokens": c.ephemeral_5m_input_tokens,
                "ephemeral_1h_input_tokens": c.ephemeral_1h_input_tokens,
            })),
        });
        // Forward-compat: emit unknown usage sub-fields (e.g.
        // `service_tier`) that flowed into `extras` from upstream.
        // Typed fields above win on key conflict.
        if let Some(map) = usage_obj.as_object_mut() {
            for (k, v) in &u.extras {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        usage_obj
    });
    if let Some(u) = usage {
        body.insert("usage".into(), u);
    }

    // Forward-compat: emit unknown response top-level fields (e.g.
    // `context_management`) that flowed into `ChatResponse.extras`
    // from upstream. Typed fields above win on key conflict. The
    // drop-list belongs here -- routectl owns the egress wire policy.
    for (k, v) in &resp.extras {
        // `stop_details` is a Bedrock-only Anthropic-API extension
        // not in the public Anthropic Messages baseline.
        if k == "stop_details" {
            continue;
        }
        body.entry(k.clone()).or_insert_with(|| v.clone());
    }

    Value::Object(body)
}

/// Build the Anthropic `content[]` array from a canonical assistant
/// `Message`. Order: thinking blocks (in detail-index order) first,
/// then tool_use blocks (one per tool_call), then any text content.
fn build_content_array(msg: &Message) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    // Thinking + redacted_thinking blocks from reasoning_details.
    // We emit for ALL dialect formats (anthropic-claude-v1,
    // deepseek-v1, vllm-reasoning-v1, openai-responses-v1,
    // openrouter, raw-think-tag-v1, ...). Anthropic's wire spec
    // doesn't carry a format tag -- only the canonical kind + payload
    // are needed. For Anthropic-sourced details, the upstream
    // signature passes through. For non-Anthropic, signature is
    // null/empty; cc + Anthropic SDK accept this. The openai-compat
    // egress's wire_lift/thinking extraction picks the blocks back
    // up on multi-turn echo so deepseek-v4-pro and vLLM (which
    // require reasoning_content echo-back) get a clean round-trip.
    let mut details = msg.reasoning_details.clone();
    details.sort_by_key(|d| d.index.unwrap_or(0));
    for d in &details {
        match &d.kind {
            routectl_core::ReasoningDetailKind::Text => {
                let text = d
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let signature = d.payload.get("signature").cloned().unwrap_or(Value::Null);
                blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": signature,
                }));
            }
            routectl_core::ReasoningDetailKind::Encrypted => {
                // Encrypted detail; field name on the wire differs by
                // dialect:
                //   - Anthropic: `data` (opaque encrypted payload)
                //   - OpenAI Responses: `encrypted_content`
                //   - OpenRouter passthrough: `data`
                // Read whichever is populated.
                let data = d
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .or_else(|| d.payload.get("encrypted_content").and_then(|v| v.as_str()))
                    .unwrap_or_default();
                blocks.push(json!({
                    "type": "redacted_thinking",
                    "data": data,
                }));
            }
            routectl_core::ReasoningDetailKind::Summary => {
                // OpenAI Responses emits per-step reasoning summaries
                // as Summary-kind details. Surface them as thinking
                // blocks so cc displays them (and so they round-trip
                // on multi-turn echo if the upstream uses summaries
                // as its reasoning carrier).
                let text = d
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !text.is_empty() {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": Value::Null,
                    }));
                }
            }
        }
    }

    // Pre-scan `msg.content` for ToolUse parts and collect their ids.
    // Some egresses populate BOTH the OpenAI-shape `msg.tool_calls`
    // AND the typed `ContentPart::ToolUse` part for the same upstream
    // function_call (openai-responses, anthropic-api response paths).
    // Without dedup, the renderer below would emit the same tool_use
    // block twice in the Anthropic `content` array -- once from the
    // tool_calls loop, once from the parts iteration. cc and the
    // Anthropic SDK tolerate the dupes during streaming (where the
    // dedup runs on chunk-state) but they break the non-streaming
    // path with two identical blocks back-to-back. Source of truth on
    // dedup: parts wins -- it carries the Anthropic-native shape and
    // any cache_control on the tool_use block, whereas the
    // tool_calls loop synthesizes a fresh block without those
    // sub-fields. Bug D (cc-via-* 2026-05-18).
    //
    // The scan covers BOTH `ContentPart::Known(KnownContentPart::ToolUse)`
    // (the typed-known shape) AND `ContentPart::Other` whose type_tag
    // is "tool_use" (the forward-compat shape -- happens when a future
    // Anthropic tool_use sub-field shows up that breaks
    // KnownContentPart::ToolUse's serde struct, so the deserializer
    // falls through to Other). Without the Other coverage a future
    // wire change reintroduces the duplicate-tool_use bug on the
    // all-Anthropic path.
    let mut parts_tool_use_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if let MessageContent::Parts(parts) = &msg.content {
        for p in parts {
            match p {
                ContentPart::Known(routectl_core::KnownContentPart::ToolUse { id, .. }) => {
                    parts_tool_use_ids.insert(id.clone());
                }
                ContentPart::Other {
                    type_tag, extras, ..
                } if type_tag == "tool_use" => {
                    if let Some(id) = extras.get("id").and_then(|v| v.as_str()) {
                        parts_tool_use_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // Tool-use blocks from tool_calls (OpenAI shape). Skip any whose
    // id already appears as a ContentPart::ToolUse in `msg.content`
    // (the parts iteration below will emit those in Anthropic-native
    // shape).
    if let Some(tcs) = msg.tool_calls.as_ref() {
        for tc in tcs {
            let id_value = tc.get("id").cloned().unwrap_or(Value::Null);
            if let Some(id_str) = id_value.as_str() {
                if parts_tool_use_ids.contains(id_str) {
                    continue;
                }
            }
            let func = tc.get("function").and_then(|v| v.as_object());
            let name = func
                .and_then(|f| f.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            blocks.push(json!({
                "type": "tool_use",
                "id": id_value,
                "name": name,
                "input": args,
            }));
        }
    }

    // Text content -- last so it follows thinking + tool_use, matching
    // Anthropic's natural ordering.
    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            blocks.push(json!({"type": "text", "text": t}));
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let Ok(v) = serde_json::to_value(p) {
                    blocks.push(v);
                }
            }
        }
        _ => {}
    }

    blocks
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
