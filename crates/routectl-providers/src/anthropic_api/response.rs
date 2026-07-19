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
use serde_json::{Value, json};
use std::collections::HashMap;
use tracing::warn;
use uuid::Uuid;

use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatResponse, Choice, ContentPart, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Result, Role, Usage,
};

use super::types::{AnthropicResponse, AnthropicUsage, ContentBlock};

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
                    format: Some(super::ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"text": thinking, "signature": signature}),
                });
                detail_index += 1;
            }
            ContentBlock::RedactedThinking { data, .. } => {
                reasoning_details.push(ReasoningDetail {
                    kind: ReasoningDetailKind::Encrypted,
                    id: Some(Uuid::new_v4().to_string()),
                    format: Some(super::ANTHROPIC_FORMAT.to_string()),
                    index: Some(detail_index),
                    payload: json!({"data": data}),
                });
                detail_index += 1;
            }
            ContentBlock::Text {
                text, citations, ..
            } => {
                text_parts.push(text.clone());
                parts.push(ContentPart::Known(KnownContentPart::Text {
                    text: text.clone(),
                    citations: citations.clone(),
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

/// Sum the Anthropic-vocabulary input buckets into the canonical
/// `prompt_tokens`. Anthropic and Bedrock-Converse both report the
/// raw `input_tokens` field as ONLY the new, non-cached tokens, while
/// OpenAI's `prompt_tokens` is the full prompt size. To stay
/// OpenAI-spec correct on the wire, `prompt_tokens` is the saturating
/// SUM of new + cache-creation + cache-read inputs; the per-bucket
/// breakdown stays available on the canonical extension fields.
///
/// Shared by `anthropic_api::response::translate_usage` and
/// `bedrock::converse::response::translate_usage` so the summing rule
/// cannot drift between the two Anthropic-vocabulary egresses (the
/// field NAMES differ -- `cache_creation_input_tokens` vs
/// `cache_write_input_tokens` -- but the arithmetic is identical).
pub(crate) const fn sum_prompt_tokens(
    input_tokens: u32,
    cache_creation: u32,
    cache_read: u32,
) -> u32 {
    input_tokens
        .saturating_add(cache_creation)
        .saturating_add(cache_read)
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
    let prompt_tokens = sum_prompt_tokens(u.input_tokens, cache_creation, cache_read);
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
        server_tool_use: u.server_tool_use.clone(),
        extras: u.extras.clone(),
    }
}

/// Restore the client's original tool names on a normalized response.
///
/// The cloak forward pass normalized non-`mcp__` tool names to the `mcp__`
/// prefix on the wire; `map` carries the per-request reverse (renamed
/// upstream name -> original client name). This reverses tool_use names on
/// BOTH canonical surfaces produced by `walk_content_blocks`:
/// - the OpenAI-shape `choices[].message.tool_calls[].function.name`
/// - the Anthropic-shape `KnownContentPart::ToolUse` name in
///   `message.content` Parts
///
/// Only names present in `map` are reversed. A tool_use name with the
/// `mcp__` shape that is absent from `map` is left unchanged and bumps a
/// debug-level unmatched-reverse counter. Empty map = no-op.
pub(crate) fn reverse_tool_names(resp: &mut ChatResponse, map: &HashMap<String, String>) {
    if map.is_empty() {
        return;
    }
    for choice in &mut resp.choices {
        reverse_tool_calls(&mut choice.message.tool_calls, map);
        reverse_content_parts(&mut choice.message.content, map);
    }
}

/// Reverse names on the OpenAI-shape `tool_calls[].function.name`.
fn reverse_tool_calls(tool_calls: &mut Option<Vec<Value>>, map: &HashMap<String, String>) {
    let Some(calls) = tool_calls.as_mut() else {
        return;
    };
    for call in calls.iter_mut() {
        let Some(name) = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if let Some(original) = lookup_reverse(&name, map)
            && let Some(func_name) = call.pointer_mut("/function/name")
        {
            *func_name = Value::String(original);
        }
    }
}

/// Reverse names on the Anthropic-shape `ToolUse` parts in message content.
fn reverse_content_parts(content: &mut MessageContent, map: &HashMap<String, String>) {
    let MessageContent::Parts(parts) = content else {
        return;
    };
    for part in parts.iter_mut() {
        if let ContentPart::Known(KnownContentPart::ToolUse { name, .. }) = part
            && let Some(original) = lookup_reverse(name, map)
        {
            *name = original;
        }
    }
}

/// Look up the original client name for an upstream tool name. Returns
/// `Some(original)` when present in the map; `None` (with a debug count
/// for the unmatched `mcp__` case) otherwise.
fn lookup_reverse(name: &str, map: &HashMap<String, String>) -> Option<String> {
    if let Some(original) = map.get(name) {
        return Some(original.clone());
    }
    if name.starts_with("mcp__") {
        tracing::debug!(
            "anthropic response tool_use name has mcp__ shape but is absent from the \
             cloak reverse map; leaving unchanged",
        );
    }
    None
}

pub fn normalize(id: &str, raw: Value) -> Result<ChatResponse> {
    let resp: AnthropicResponse =
        serde_json::from_value(raw).map_err(|e| Error::normalize_response(id, e.to_string()))?;

    let (text, reasoning_details, tool_calls, parts) = walk_content_blocks(id, &resp.content)?;
    let usage = resp.usage.as_ref().map(translate_usage);
    let finish_reason = map_stop_reason(resp.stop_reason.as_deref());
    // Lift the upstream `stop_sequence` only when the upstream stopped
    // because of a matched sequence. Other stop reasons might emit a
    // stray field on some hosts; ignoring it here keeps the Anthropic
    // ingress from mis-rendering `stop_reason:"stop_sequence"`.
    let matched_stop_sequence = match resp.stop_reason.as_deref() {
        Some("stop_sequence") => resp.stop_sequence.clone(),
        _ => None,
    };

    let content = select_message_content(text, parts);
    let message = Message {
        refusal: None,
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
            logprobs: None,
            index: 0,
            message,
            finish_reason,
            matched_stop_sequence,
        }],
        usage,
        routectl_provider: None,
        extras: resp.extras,
        upstream_meta: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reverse_map() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(
            "mcp__linear_get_issue".to_string(),
            "mcp_linear_get_issue".to_string(),
        );
        m
    }

    #[test]
    fn reverse_tool_names_restores_openai_shape_function_name() {
        // Arrange
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__linear_get_issue",
                "input": {"k": "v"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");

        // Act
        reverse_tool_names(&mut resp, &reverse_map());

        // Assert: OpenAI-shape tool_calls function.name reversed.
        let name = resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(name, "mcp_linear_get_issue");
    }

    #[test]
    fn reverse_tool_names_restores_anthropic_part_name() {
        // Arrange
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__linear_get_issue",
                "input": {"k": "v"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");

        // Act
        reverse_tool_names(&mut resp, &reverse_map());

        // Assert: Anthropic-shape ToolUse Part name reversed.
        let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
            panic!("expected Parts content for a tool_use-only response");
        };
        let found = parts.iter().any(|p| {
            matches!(
                p,
                ContentPart::Known(KnownContentPart::ToolUse { name, .. })
                    if name == "mcp_linear_get_issue"
            )
        });
        assert!(
            found,
            "ToolUse part name must be reversed to client original"
        );
    }

    #[test]
    fn reverse_tool_names_empty_map_is_noop() {
        // Arrange
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__linear_get_issue",
                "input": {}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");

        // Act
        reverse_tool_names(&mut resp, &HashMap::new());

        // Assert: name unchanged.
        let name = resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(name, "mcp__linear_get_issue");
    }

    #[test]
    fn reverse_tool_names_unmatched_mcp_name_left_unchanged() {
        // Arrange: a mcp__ name absent from the map is left as-is.
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__other",
                "input": {}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");

        // Act
        reverse_tool_names(&mut resp, &reverse_map());

        // Assert
        let name = resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(name, "mcp__other");
    }

    // -- forward+reverse round trip ----------------------------------------

    #[test]
    fn round_trip_forward_cloak_then_reverse_response() {
        use super::super::cloak::{ClaudeCodeIdentity, CloakConfig, cloak_oauth_egress};
        use routectl_core::ChatRequest;

        // Arrange: an outgoing request with a single-underscore mcp_ tool
        // on both tools[] and a prior tool_use in history.
        let id = ClaudeCodeIdentity::mint(Some("sess"));
        let req = ChatRequest::default();
        let mut body = json!({
            "tools": [{"name": "mcp_linear_get_issue"}],
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1",
                    "name": "mcp_linear_get_issue", "input": {}
                }]
            }]
        });

        // Act 1: forward cloak.
        let result = cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());

        // Assert: outgoing body carries the doubled prefix on both surfaces.
        assert_eq!(body["tools"][0]["name"], "mcp__linear_get_issue");
        assert_eq!(
            body["messages"][0]["content"][0]["name"],
            "mcp__linear_get_issue"
        );

        // Act 2: a synthetic upstream response uses the upstream
        // (renamed) name; reverse it through normalize + reverse_tool_names.
        let raw = json!({
            "id": "msg_rt",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__linear_get_issue",
                "input": {"q": 1}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");
        reverse_tool_names(&mut resp, &result.tool_reverse);

        // Assert: BOTH surfaces reversed to the client's original name.
        let fn_name = resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(fn_name, "mcp_linear_get_issue");
        let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
            panic!("expected Parts content");
        };
        assert!(parts.iter().any(|p| matches!(
            p,
            ContentPart::Known(KnownContentPart::ToolUse { name, .. })
                if name == "mcp_linear_get_issue"
        )));
    }

    #[test]
    fn round_trip_bare_tool_name_forward_cloak_then_reverse_response() {
        use super::super::cloak::{ClaudeCodeIdentity, CloakConfig, cloak_oauth_egress};
        use routectl_core::ChatRequest;

        // Arrange: an outgoing request with a BARE snake_case tool name (the
        // hermes-style set) on both tools[] and a prior tool_use in history.
        let id = ClaudeCodeIdentity::mint(Some("sess"));
        let req = ChatRequest::default();
        let mut body = json!({
            "tools": [{"name": "read_file"}],
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1",
                    "name": "read_file", "input": {}
                }]
            }]
        });

        // Act 1: forward cloak. The bare name is prefixed with mcp__.
        let result = cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());
        assert_eq!(body["tools"][0]["name"], "mcp__read_file");
        assert_eq!(body["messages"][0]["content"][0]["name"], "mcp__read_file");
        assert_eq!(
            result
                .tool_reverse
                .get("mcp__read_file")
                .map(String::as_str),
            Some("read_file")
        );

        // Act 2: a synthetic upstream response uses the upstream (prefixed)
        // name; reverse it through normalize + reverse_tool_names.
        let raw = json!({
            "id": "msg_rt",
            "model": "claude-opus-4-8",
            "content": [{
                "type": "tool_use",
                "id": "t1",
                "name": "mcp__read_file",
                "input": {"path": "/x"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let mut resp = normalize("test", raw).expect("normalize");
        reverse_tool_names(&mut resp, &result.tool_reverse);

        // Assert: BOTH surfaces reversed to the client's bare original name.
        let fn_name = resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(fn_name, "read_file");
        let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
            panic!("expected Parts content");
        };
        assert!(parts.iter().any(|p| matches!(
            p,
            ContentPart::Known(KnownContentPart::ToolUse { name, .. })
                if name == "read_file"
        )));
    }

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

    #[test]
    fn context_management_round_trips_into_extras() {
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 1},
            "context_management": {"applied_edits": []}
        });
        let resp = normalize("test", raw).unwrap();
        assert_eq!(
            resp.extras.get("context_management"),
            Some(&json!({"applied_edits": []})),
            "context_management must survive normalize() into ChatResponse.extras"
        );
    }

    #[test]
    fn usage_service_tier_round_trips_into_extras() {
        let raw = json!({
            "id": "msg_01",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 1,
                "service_tier": "standard"
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.expect("usage present");
        assert_eq!(
            u.extras.get("service_tier"),
            Some(&json!("standard")),
            "service_tier must survive translate_usage into Usage.extras"
        );
    }

    #[test]
    fn usage_server_tool_use_populates_typed_field() {
        // Anthropic reports server-side tool invocation counts (e.g.
        // web_search) in `usage.server_tool_use`. The complete-path
        // normalizer must lift that object onto the typed canonical
        // field so the usage-accounting layer can store it.
        let raw = json!({
            "id": "msg_st",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 1,
                "server_tool_use": {"web_search_requests": 2}
            }
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.expect("usage present");
        assert_eq!(
            u.server_tool_use,
            Some(json!({"web_search_requests": 2})),
            "server_tool_use must populate the typed canonical field"
        );
        // Forward-compat invariant: server_tool_use is lifted OUT of
        // extras into the typed slot, not duplicated into both.
        assert!(
            u.extras.get("server_tool_use").is_none(),
            "server_tool_use must not also land in extras"
        );
    }

    #[test]
    fn usage_without_server_tool_use_leaves_field_none() {
        let raw = json!({
            "id": "msg_no_stu",
            "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        });
        let resp = normalize("test", raw).unwrap();
        let u = resp.usage.expect("usage present");
        assert_eq!(u.server_tool_use, None);
    }
}
