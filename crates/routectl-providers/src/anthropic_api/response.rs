//! Response normalization: Anthropic wire format -> routectl shape.
//!
//! Cache-stats handling: the upstream Anthropic / Bedrock-Invoke
//! `usage` object carries `cache_creation_input_tokens`,
//! `cache_read_input_tokens`, and a per-TTL `cache_creation` breakdown.
//! These now flow into the canonical `Usage` so OpenAI-SSE clients see
//! the same totals at end-of-stream. Forward-compat: unknown content
//! block types (server_tool_use, web_search_tool_result,
//! code_execution_tool_result, ...) are accepted on the wire as
//! `ContentBlock::Other` and dropped from the flat-text output with
//! a `tracing::warn!` so the client sees what was lost.

use chrono::Utc;
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatResponse, Choice, ContentPart, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Result, Role, Usage,
};

use super::types::{AnthropicResponse, AnthropicUsage, ContentBlock};

const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Map Anthropic stop_reason to OpenAI finish_reason.
pub fn map_stop_reason(stop_reason: Option<&str>) -> Option<String> {
    let reason = match stop_reason? {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        other => other,
    };
    Some(reason.to_string())
}

/// Walk content blocks and produce (text, reasoning_details, tool_calls,
/// parts). The first three mirror the legacy flat-text shape; `parts`
/// preserves every block (text + thinking + image + tool_use + Other)
/// in arrival order so non-text content can flow through to ingresses
/// that support typed content (the Anthropic ingress passes parts back
/// as Anthropic blocks; the OpenAI ingress collapses to a flat string).
/// Thinking and RedactedThinking are intentionally omitted from `parts`
/// because reasoning_details is the canonical surface for them; the
/// Anthropic ingress reconstructs `thinking` blocks from
/// reasoning_details on egress.
#[allow(clippy::type_complexity)] // 5-tuple matches the wire shape; alias would obscure intent
pub(crate) fn walk_content_blocks(
    id: &str,
    blocks: &[ContentBlock],
) -> Result<(
    String,
    Vec<ReasoningDetail>,
    Option<Vec<Value>>,
    Vec<ContentPart>,
)> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_details: Vec<ReasoningDetail> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut detail_index: u32 = 0;
    let mut parts: Vec<ContentPart> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"text": thinking, "signature": signature}),
                });
                detail_index += 1;
            }
            ContentBlock::RedactedThinking { data, .. } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Encrypted,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"data": data}),
                });
                detail_index += 1;
            }
            ContentBlock::Text { text, .. } => {
                text_parts.push(text.clone());
                parts.push(ContentPart::Known(KnownContentPart::Text {
                    text: text.clone(),
                    cache_control: None,
                }));
            }
            ContentBlock::ToolUse {
                id: tool_id,
                name,
                input,
                ..
            } => {
                // Convert to OpenAI tool_call shape.
                let arguments = serde_json::to_string(input)
                    .map_err(|e| Error::normalize_response(id, e.to_string()))?;
                tool_calls.push(json!({
                    "id": tool_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }));
                parts.push(ContentPart::Known(KnownContentPart::ToolUse {
                    id: tool_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    cache_control: None,
                }));
            }
            ContentBlock::ToolResult { .. } => {
                // Not expected in a response; skip.
            }
            ContentBlock::Image { source, .. } => {
                warn!(
                    provider = id,
                    "image block in response dropped from flat-text output"
                );
                parts.push(ContentPart::Known(KnownContentPart::Image {
                    source: source.clone(),
                    cache_control: None,
                }));
            }
            ContentBlock::Document {
                source,
                title,
                citations,
                ..
            } => {
                warn!(
                    provider = id,
                    "document block in response dropped from flat-text output"
                );
                parts.push(ContentPart::Known(KnownContentPart::Document {
                    source: source.clone(),
                    title: title.clone(),
                    citations: citations.clone(),
                    cache_control: None,
                }));
            }
            ContentBlock::Other {
                type_tag, extras, ..
            } => {
                warn!(
                    provider = id,
                    block_type = %type_tag,
                    "unknown content block type in response dropped from flat-text output",
                );
                parts.push(ContentPart::Other {
                    type_tag: type_tag.clone(),
                    cache_control: None,
                    extras: extras.clone(),
                });
            }
        }
    }

    let text = text_parts.join("");
    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    Ok((text, reasoning_details, tool_calls_opt, parts))
}

