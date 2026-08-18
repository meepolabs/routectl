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
use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatResponse, Choice, ContentPart, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Result, Role, Usage, sanitize_for_log,
};

use crate::anthropic_api::response::{map_stop_reason, sum_prompt_tokens};

use super::response_types::{
    ConverseCacheDetail, ConverseResponse, ConverseResponseContentBlock, ConverseUsage,
};
use super::tools::HISTORY_COMPAT_TOOL_NAME;

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

    // Converse responses always carry role "assistant"; no other value
    // is defined on the response side.
    let role = Role::Assistant;

    let content = select_message_content(text, parts);
    let message = Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details,
        name: None,
        tool_call_id: None,
        tool_calls,
    };

    let matched_stop_sequence = lift_stop_sequence(
        provider_id,
        resp.stop_reason.as_deref(),
        resp.additional_model_response_fields.as_ref(),
    );

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
            logprobs: None,
            index: 0,
            message,
            finish_reason,
            matched_stop_sequence,
        }],
        usage,
        routectl_provider: None,
        // Converse top-level extras beyond output/stopReason/usage/
        // metrics/additionalModelResponseFields are not forwarded;
        // routectl pulls the meaningful fields explicitly via the
        // typed ConverseResponse fields. The Anthropic ingress's
        // response wire-render only needs the fields surfaced on
        // the canonical ChatResponse.
        extras: Default::default(),
        upstream_meta: None,
    })
}

