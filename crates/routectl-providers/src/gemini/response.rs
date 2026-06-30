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

use routectl_core::{ChatResponse, Choice, Message, MessageContent, Result, Role, Usage};

use super::types::{GenerateContentResponse, ResponsePart, UsageMetadata};

/// Translate a deserialized Gemini response into canonical `ChatResponse`.
pub(crate) fn translate(provider_id: &str, resp: GenerateContentResponse) -> Result<ChatResponse> {
    let candidate = resp.candidates.into_iter().next();

    let (text, tool_calls, finish_reason) = match candidate {
        None => (String::new(), None, Some("stop".to_string())),
        Some(c) => {
            let parts = c.content.map(|cont| cont.parts).unwrap_or_default();
            let (text, tool_calls) = walk_parts(provider_id, &parts)?;
            let has_tool_calls = tool_calls.is_some();
            let finish = map_finish_reason(c.finish_reason.as_deref(), has_tool_calls);
            (text, tool_calls, finish)
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
        reasoning_details: Vec::new(),
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

/// Walk the candidate's `parts[]`. Returns:
///   - concatenated text
///   - OpenAI-shape `tool_calls` Vec if any functionCall parts were seen
fn walk_parts(provider_id: &str, parts: &[ResponsePart]) -> Result<(String, Option<Vec<Value>>)> {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for part in parts {
        if let Some(t) = &part.text {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
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
        // TODO(slice-2): check part.thought to collect reasoning tokens
    }

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    Ok((text, tool_calls_opt))
}

/// Map Gemini's `finishReason` to the canonical finish_reason string.
fn map_finish_reason(gemini_reason: Option<&str>, has_tool_calls: bool) -> Option<String> {
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
}
