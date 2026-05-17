//! Bedrock Converse response body -> canonical `ChatResponse`.
//!
//! Walks `output.message.content[]` blocks, accumulating flat text,
//! reasoning_details, and tool_calls in the same shape the openai-compat
//! and Anthropic-API egresses produce. The `stopReason` -> `finish_reason`
//! mapping reuses the Anthropic canonical mapping (the value sets overlap
//! exactly for the OpenAI-compatible shapes; Converse-specific values
//! like `guardrail_intervened` pass through verbatim and clients see the
//! literal AWS string -- mirroring the `pause_turn` / `refusal` /
//! `model_context_window_exceeded` passthrough described in CLAUDE.md
//! gotchas).
//!
//! Usage translation collapses Converse's `cacheDetails: [{tokens, ttl}]`
//! into the canonical `cache_creation` per-TTL object so downstream
//! OpenAI-SSE clients see identical totals regardless of whether the
//! upstream was Bedrock-Invoke or Bedrock-Converse.

use chrono::Utc;
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatResponse, Choice, ContentPart, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Result, Role, Usage,
};

use super::response_types::{
    ConverseCacheDetail, ConverseResponse, ConverseResponseContentBlock, ConverseUsage,
};

/// Format tag mirroring the Anthropic-API egress so multi-turn callers
/// echoing reasoning_details back don't see a different format string
/// when the same model is fronted by Converse instead of Invoke.
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Translate a raw Converse response body into the canonical
/// `ChatResponse`.
pub fn translate(provider_id: &str, body: &Value) -> Result<ChatResponse> {
    let resp: ConverseResponse = serde_json::from_value(body.clone())
        .map_err(|e| Error::normalize_response(provider_id, e.to_string()))?;

    let (text, reasoning_details, tool_calls, parts) =
        walk_content_blocks(provider_id, &resp.output.message.content)?;
    let finish_reason = map_stop_reason(resp.stop_reason.as_deref());
    let usage = resp.usage.as_ref().map(translate_usage);

    let role = if resp.output.message.role == "assistant" {
        Role::Assistant
    } else {
        // AWS only emits "assistant" today on the response side; if a
        // future model returns something else, surface the literal so
        // we don't lie to the client. Falling back to Assistant
        // preserves OpenAI-compat semantics on ChatResponse.
        Role::Assistant
    };

    let content = select_message_content(text, parts);
    let message = Message {
        role,
        content,
        reasoning: None,
        reasoning_details,
        name: None,
        tool_call_id: None,
        tool_calls,
    };

    Ok(ChatResponse {
        // Bedrock Converse responses don't carry an upstream message id.
        // Synthesize one so downstream OpenAI-SSE clients see the same
        // shape they would from the Anthropic-API path. Time-sortable
        // (now_v7) for log correlation.
        id: Uuid::now_v7().to_string(),
        // Converse responses don't echo the model id; routectl's
        // canonical `ChatResponse.model` is informational and clients
        // typically read the alias from the request, so leaving this
        // empty matches the Anthropic-API behavior pre-fix when
        // upstream omitted model.
        model: String::new(),
        created: Utc::now().timestamp(),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
        }],
        usage,
        routectl_provider: None,
        // Forward-compat: carry AWS-Converse top-level extras
        // verbatim. The Anthropic egress's wire-render iterates
        // these out, so any new Converse top-level field flows
        // through to the client without a routectl release.
        extras: resp.extras,
    })
}

