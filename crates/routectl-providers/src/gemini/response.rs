//! Gemini `generateContent` response -> canonical `ChatResponse` translation.
//!
//! Finish-reason mapping (Gemini -> canonical):
//!   STOP                                     -> "stop"
//!   MAX_TOKENS                               -> "length"
//!   SAFETY, RECITATION, PROHIBITED_CONTENT,
//!   BLOCKLIST, SPII, IMAGE_SAFETY, LANGUAGE  -> "content_filter"
//!   MALFORMED_FUNCTION_CALL,
//!   UNEXPECTED_TOOL_CALL, TOO_MANY_TOOL_CALLS -> "error"
//!   any functionCall parts                   -> "tool_calls" (overrides the above)
//!   OTHER / any unknown token                -> "stop" (safe default; logged at DEBUG)
//!
//! A prompt-level block (empty candidates + `promptFeedback.blockReason`)
//! also maps to "content_filter" on the HTTP-200 surface.

use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use routectl_core::{
    ChatResponse, Choice, Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Result,
    Role, Usage,
};

use super::GEMINI_FORMAT;
use super::types::{GenerateContentResponse, ResponsePart, UsageMetadata};

/// Translate a deserialized Gemini response into canonical `ChatResponse`.
pub fn translate(provider_id: &str, resp: GenerateContentResponse) -> Result<ChatResponse> {
    let candidate = resp.candidates.into_iter().next();

    let (text, tool_calls, reasoning_details, finish_reason) = match candidate {
        None => {
            let block_reason = resp
                .prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref())
                .filter(|r| !r.is_empty());
            let finish = match block_reason {
                Some(reason) => {
                    tracing::info!(
                        provider = %provider_id,
                        surface = "complete",
                        origin = "prompt_feedback",
                        block_reason = %reason,
                        "gemini: prompt blocked on 200 surface"
                    );
                    Some("content_filter".to_string())
                }
                None => Some("stop".to_string()),
            };
            (String::new(), None, Vec::new(), finish)
        }
        Some(c) => {
            let parts = c.content.map(|cont| cont.parts).unwrap_or_default();
            let walked = walk_parts(provider_id, &parts)?;
            let has_tool_calls = walked.tool_calls.is_some();
            let finish = map_finish_reason(c.finish_reason.as_deref(), has_tool_calls);
            if finish.as_deref() == Some("content_filter") {
                tracing::info!(
                    provider = %provider_id,
                    surface = "complete",
                    origin = "candidate_finish",
                    block_reason = c.finish_reason.as_deref().unwrap_or_default(),
                    "gemini: candidate blocked on 200 surface"
                );
            }
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
///
/// Safety / policy tokens (`SAFETY`, `RECITATION`, `PROHIBITED_CONTENT`,
/// `BLOCKLIST`, `SPII`, `IMAGE_SAFETY`, `LANGUAGE`) map to `content_filter`;
/// tool-protocol failures (`MALFORMED_FUNCTION_CALL`, `UNEXPECTED_TOOL_CALL`,
/// `TOO_MANY_TOOL_CALLS`) map to `error`. `OTHER` and any token this map does
/// not know are deliberately left as `stop` -- labelling a non-policy
/// termination as `content_filter` would be wrong -- and the fallthrough
/// emits a DEBUG naming the token so a newly minted Google token surfaces.
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
    let mapped = match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY"
        | "LANGUAGE" => "content_filter",
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "TOO_MANY_TOOL_CALLS" => "error",
        other => {
            tracing::debug!(
                finish_reason = %other,
                "gemini: unmapped finishReason; defaulting to stop"
            );
            "stop"
        }
    };
    Some(mapped.to_string())
}