/// Choose between `MessageContent::Text(joined)` and
/// `MessageContent::Parts(parts)`. When the response only contained Text
/// blocks (i.e. parts.len() == count of Text-flavored entries), collapse
/// to flat Text for OpenAI-ingress wire stability. Otherwise emit Parts
/// so multimodal/forward-compat blocks ride through to ingresses that
/// preserve them. Thinking/RedactedThinking aren't in `parts` -- they
/// surface via reasoning_details -- so a response with only text +
/// thinking still collapses cleanly to Text here, while text + image
/// emits Parts.
// TODO(M12): extract to a providers-internal shared module; the
// twin in crates/routectl-providers/src/bedrock/converse/response.rs
// is byte-identical. M12 (managed-key dedup wave) is the natural
// landing for this.
fn select_message_content(text: String, parts: Vec<ContentPart>) -> MessageContent {
    let only_text = parts
        .iter()
        .all(|p| matches!(p, ContentPart::Known(KnownContentPart::Text { .. })));
    if only_text {
        MessageContent::Text(text)
    } else {
        MessageContent::Parts(parts)
    }
}

/// Translate Anthropic `usage` into the canonical `Usage`, including
/// the cache-stats extension (cache_creation_input_tokens,
/// cache_read_input_tokens, per-TTL breakdown).
///
/// Anthropic reports `input_tokens` as ONLY the new, non-cached tokens.
/// OpenAI's `prompt_tokens` is the full prompt size (with cached subset
/// reported separately). To stay OpenAI-spec correct on the wire,
/// `prompt_tokens` is the SUM of new + cache-creation + cache-read
/// inputs, while the per-bucket breakdown stays available on the
/// extension fields.
fn translate_usage(u: &AnthropicUsage) -> Usage {
    let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    let prompt_tokens = u
        .input_tokens
        .saturating_add(cache_creation)
        .saturating_add(cache_read);
    Usage {
        prompt_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: prompt_tokens.saturating_add(u.output_tokens),
        reasoning_tokens: u.reasoning_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation: u.cache_creation.as_ref().map(|c| CacheCreation {
            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
        }),
    }
}

