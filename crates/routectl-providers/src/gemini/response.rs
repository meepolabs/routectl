//! Gemini `generateContent` response -> canonical `ChatResponse` translation.
//!
//! Finish-reason mapping (Gemini -> canonical):
//!   STOP                     -> "stop"
//!   MAX_TOKENS               -> "length"
//!   SAFETY, RECITATION       -> "content_filter"
//!   MALFORMED_FUNCTION_CALL  -> "error"
//!   any functionCall parts   -> "tool_calls" (overrides the above)
//!   anything else            -> "stop" (safe default)

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use routectl_core::{
    ChatResponse, Choice, Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Result,
    Role, Usage,
};

use super::types::{GenerateContentResponse, ResponsePart, UsageMetadata};
use super::GEMINI_FORMAT;

/// Translate a deserialized Gemini response into canonical `ChatResponse`.
pub(crate) fn translate(provider_id: &str, resp: GenerateContentResponse) -> Result<ChatResponse> {
    let candidate = resp.candidates.into_iter().next();

    let (text, tool_calls, reasoning_details, finish_reason) = match candidate {
        None => (String::new(), None, Vec::new(), Some("stop".to_string())),
        Some(c) => {
            let parts = c.content.map(|cont| cont.parts).unwrap_or_default();
            let walked = walk_parts(provider_id, &parts)?;
            let has_tool_calls = walked.tool_calls.is_some();
            let finish = map_finish_reason(c.finish_reason.as_deref(), has_tool_calls);
            (
                walked.text,
                walked.tool_calls,
                walked.reasoning_details,
                finish,
            )
        }
    };

    let content = if text.is_empty() {
        MessageContent::Null
    } else {
        MessageContent::Text(text)
    };

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

    let usage = resp.usage_metadata.as_ref().map(translate_usage);

    let id = resp
        .response_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    let model = resp.model_version.unwrap_or_default();

    Ok(ChatResponse {
        id,
        model,
        created: Utc::now().timestamp(),
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message,
            finish_reason,
            matched_stop_sequence: None,
        }],
        usage,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    })
}

/// Result of walking a candidate's `parts[]`.
struct WalkedParts {
    /// Concatenated visible (non-thought) text.
    text: String,
    /// OpenAI-shape `tool_calls` when any functionCall parts were seen.
    tool_calls: Option<Vec<Value>>,
    /// Canonical reasoning details collected from thought parts.
    reasoning_details: Vec<ReasoningDetail>,
}

/// Walk the candidate's `parts[]`, separating visible text, tool calls,
/// and thought (reasoning) parts. Thought parts carry their text and
/// `thoughtSignature` into a canonical `ReasoningDetail` tagged with
/// `GEMINI_FORMAT` so a downstream ingress can replay them.
fn walk_parts(provider_id: &str, parts: &[ResponsePart]) -> Result<WalkedParts> {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning_details: Vec<ReasoningDetail> = Vec::new();

    for part in parts {
        let is_thought = part.thought == Some(true);
        if let Some(t) = &part.text {
            if is_thought {
                reasoning_details.push(thought_detail(
                    t,
                    part.thought_signature.as_deref(),
                    reasoning_details.len() as u32,
                ));
            } else {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if let Some(fc) = &part.function_call {
            // Synthesize an OpenAI-shape tool_call with a generated id.
            let id = format!("call_{}", Uuid::now_v7().simple());
            let args_str = match serde_json::to_string(&fc.args) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        provider = %provider_id,
                        fn_name = %fc.name,
                        error = %e,
                        "gemini: could not serialize function_call args"
                    );
                    "{}".to_string()
                }
            };
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": fc.name,
                    "arguments": args_str
                }
            }));
        }
    }

    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    Ok(WalkedParts {
        text,
        tool_calls,
        reasoning_details,
    })
}

/// Build a canonical reasoning detail from a Gemini thought part. The
/// `thoughtSignature` is preserved in the payload so it can be replayed
/// verbatim on a follow-up turn.
fn thought_detail(text: &str, signature: Option<&str>, index: u32) -> ReasoningDetail {
    let mut payload = json!({ "text": text });
    if let Some(sig) = signature {
        payload["thought_signature"] = json!(sig);
    }
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(GEMINI_FORMAT.to_string()),
        index: Some(index),
        payload,
    }
}

/// Map Gemini's `finishReason` to the canonical finish_reason string.
pub(super) fn map_finish_reason(
    gemini_reason: Option<&str>,
    has_tool_calls: bool,
) -> Option<String> {
    if has_tool_calls {
        return Some("tool_calls".to_string());
    }
    let reason = match gemini_reason {
        None | Some("") => return Some("stop".to_string()),
        Some(r) => r,
    };
    Some(
        match reason {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            "SAFETY" | "RECITATION" => "content_filter",
            "MALFORMED_FUNCTION_CALL" => "error",
            _ => "stop",
        }
        .to_string(),
    )
}