/// Map Gemini `usageMetadata` to canonical `Usage`.
fn translate_usage(meta: &UsageMetadata) -> Usage {
    let mut usage = Usage {
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
    };
    usage.derive_total_if_absent();
    usage
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::types::{
        Candidate, GenerateContentResponse, PromptFeedback, ResponseContent, ResponseFunctionCall,
        ResponsePart, UsageMetadata,
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
            prompt_feedback: None,
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
            prompt_feedback: None,
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
    fn content_filter_tokens_map_to_content_filter() {
        for token in [
            "SAFETY",
            "RECITATION",
            "PROHIBITED_CONTENT",
            "BLOCKLIST",
            "SPII",
            "IMAGE_SAFETY",
            "LANGUAGE",
        ] {
            assert_eq!(
                map_finish_reason(Some(token), false).as_deref(),
                Some("content_filter"),
                "token {token} should map to content_filter"
            );
        }
    }

    #[test]
    fn tool_error_tokens_map_to_error() {
        for token in [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            "TOO_MANY_TOOL_CALLS",
        ] {
            assert_eq!(
                map_finish_reason(Some(token), false).as_deref(),
                Some("error"),
                "token {token} should map to error"
            );
        }
    }

    #[test]
    fn other_and_unknown_tokens_map_to_stop() {
        assert_eq!(
            map_finish_reason(Some("OTHER"), false).as_deref(),
            Some("stop")
        );
        assert_eq!(
            map_finish_reason(Some("SOME_FUTURE_TOKEN"), false).as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn retained_arms_unchanged() {
        assert_eq!(
            map_finish_reason(Some("STOP"), false).as_deref(),
            Some("stop")
        );
        assert_eq!(
            map_finish_reason(Some("MAX_TOKENS"), false).as_deref(),
            Some("length")
        );
        // has_tool_calls short-circuits regardless of the reason token.
        assert_eq!(
            map_finish_reason(Some("SAFETY"), true).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(map_finish_reason(None, false).as_deref(), Some("stop"));
        assert_eq!(map_finish_reason(Some(""), false).as_deref(), Some("stop"));
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
    fn absent_total_derived_from_component_sum() {
        let meta = UsageMetadata {
            prompt_token_count: 100,
            candidates_token_count: 50,
            total_token_count: 0,
            cached_content_token_count: 0,
            thoughts_token_count: 0,
            tool_use_prompt_token_count: 0,
        };
        let usage = translate_usage(&meta);
        assert_eq!(usage.total_tokens, 150);
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
            prompt_feedback: None,
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

    fn empty_response(prompt_feedback: Option<PromptFeedback>) -> GenerateContentResponse {
        GenerateContentResponse {
            candidates: vec![],
            usage_metadata: None,
            model_version: Some("gemini-2.5-pro".into()),
            response_id: Some("resp-x".into()),
            prompt_feedback,
        }
    }

    #[test]
    fn empty_candidates_with_prompt_block_maps_to_content_filter() {
        let resp = empty_response(Some(PromptFeedback {
            block_reason: Some("SAFETY".into()),
        }));
        let chat = translate("gemini:test", resp).expect("translate ok");
        assert_eq!(
            chat.choices[0].finish_reason.as_deref(),
            Some("content_filter")
        );
    }

    #[test]
    fn empty_candidates_without_prompt_block_stays_stop() {
        // A legit empty completion (no candidates, no blockReason) must
        // remain a clean stop, not be mislabelled as a policy block.
        let chat = translate("gemini:test", empty_response(None)).expect("translate ok");
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("stop"));

        // An empty (present-but-blank) blockReason is also not a block.
        let resp = empty_response(Some(PromptFeedback {
            block_reason: Some(String::new()),
        }));
        let chat = translate("gemini:test", resp).expect("translate ok");
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn prompt_block_emits_one_info_no_payload() {
        let events = routectl_testkit::capture_events(|| {
            let resp = empty_response(Some(PromptFeedback {
                block_reason: Some("PROHIBITED_CONTENT".into()),
            }));
            let _ = translate("gemini:test", resp).expect("translate ok");
        });
        let infos: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::INFO)
            .collect();
        assert_eq!(infos.len(), 1, "exactly one INFO; got {infos:?}");
        assert_eq!(infos[0].field("provider"), Some("gemini:test"));
        assert_eq!(infos[0].field("surface"), Some("complete"));
        assert_eq!(infos[0].field("origin"), Some("prompt_feedback"));
        assert_eq!(infos[0].field("block_reason"), Some("PROHIBITED_CONTENT"));
    }

    #[test]
    fn candidate_content_filter_emits_one_info() {
        let events = routectl_testkit::capture_events(|| {
            let _ = translate("gemini:test", text_response("", "SAFETY")).expect("translate ok");
        });
        let infos: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::INFO)
            .collect();
        assert_eq!(infos.len(), 1, "exactly one INFO; got {infos:?}");
        assert_eq!(infos[0].field("surface"), Some("complete"));
        assert_eq!(infos[0].field("origin"), Some("candidate_finish"));
        assert_eq!(infos[0].field("block_reason"), Some("SAFETY"));
    }

    #[test]
    fn unmapped_token_emits_debug_but_mapped_tokens_do_not() {
        // The fallthrough arm is the only place a DEBUG fires.
        let events = routectl_testkit::capture_events(|| {
            assert_eq!(
                map_finish_reason(Some("SOME_FUTURE_TOKEN"), false).as_deref(),
                Some("stop")
            );
        });
        let debugs: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::DEBUG)
            .collect();
        assert_eq!(
            debugs.len(),
            1,
            "one DEBUG on the fallthrough; got {debugs:?}"
        );
        assert_eq!(debugs[0].field("finish_reason"), Some("SOME_FUTURE_TOKEN"));

        // A mapped token never logs.
        let quiet = routectl_testkit::capture_events(|| {
            let _ = map_finish_reason(Some("STOP"), false);
            let _ = map_finish_reason(Some("SAFETY"), false);
        });
        assert!(
            quiet.iter().all(|e| e.level != tracing::Level::DEBUG),
            "mapped tokens must not log; got {quiet:?}"
        );
    }

    #[test]
    fn quota_and_permission_stay_off_content_filter_path() {
        use routectl_core::Error;
        use routectl_core::failure_class::{FailureClass, classify};

        let rate_limited = Error::upstream_full(
            "gemini",
            429,
            "",
            None,
            Some("RESOURCE_EXHAUSTED".to_string()),
            None,
        );
        assert_eq!(
            classify(&rate_limited, Some("gemini")).class,
            FailureClass::RateLimited
        );

        let auth = Error::upstream_full(
            "gemini",
            403,
            "",
            None,
            Some("PERMISSION_DENIED".to_string()),
            None,
        );
        assert_eq!(classify(&auth, Some("gemini")).class, FailureClass::Auth);
    }
}