pub fn normalize(id: &str, raw: Value) -> Result<ChatResponse> {
    let resp: AnthropicResponse =
        serde_json::from_value(raw).map_err(|e| Error::normalize_response(id, e.to_string()))?;

    let (text, reasoning_details, tool_calls, parts) = walk_content_blocks(id, &resp.content)?;
    let usage = resp.usage.as_ref().map(translate_usage);
    let finish_reason = map_stop_reason(resp.stop_reason.as_deref());

    let content = select_message_content(text, parts);
    let message = Message {
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details,
        name: None,
        tool_call_id: None,
        tool_calls,
    };

    Ok(ChatResponse {
        id: resp.id,
        model: resp.model,
        created: Utc::now().timestamp(),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
        }],
        usage,
        routectl_provider: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_usage_fields_propagate_to_canonical() {
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 10,
                "cache_creation_input_tokens": 4096,
                "cache_read_input_tokens": 8192,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 2048,
                    "ephemeral_1h_input_tokens": 2048
                }
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.unwrap();
        assert_eq!(u.cache_creation_input_tokens, Some(4096));
        assert_eq!(u.cache_read_input_tokens, Some(8192));
        let cc = u.cache_creation.unwrap();
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(2048));
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(2048));
    }

    #[test]
    fn prompt_tokens_sums_input_plus_cache_creation_plus_cache_read() {
        // Anthropic reports input_tokens as ONLY the new (non-cached)
        // tokens. OpenAI's prompt_tokens is the full prompt size.
        // Translation must sum the three buckets so OpenAI clients
        // reading the canonical response see the cumulative context
        // size, not just the new turn's tokens.
        let raw = json!({
            "id": "msg_03",
            "model": "claude-opus-4-7",
            "content": [{"type":"text","text":"ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 10,
                "cache_creation_input_tokens": 4096,
                "cache_read_input_tokens": 8192
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.unwrap();
        // 50 + 4096 + 8192 = 12338
        assert_eq!(u.prompt_tokens, 12338);
        // total_tokens reflects the summed prompt + completion.
        assert_eq!(u.total_tokens, 12338 + 10);
        // Per-bucket breakdown still available for clients that want it.
        assert_eq!(u.cache_creation_input_tokens, Some(4096));
        assert_eq!(u.cache_read_input_tokens, Some(8192));
    }

    #[test]
    fn prompt_tokens_with_no_cache_equals_input_tokens() {
        // First turn (or non-cached path): cache_* = None, so
        // prompt_tokens == input_tokens unchanged.
        let raw = json!({
            "id": "msg_04",
            "model": "claude-opus-4-7",
            "content": [{"type":"text","text":"ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.total_tokens, 120);
    }

    #[test]
    fn unknown_block_in_response_does_not_panic() {
        // Forward-compat: a future Anthropic response with a
        // server_tool_use block must parse without error and surface
        // the rest of the content cleanly.
        let raw = json!({
            "id": "msg_02",
            "model": "claude-opus-4-7",
            "content": [
                {"type": "server_tool_use", "id": "srvtu_01", "name": "web_search", "input": {}},
                {"type": "text", "text": "after"}
            ],
            "stop_reason": "end_turn"
        });
        let resp = normalize("test", raw).unwrap();
        let msg = &resp.choices[0].message;
        // The presence of an Other block forces Parts emission so the
        // forward-compat block survives end-to-end.
        match &msg.content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    ContentPart::Other { type_tag, .. } => {
                        assert_eq!(type_tag, "server_tool_use");
                    }
                    other => panic!("expected Other, got {other:?}"),
                }
                match &parts[1] {
                    ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                        assert_eq!(text, "after");
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn text_only_response_collapses_to_message_content_text() {
        // OpenAI-ingress wire stability: when the response is purely
        // Text blocks (the common path), keep emitting flat
        // MessageContent::Text so existing OpenAI clients see the
        // same shape they always have.
        let raw = json!({
            "id": "msg_05",
            "model": "claude-opus-4-7",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"}
            ],
            "stop_reason": "end_turn"
        });
        let resp = normalize("test", raw).unwrap();
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_plus_thinking_response_still_collapses_to_text() {
        // Thinking lives on reasoning_details, not on parts. A response
        // with text + thinking still has only Text in `parts`, so
        // select_message_content collapses to MessageContent::Text and
        // the signature rides on reasoning_details for replay.
        let raw = json!({
            "id": "msg_06",
            "model": "claude-opus-4-7",
            "content": [
                {"type": "thinking", "thinking": "step 1", "signature": "sig_x"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        });
        let resp = normalize("test", raw).unwrap();
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "answer"),
            other => panic!("expected Text, got {other:?}"),
        }
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].payload["text"], "step 1");
        assert_eq!(details[0].payload["signature"], "sig_x");
    }

    #[test]
    fn forward_compat_unknown_block_emits_parts_preserving_other() {
        // [text, future_block_v2, text] -- the unknown block forces
        // Parts emission so the operator's forward-compat block
        // survives the response normalization. The Other variant
        // preserves the original type_tag and extras.
        let raw = json!({
            "id": "msg_07",
            "model": "claude-opus-4-7",
            "content": [
                {"type": "text", "text": "before"},
                {"type": "future_block_v2", "custom_field": "x"},
                {"type": "text", "text": "after"}
            ],
            "stop_reason": "end_turn"
        });
        let resp = normalize("test", raw).unwrap();
        match &resp.choices[0].message.content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 3);
                match &parts[1] {
                    ContentPart::Other {
                        type_tag, extras, ..
                    } => {
                        assert_eq!(type_tag, "future_block_v2");
                        assert_eq!(
                            extras.get("custom_field").and_then(|v| v.as_str()),
                            Some("x")
                        );
                    }
                    other => panic!("expected Other, got {other:?}"),
                }
            }
            other => panic!("expected Parts, got {other:?}"),
        }
    }
}