/// Map Gemini `usageMetadata` to canonical `Usage`.
fn translate_usage(meta: &UsageMetadata) -> Usage {
    Usage {
        prompt_tokens: meta.prompt_token_count,
        completion_tokens: meta.candidates_token_count,
        total_tokens: meta.total_token_count,
        reasoning_tokens: if meta.thoughts_token_count > 0 {
            Some(meta.thoughts_token_count)
        } else {
            None
        },
        cache_creation_input_tokens: None,
        cache_read_input_tokens: if meta.cached_content_token_count > 0 {
            Some(meta.cached_content_token_count)
        } else {
            None
        },
        cache_creation: None,
        server_tool_use: None,
        extras: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::types::{
        Candidate, GenerateContentResponse, ResponseContent, ResponseFunctionCall, ResponsePart,
        UsageMetadata,
    };

    fn text_response(text: &str, finish: &str) -> GenerateContentResponse {
        GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(ResponseContent {
                    parts: vec![ResponsePart {
                        text: Some(text.to_string()),
                        function_call: None,
                        ..Default::default()
                    }],
                    role: Some("model".to_string()),
                }),
                finish_reason: Some(finish.to_string()),
                index: 0,
            }],
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                total_token_count: 15,
                ..Default::default()
            }),
            model_version: Some("gemini-2.5-pro-001".to_string()),
            response_id: Some("resp-123".to_string()),
        }
    }

    #[test]
    fn text_response_translates_to_chat_response() {
        let resp = text_response("Hello!", "STOP");
        let chat = translate("gemini:test", resp).expect("translate ok");

        assert_eq!(chat.id, "resp-123");
        assert_eq!(chat.model, "gemini-2.5-pro-001");
        match &chat.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "Hello!"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn function_call_part_becomes_tool_calls() {
        let resp = GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(ResponseContent {
                    parts: vec![ResponsePart {
                        text: None,
                        function_call: Some(ResponseFunctionCall {
                            name: "get_weather".into(),
                            args: serde_json::json!({"city": "Tokyo"}),
                        }),
                        ..Default::default()
                    }],
                    role: Some("model".to_string()),
                }),
                finish_reason: Some("STOP".to_string()),
                index: 0,
            }],
            usage_metadata: None,
            model_version: None,
            response_id: None,
        };
        let chat = translate("gemini:test", resp).expect("translate ok");

        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        let tool_calls = chat.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls present");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        let args: Value =
            serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "Tokyo");
    }

    #[test]
    fn finish_reason_stop() {
        assert_eq!(
            map_finish_reason(Some("STOP"), false).as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn finish_reason_max_tokens() {
        assert_eq!(
            map_finish_reason(Some("MAX_TOKENS"), false).as_deref(),
            Some("length")
        );
    }

    #[test]
    fn finish_reason_safety() {
        assert_eq!(
            map_finish_reason(Some("SAFETY"), false).as_deref(),
            Some("content_filter")
        );
    }

    #[test]
    fn finish_reason_recitation() {
        assert_eq!(
            map_finish_reason(Some("RECITATION"), false).as_deref(),
            Some("content_filter")
        );
    }

    #[test]
    fn finish_reason_malformed_function_call() {
        assert_eq!(
            map_finish_reason(Some("MALFORMED_FUNCTION_CALL"), false).as_deref(),
            Some("error")
        );
    }

    #[test]
    fn finish_reason_tool_calls_wins() {
        assert_eq!(
            map_finish_reason(Some("STOP"), true).as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn usage_metadata_maps_to_canonical_usage() {
        let meta = UsageMetadata {
            prompt_token_count: 100,
            candidates_token_count: 50,
            total_token_count: 150,
            cached_content_token_count: 20,
            thoughts_token_count: 30,
            tool_use_prompt_token_count: 0,
        };
        let usage = translate_usage(&meta);

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cache_read_input_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(30));
        assert!(usage.cache_creation_input_tokens.is_none());
    }

    #[test]
    fn zero_cached_tokens_omitted() {
        let meta = UsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            total_token_count: 15,
            cached_content_token_count: 0,
            thoughts_token_count: 0,
            tool_use_prompt_token_count: 0,
        };
        let usage = translate_usage(&meta);
        assert!(usage.cache_read_input_tokens.is_none());
        assert!(usage.reasoning_tokens.is_none());
    }

    #[test]
    fn thought_parts_become_reasoning_details_not_content() {
        let resp = GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(ResponseContent {
                    parts: vec![
                        ResponsePart {
                            text: Some("let me think".into()),
                            thought: Some(true),
                            thought_signature: Some("sig-42".into()),
                            ..Default::default()
                        },
                        ResponsePart {
                            text: Some("the answer".into()),
                            ..Default::default()
                        },
                    ],
                    role: Some("model".into()),
                }),
                finish_reason: Some("STOP".into()),
                index: 0,
            }],
            usage_metadata: None,
            model_version: None,
            response_id: Some("r".into()),
        };
        let chat = translate("gemini:test", resp).expect("translate ok");

        // Visible content is only the non-thought text.
        match &chat.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "the answer"),
            other => panic!("expected Text, got {other:?}"),
        }
        // The thought part surfaces as a reasoning_detail carrying the signature.
        let details = &chat.choices[0].message.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some(super::GEMINI_FORMAT));
        assert_eq!(details[0].payload["text"], "let me think");
        assert_eq!(details[0].payload["thought_signature"], "sig-42");
    }
}