/// Walk Converse content blocks. Mirrors
/// `anthropic_api::response::walk_content_blocks` shape: returns flat
/// text, reasoning_details, OpenAI-shape tool_calls, and a `parts`
/// vector preserving every block in arrival order. Reasoning blocks
/// surface on `reasoning_details` only (not in `parts`); the
/// caller-side `select_message_content` collapses to flat Text when the
/// only Parts entries are Text-typed and emits Parts otherwise so
/// multimodal/forward-compat content survives end-to-end.
#[allow(clippy::type_complexity)] // multi-tuple return matches the wire walk; alias would obscure intent
fn walk_content_blocks(
    provider_id: &str,
    blocks: &[ConverseResponseContentBlock],
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
            ConverseResponseContentBlock::Text { text } => {
                text_parts.push(text.clone());
                parts.push(ContentPart::Known(KnownContentPart::Text {
                    text: text.clone(),
                    cache_control: None,
                }));
            }
            ConverseResponseContentBlock::ToolUse { tool_use } => {
                let arguments = serde_json::to_string(&tool_use.input)
                    .map_err(|e| Error::normalize_response(provider_id, e.to_string()))?;
                tool_calls.push(json!({
                    "id": tool_use.tool_use_id,
                    "type": "function",
                    "function": {"name": tool_use.name, "arguments": arguments}
                }));
                parts.push(ContentPart::Known(KnownContentPart::ToolUse {
                    id: tool_use.tool_use_id.clone(),
                    name: tool_use.name.clone(),
                    input: tool_use.input.clone(),
                    cache_control: None,
                }));
            }
            ConverseResponseContentBlock::ReasoningContent { reasoning_content } => {
                if let Some(rt) = reasoning_content.reasoning_text.as_ref() {
                    reasoning_details.push(ReasoningDetail {
                        kind: ReasoningDetailKind::Text,
                        id: Some(Uuid::new_v4().to_string()),
                        format: Some(ANTHROPIC_FORMAT.to_string()),
                        index: Some(detail_index),
                        payload: json!({"text": rt.text, "signature": rt.signature}),
                    });
                    detail_index += 1;
                }
                if let Some(redacted) = reasoning_content.redacted_content.as_ref() {
                    reasoning_details.push(ReasoningDetail {
                        kind: ReasoningDetailKind::Encrypted,
                        id: Some(Uuid::new_v4().to_string()),
                        format: Some(ANTHROPIC_FORMAT.to_string()),
                        index: Some(detail_index),
                        payload: json!({"data": redacted}),
                    });
                    detail_index += 1;
                }
            }
            ConverseResponseContentBlock::Other(v) => {
                let (tag, extras) = extract_other_tag_and_extras(v);
                warn!(
                    provider = provider_id,
                    block_type = %tag,
                    "unknown converse content block dropped from flat-text output"
                );
                parts.push(ContentPart::Other {
                    type_tag: tag,
                    cache_control: None,
                    extras,
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

/// Pull the single-key tag + inner-fields out of an unknown Converse
/// content block. AWS unions are single-key objects (`{video: {...}}`,
/// `{citationsContent: {...}}`, ...); the Other variant catches whichever
/// key didn't match a typed arm. We extract the inner object as the
/// `extras` map so a forward-compat ContentPart::Other carries the
/// original payload through the canonical schema -- the egress sees the
/// same shape the upstream produced.
fn extract_other_tag_and_extras(v: &Value) -> (String, serde_json::Map<String, Value>) {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return ("unknown".to_string(), serde_json::Map::new()),
    };
    let (tag, inner) = obj
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap_or_else(|| ("unknown".to_string(), Value::Null));
    let extras = match inner {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    (tag, extras)
}

/// Choose between `MessageContent::Text(joined)` and
/// `MessageContent::Parts(parts)`. Mirrors the Anthropic-API egress's
/// helper: collapse to Text only when every emitted Part is a Text
/// entry; otherwise preserve every block (multimodal, forward-compat,
/// reasoning markers) by emitting Parts. Reasoning blocks aren't in
/// parts -- they ride on reasoning_details -- so a text + reasoning
/// response still collapses to Text here.
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

/// Map a Converse stopReason to canonical OpenAI-shape finish_reason.
/// Overlap with the Anthropic value set is exact for the four
/// OpenAI-mappable values (`end_turn`, `stop_sequence`, `max_tokens`,
/// `tool_use`); Converse-specific values (`guardrail_intervened`,
/// `content_filtered`, `malformed_model_output`, `malformed_tool_use`,
/// `model_context_window_exceeded`) pass through verbatim per the
/// CLAUDE.md "stop_reason round-trip is lossy for Anthropic-only values"
/// gotcha -- preserving information at the canonical layer rather than
/// clobbering to "stop".
pub(crate) fn map_stop_reason(reason: Option<&str>) -> Option<String> {
    let r = reason?;
    let mapped = match r {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        other => other,
    };
    Some(mapped.to_string())
}

/// Translate Converse `usage` into the canonical `Usage`. Mirrors
/// `anthropic_api::response::translate_usage` so an OpenAI client
/// reading `prompt_tokens` sees the cumulative context size
/// (raw input plus cache_creation plus cache_read), not just the
/// new-tokens count AWS reports in `inputTokens`.
fn translate_usage(u: &ConverseUsage) -> Usage {
    let cache_write = u.cache_write_input_tokens.unwrap_or(0);
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    let prompt_tokens = u
        .input_tokens
        .saturating_add(cache_write)
        .saturating_add(cache_read);
    let completion_tokens = u.output_tokens;
    Usage {
        prompt_tokens,
        completion_tokens,
        // Trust AWS's `totalTokens` if it diverges from the sum (it
        // shouldn't, but per AWS docs the field is computed
        // server-side and is authoritative).
        total_tokens: u
            .total_tokens
            .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens)),
        reasoning_tokens: None,
        cache_creation_input_tokens: u.cache_write_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation: u.cache_details.as_deref().map(translate_cache_details),
        extras: Default::default(),
    }
}

/// Flatten Converse's `cacheDetails: [{inputTokens, ttl}]` array into
/// the canonical per-TTL object. Multiple entries with the same `ttl`
/// (shouldn't happen on the wire today but is theoretically possible)
/// sum together so no token counts get silently lost.
fn translate_cache_details(details: &[ConverseCacheDetail]) -> CacheCreation {
    let mut five_min: Option<u32> = None;
    let mut one_hour: Option<u32> = None;
    for d in details {
        match d.ttl.as_str() {
            "5m" => {
                let cur = five_min.unwrap_or(0);
                five_min = Some(cur.saturating_add(d.input_tokens));
            }
            "1h" => {
                let cur = one_hour.unwrap_or(0);
                one_hour = Some(cur.saturating_add(d.input_tokens));
            }
            // Forward compat: an unknown TTL bucket means a future AWS
            // value (e.g. "24h"). Drop it from the per-TTL object
            // rather than coercing into the wrong bucket; it still
            // contributed to the cache_creation_input_tokens total
            // upstream.
            _ => {}
        }
    }
    CacheCreation {
        ephemeral_5m_input_tokens: five_min,
        ephemeral_1h_input_tokens: one_hour,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_only_response_translates_to_canonical_text_message() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello world"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {"inputTokens": 10, "outputTokens": 20, "totalTokens": 30},
            "metrics": {"latencyMs": 1234}
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        if let MessageContent::Text(t) = &resp.choices[0].message.content {
            assert_eq!(t, "hello world");
        } else {
            panic!("expected text content");
        }
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 20);
        assert_eq!(u.total_tokens, 30);
    }

    #[test]
    fn tool_use_block_translates_to_openai_tool_call() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "let me check"},
                        {"toolUse": {
                            "toolUseId": "tu_42",
                            "name": "get_weather",
                            "input": {"location": "Tokyo"}
                        }}
                    ]
                }
            },
            "stopReason": "tool_use"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        let tcs = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "tu_42");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "get_weather");
        // Arguments is a JSON-stringified object per OpenAI spec.
        let args = tcs[0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed, json!({"location": "Tokyo"}));
        // Mixed text + tool_use response emits Parts so the tool_use
        // block survives end-to-end. The text block also survives in
        // parts.
        match &resp.choices[0].message.content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                        assert_eq!(text, "let me check");
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
                match &parts[1] {
                    ContentPart::Known(KnownContentPart::ToolUse { id, name, .. }) => {
                        assert_eq!(id, "tu_42");
                        assert_eq!(name, "get_weather");
                    }
                    other => panic!("expected ToolUse, got {other:?}"),
                }
            }
            other => panic!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_text_block_emits_thinking_detail() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"reasoningContent": {
                            "reasoningText": {"text": "step 1", "signature": "sig123"}
                        }},
                        {"text": "answer"}
                    ]
                }
            },
            "stopReason": "end_turn"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
        assert_eq!(details[0].format.as_deref(), Some(ANTHROPIC_FORMAT));
        assert_eq!(details[0].payload["text"], "step 1");
        assert_eq!(details[0].payload["signature"], "sig123");
        if let MessageContent::Text(t) = &resp.choices[0].message.content {
            assert_eq!(t, "answer");
        }
    }

    #[test]
    fn redacted_reasoning_emits_encrypted_detail() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"reasoningContent": {
                            "redactedContent": "AAECAwQF"
                        }}
                    ]
                }
            },
            "stopReason": "end_turn"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert!(matches!(details[0].kind, ReasoningDetailKind::Encrypted));
        assert_eq!(details[0].payload["data"], "AAECAwQF");
    }

    #[test]
    fn unknown_block_type_is_dropped_with_warn_not_error() {
        // Arrange: a future AWS block type. Forward compat means we
        // preserve it as ContentPart::Other in Parts emission rather
        // than failing or silently dropping. The tag and inner fields
        // round-trip so an egress that knows the type can re-emit it.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"futureBlock": {"x": 1}},
                        {"text": "after"}
                    ]
                }
            },
            "stopReason": "end_turn"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        match &resp.choices[0].message.content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    ContentPart::Other {
                        type_tag, extras, ..
                    } => {
                        assert_eq!(type_tag, "futureBlock");
                        assert_eq!(extras.get("x").and_then(|v| v.as_i64()), Some(1));
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
        // OpenAI-ingress wire stability: pure-text response stays as
        // flat MessageContent::Text so existing OpenAI clients see the
        // same shape they always have.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "hello "},
                        {"text": "world"}
                    ]
                }
            },
            "stopReason": "end_turn"
        });

        let resp = translate("test", &raw).unwrap();

        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_plus_reasoning_response_still_collapses_to_text() {
        // Reasoning blocks ride on reasoning_details, not on parts.
        // A response with text + reasoning still has only Text in
        // parts, so the content collapses to MessageContent::Text and
        // signature/data is preserved on reasoning_details for replay.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"reasoningContent": {
                            "reasoningText": {"text": "step 1", "signature": "sig123"}
                        }},
                        {"text": "answer"}
                    ]
                }
            },
            "stopReason": "end_turn"
        });

        let resp = translate("test", &raw).unwrap();

        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "answer"),
            other => panic!("expected Text, got {other:?}"),
        }
        let details = &resp.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].payload["text"], "step 1");
        assert_eq!(details[0].payload["signature"], "sig123");
    }

    #[test]
    fn prompt_tokens_sums_input_plus_cache_read_plus_cache_write() {
        // Arrange: AWS reports `inputTokens` as ONLY new tokens (per
        // the Anthropic-on-Bedrock convention). Canonical
        // `prompt_tokens` must be the cumulative size so OpenAI clients
        // see the full prompt.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "ok"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 50,
                "outputTokens": 10,
                "cacheReadInputTokens": 200,
                "cacheWriteInputTokens": 100
            }
        });

        // Act
        let resp = translate("test", &raw).unwrap();
        let u = resp.usage.unwrap();

        // Assert: 50 + 100 + 200 = 350.
        assert_eq!(u.prompt_tokens, 350);
        assert_eq!(u.cache_creation_input_tokens, Some(100));
        assert_eq!(u.cache_read_input_tokens, Some(200));
    }

    #[test]
    fn cache_details_translate_to_per_ttl_breakdown() {
        // Arrange
        let raw = json!({
            "output": {"message": {"role": "assistant", "content": [{"text":"ok"}]}},
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 0,
                "outputTokens": 5,
                "cacheDetails": [
                    {"inputTokens": 75, "ttl": "5m"},
                    {"inputTokens": 100, "ttl": "1h"}
                ]
            }
        });

        // Act
        let resp = translate("test", &raw).unwrap();
        let cc = resp.usage.unwrap().cache_creation.unwrap();

        // Assert
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(75));
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(100));
    }

    #[test]
    fn unknown_stop_reason_passes_through_verbatim() {
        // Arrange: Converse-only value (no OpenAI overlap). Must
        // pass through so callers can dispatch on it; clobbering to
        // "stop" would lose information.
        let raw = json!({
            "output": {"message": {"role": "assistant", "content": [{"text":"x"}]}},
            "stopReason": "guardrail_intervened"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("guardrail_intervened")
        );
    }

    #[test]
    fn missing_usage_is_tolerated() {
        // Arrange: minimal AWS response without usage/metrics.
        let raw = json!({
            "output": {"message": {"role": "assistant", "content": [{"text":"hi"}]}}
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert!(resp.usage.is_none());
        assert!(resp.choices[0].finish_reason.is_none());
    }
}