/// Shared helper for the streaming and non-streaming lift sites. Both
/// paths gate on `stop_reason == "stop_sequence"` and read a flat
/// string under the camelCase `stop_sequence` key on the
/// additionalModelResponseFields bag. A debug-level event fires when
/// the gate is satisfied but the lift comes back empty so operators
/// can spot schema drift on Converse hosts without burning warn-level
/// noise on the happy path.
pub(super) fn lift_stop_sequence(
    provider_id: &str,
    stop_reason: Option<&str>,
    additional_fields: Option<&Value>,
) -> Option<String> {
    if stop_reason != Some("stop_sequence") {
        return None;
    }
    let lifted = additional_fields
        .and_then(|v| v.get("stop_sequence"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if lifted.is_none() {
        debug!(
            provider = provider_id,
            additional_model_response_fields = ?additional_fields,
            "converse: stop_reason=stop_sequence but additionalModelResponseFields.stop_sequence missing or non-string"
        );
    }
    lifted
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
    // `translate` calls this once per upstream response and the walk is
    // flat (no recursion into nested blocks), so a local flag is exactly
    // once-per-response for the reserved-dummy diagnostic below.
    let mut history_compat_selection_warned = false;

    for block in blocks {
        match block {
            ConverseResponseContentBlock::Text { text } => {
                text_parts.push(text.clone());
                parts.push(ContentPart::Known(KnownContentPart::Text {
                    text: text.clone(),
                    citations: None,
                    cache_control: None,
                }));
            }
            ConverseResponseContentBlock::ToolUse { tool_use } => {
                // DIAGNOSTIC: remove once a live Bedrock probe confirms
                // whether the model ever selects the reserved dummy tool.
                if tool_use.name == HISTORY_COMPAT_TOOL_NAME && !history_compat_selection_warned {
                    history_compat_selection_warned = true;
                    warn!(
                        provider = provider_id,
                        reserved_tool_name = HISTORY_COMPAT_TOOL_NAME,
                        "converse: model selected the reserved history-compat dummy tool"
                    );
                }
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
                debug!(
                    provider = provider_id,
                    block_type = %sanitize_for_log(&tag),
                    "unrecognized converse content block preserved as ContentPart::Other on canonical response"
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
    let (tag, inner) = obj.iter().next().map_or_else(
        || ("unknown".to_string(), Value::Null),
        |(k, v)| (k.clone(), v.clone()),
    );
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

/// Translate Converse `usage` into the canonical `Usage`. Mirrors
/// `anthropic_api::response::translate_usage` so an OpenAI client
/// reading `prompt_tokens` sees the cumulative context size
/// (raw input plus cache_creation plus cache_read), not just the
/// new-tokens count AWS reports in `inputTokens`.
fn translate_usage(u: &ConverseUsage) -> Usage {
    let cache_write = u.cache_write_input_tokens.unwrap_or(0);
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    let prompt_tokens = sum_prompt_tokens(u.input_tokens, cache_write, cache_read);
    let completion_tokens = u.output_tokens;
    let mut usage = Usage {
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
        server_tool_use: None,
        extras: Default::default(),
    };
    // Guards an explicit `totalTokens: 0` sent alongside nonzero components.
    usage.derive_total_if_absent();
    usage
}

/// Flatten Converse's `cacheDetails: [{inputTokens, ttl}]` array into
/// the canonical per-TTL object. Multiple entries with the same `ttl`
/// (shouldn't happen on the wire today but is theoretically possible)
/// sum together so no token counts get silently lost.
pub fn translate_cache_details(details: &[ConverseCacheDetail]) -> CacheCreation {
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

    include!("response_history_compat_tests.rs");

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
    fn unknown_block_type_preserved_as_content_part_other_with_debug_log() {
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
                        assert_eq!(extras.get("x").and_then(serde_json::Value::as_i64), Some(1));
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

    #[test]
    fn matched_stop_sequence_lifted_when_stop_reason_is_stop_sequence() {
        // Arrange: AWS echoes the matched sequence back via
        // additionalModelResponseFields when the request opted into
        // /stop_sequence. The canonical ChatResponse must surface it
        // identically to the Anthropic-API and Bedrock-Invoke paths.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello STOP"}]
                }
            },
            "stopReason": "stop_sequence",
            "additionalModelResponseFields": {"stop_sequence": "STOP"}
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            resp.choices[0].matched_stop_sequence.as_deref(),
            Some("STOP")
        );
    }

    #[test]
    fn matched_stop_sequence_gated_off_for_non_stop_sequence_reason() {
        // Arrange: a stray `stop_sequence` value paired with a
        // different stop_reason must NOT be lifted -- mirrors the
        // anthropic_api egress so canonical `matched_stop_sequence`
        // only fires when AWS actually stopped on a matched sequence.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "all done"}]
                }
            },
            "stopReason": "end_turn",
            "additionalModelResponseFields": {"stop_sequence": "STOP"}
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert!(resp.choices[0].matched_stop_sequence.is_none());
    }

    #[test]
    fn matched_stop_sequence_absent_field_yields_none_no_panic() {
        // Arrange: AWS reported stop_reason=stop_sequence but did not
        // surface the field (provider quirk or schema drift). The lift
        // must fall through to None without panicking; a debug-level
        // diagnostic fires for operator visibility but is not asserted
        // here (the project doesn't wire tracing_test into unit tests).
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello"}]
                }
            },
            "stopReason": "stop_sequence"
        });

        // Act
        let resp = translate("test", &raw).unwrap();

        // Assert
        assert!(resp.choices[0].matched_stop_sequence.is_none());
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn matched_stop_sequence_parity_with_bedrock_invoke_path() {
        // Arrange: Bedrock-Invoke delegates the response normalization
        // to `anthropic_api::response::normalize`, which lifts the
        // native top-level `stop_sequence` field. Bedrock-Converse
        // lifts the same value out of
        // `additionalModelResponseFields["stop_sequence"]`. Both must
        // surface the identical canonical `matched_stop_sequence`
        // string when the upstream stops on the same sequence with
        // the same prompt + stop list.
        let invoke_raw = json!({
            "id": "msg_invoke_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "content": [{"type": "text", "text": "hello STOP"}],
            "stop_reason": "stop_sequence",
            "stop_sequence": "STOP",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let converse_raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello STOP"}]
                }
            },
            "stopReason": "stop_sequence",
            "additionalModelResponseFields": {"stop_sequence": "STOP"},
            "usage": {"inputTokens": 5, "outputTokens": 3, "totalTokens": 8}
        });

        // Act
        let invoke_resp =
            crate::anthropic_api::response::normalize("bedrock-invoke", invoke_raw).unwrap();
        let converse_resp = translate("bedrock-converse", &converse_raw).unwrap();

        // Assert: the canonical matched_stop_sequence value matches
        // across both Bedrock egress paths.
        assert_eq!(
            invoke_resp.choices[0].matched_stop_sequence,
            converse_resp.choices[0].matched_stop_sequence,
        );
        assert_eq!(
            converse_resp.choices[0].matched_stop_sequence.as_deref(),
            Some("STOP"),
        );
        // And the finish_reason mapping agrees too.
        assert_eq!(
            invoke_resp.choices[0].finish_reason,
            converse_resp.choices[0].finish_reason,
        );
    }
}
