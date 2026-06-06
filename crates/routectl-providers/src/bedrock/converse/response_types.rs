//! AWS Bedrock Converse response + streaming wire types.
//!
//! Sibling of `types.rs` (which holds the request-side `Serialize`-only
//! shapes). Splitting the deserialize-only types here keeps each file
//! under the 800-line cap and makes ownership obvious: `types.rs` is
//! the request shape, `response_types.rs` is the reply shape (both the
//! one-shot Converse JSON body and the per-frame ConverseStream
//! payloads).
//!
//! All types tolerate missing fields with `#[serde(default)]` so a
//! minimal AWS reply (e.g. text-only with no metrics) parses cleanly.
//! Forward-compat is via `Other(Value)` arms on the union enums --
//! a future AWS block type ships without a rebuild on the
//! all-passthrough path.

// Forward-compat fields (`metrics`, `latency_ms`, `role`, tuple-struct
// payloads) are deserialized so the shape round-trips cleanly; nothing
// reads them yet, but dropping them would make a future field addition
// silently lossy on the wire.
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Non-streaming response body (the one-shot /converse reply)
// ---------------------------------------------------------------------------

/// Top-level Bedrock Converse response body. Anything routectl doesn't
/// surface on canonical lands in `additional_model_response_fields`
/// (free-form Value) -- AWS reflects vendor-specific knobs the operator
/// pulled in via `additionalModelResponseFieldPaths` here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseResponse {
    pub(crate) output: ConverseOutput,
    /// `"end_turn" | "tool_use" | "max_tokens" | "stop_sequence" |
    /// "guardrail_intervened" | "content_filtered" |
    /// "malformed_model_output" | "malformed_tool_use" |
    /// "model_context_window_exceeded"`. Optional because the AWS
    /// schema doesn't mark it required and a malformed reply could
    /// theoretically omit it; we map it to `finish_reason`.
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<ConverseUsage>,
    #[serde(default)]
    pub(crate) metrics: Option<ConverseMetrics>,
    /// AWS-side reflection of `additionalModelResponseFieldPaths` from
    /// the request. Captured for forward compat; routectl currently
    /// doesn't surface it on canonical `ChatResponse`.
    #[serde(default)]
    pub(crate) additional_model_response_fields: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConverseOutput {
    pub(crate) message: ConverseResponseMessage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConverseResponseMessage {
    pub(crate) role: String,
    pub(crate) content: Vec<ConverseResponseContentBlock>,
}

/// One content block on the response side. Single-key untagged union
/// matching the AWS shape. Forward compat: anything routectl doesn't
/// recognize falls through `Other` so a future AWS block type ships
/// without a rebuild.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ConverseResponseContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: ConverseResponseToolUse,
    },
    ReasoningContent {
        #[serde(rename = "reasoningContent")]
        reasoning_content: ConverseReasoningContent,
    },
    Other(Value),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseResponseToolUse {
    pub(crate) tool_use_id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
}

/// AWS reasoning union. Per the AWS docs only one of the two members is
/// populated per block: `reasoningText` for visible chain-of-thought,
/// `redactedContent` for safety-redacted reasoning carried as
/// base64-encoded bytes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseReasoningContent {
    #[serde(default)]
    pub(crate) reasoning_text: Option<ConverseReasoningText>,
    #[serde(default)]
    pub(crate) redacted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConverseReasoningText {
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) signature: Option<String>,
}

/// AWS-side TokenUsage. `cacheWriteInputTokens` is the AWS Converse
/// equivalent of Anthropic's `cache_creation_input_tokens` (see the
/// Converse `TokenUsage` API doc); both surfaces report tokens-written
/// on a cache miss. The cache-detail breakdown rides as
/// `cacheDetails: [{inputTokens, ttl}]` -- routectl flattens it into
/// the canonical `cache_creation` per-TTL object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseUsage {
    #[serde(default)]
    pub(crate) input_tokens: u32,
    #[serde(default)]
    pub(crate) output_tokens: u32,
    #[serde(default)]
    pub(crate) total_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_write_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_details: Option<Vec<ConverseCacheDetail>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseCacheDetail {
    pub(crate) input_tokens: u32,
    /// `"5m"` | `"1h"` -- mirrors the request-side `CachePoint.ttl`.
    pub(crate) ttl: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConverseMetrics {
    pub(crate) latency_ms: u64,
}

// ---------------------------------------------------------------------------
// Streaming event payloads (used by eventstream.rs)
// ---------------------------------------------------------------------------
//
// Each AWS Converse stream frame's `:event-type` header names one of
// these payload shapes; the JSON body of the frame deserializes into
// the matching struct. Frame routing lives in
// `bedrock::converse::eventstream`; the types here are pure wire
// shapes.

#[derive(Debug, Deserialize)]
pub(crate) struct StreamMessageStart {
    #[serde(default)]
    pub(crate) role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamContentBlockStart {
    pub(crate) content_block_index: u32,
    #[serde(default)]
    pub(crate) start: Option<StreamContentBlockStartPayload>,
}

/// `start` payload of a `contentBlockStart` event. Today AWS only
/// populates this for tool_use blocks; text + reasoning blocks open
/// without a typed start payload.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum StreamContentBlockStartPayload {
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: StreamToolUseStart,
    },
    Other(Value),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamToolUseStart {
    pub(crate) tool_use_id: String,
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamContentBlockDelta {
    pub(crate) content_block_index: u32,
    #[serde(default)]
    pub(crate) delta: Option<StreamDelta>,
}

/// `delta` payload of a `contentBlockDelta` event. Per AWS docs the
/// shape is a union; only one of (text, toolUse, reasoningContent,
/// citation, image, toolResult) is populated per frame.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum StreamDelta {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: StreamToolUseDelta,
    },
    ReasoningContent {
        #[serde(rename = "reasoningContent")]
        reasoning_content: StreamReasoningDelta,
    },
    /// Forward compat: anything we don't recognize lands here so the
    /// stream doesn't error out on a new AWS delta type.
    Other(Value),
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamToolUseDelta {
    /// Partial JSON arguments accumulated by the AWS SDK; concatenate
    /// all deltas for a given block to assemble the full tool input.
    pub(crate) input: String,
}

/// Streaming counterpart of `ConverseReasoningContent`. AWS picks a
/// single member per delta: `text` for incremental thinking, `signature`
/// for the verification token at end of block, `redactedContent` for
/// safety-redacted bytes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamReasoningDelta {
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) signature: Option<String>,
    #[serde(default)]
    pub(crate) redacted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamContentBlockStop {
    pub(crate) content_block_index: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamMessageStop {
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) additional_model_response_fields: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamMetadata {
    #[serde(default)]
    pub(crate) usage: Option<ConverseUsage>,
    #[serde(default)]
    pub(crate) metrics: Option<ConverseMetrics>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_text_only_message_deserializes() {
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
        let resp: ConverseResponse = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.output.message.role, "assistant");
        match &resp.output.message.content[0] {
            ConverseResponseContentBlock::Text { text } => assert_eq!(text, "hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
        let u = resp.usage.unwrap();
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.total_tokens, Some(30));
        assert_eq!(resp.metrics.unwrap().latency_ms, 1234);
    }

    #[test]
    fn response_tool_use_block_deserializes_with_camel_case_id() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"toolUse": {"toolUseId": "tu_1", "name": "calc",
                                     "input": {"a": 1}}}
                    ]
                }
            },
            "stopReason": "tool_use"
        });

        // Act
        let resp: ConverseResponse = serde_json::from_value(raw).unwrap();

        // Assert
        match &resp.output.message.content[0] {
            ConverseResponseContentBlock::ToolUse { tool_use } => {
                assert_eq!(tool_use.tool_use_id, "tu_1");
                assert_eq!(tool_use.name, "calc");
                assert_eq!(tool_use.input, json!({"a": 1}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn response_reasoning_text_block_deserializes() {
        // Arrange
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"reasoningContent": {
                            "reasoningText": {"text": "step 1", "signature": "sig"}
                        }}
                    ]
                }
            }
        });

        // Act
        let resp: ConverseResponse = serde_json::from_value(raw).unwrap();

        // Assert
        match &resp.output.message.content[0] {
            ConverseResponseContentBlock::ReasoningContent { reasoning_content } => {
                let rt = reasoning_content.reasoning_text.as_ref().unwrap();
                assert_eq!(rt.text, "step 1");
                assert_eq!(rt.signature.as_deref(), Some("sig"));
                assert!(reasoning_content.redacted_content.is_none());
            }
            other => panic!("expected ReasoningContent, got {other:?}"),
        }
    }

    #[test]
    fn response_unknown_content_block_falls_to_other() {
        // Arrange: a hypothetical future AWS block type. Forward
        // compat means we accept it as Other and let the response
        // walker decide whether to surface it.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"futureBlock": {"someField": "x"}}
                    ]
                }
            }
        });

        // Act
        let resp: ConverseResponse = serde_json::from_value(raw).unwrap();

        // Assert
        match &resp.output.message.content[0] {
            ConverseResponseContentBlock::Other(_) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn response_usage_cache_fields_deserialize() {
        // Arrange
        let raw = json!({
            "inputTokens": 50,
            "outputTokens": 10,
            "totalTokens": 60,
            "cacheReadInputTokens": 200,
            "cacheWriteInputTokens": 100,
            "cacheDetails": [
                {"inputTokens": 50, "ttl": "5m"},
                {"inputTokens": 50, "ttl": "1h"}
            ]
        });

        // Act
        let u: ConverseUsage = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(u.cache_read_input_tokens, Some(200));
        assert_eq!(u.cache_write_input_tokens, Some(100));
        let details = u.cache_details.unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].ttl, "5m");
        assert_eq!(details[1].ttl, "1h");
    }

    #[test]
    fn stream_content_block_delta_text_deserializes() {
        // Arrange
        let raw = json!({"contentBlockIndex": 0, "delta": {"text": "hello"}});

        // Act
        let ev: StreamContentBlockDelta = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(ev.content_block_index, 0);
        match ev.delta.unwrap() {
            StreamDelta::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text delta, got {other:?}"),
        }
    }

    #[test]
    fn stream_content_block_delta_tool_use_carries_partial_json() {
        // Arrange
        let raw = json!({
            "contentBlockIndex": 1,
            "delta": {"toolUse": {"input": "{\"a\":"}}
        });

        // Act
        let ev: StreamContentBlockDelta = serde_json::from_value(raw).unwrap();

        // Assert
        match ev.delta.unwrap() {
            StreamDelta::ToolUse { tool_use } => assert_eq!(tool_use.input, "{\"a\":"),
            other => panic!("expected ToolUse delta, got {other:?}"),
        }
    }

    #[test]
    fn stream_content_block_start_with_tool_use_payload() {
        // Arrange
        let raw = json!({
            "contentBlockIndex": 1,
            "start": {"toolUse": {"toolUseId": "tu_1", "name": "calc"}}
        });

        // Act
        let ev: StreamContentBlockStart = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(ev.content_block_index, 1);
        match ev.start.unwrap() {
            StreamContentBlockStartPayload::ToolUse { tool_use } => {
                assert_eq!(tool_use.tool_use_id, "tu_1");
                assert_eq!(tool_use.name, "calc");
            }
            other => panic!("expected ToolUse start, got {other:?}"),
        }
    }

    #[test]
    fn stream_metadata_carries_usage_and_metrics() {
        // Arrange
        let raw = json!({
            "usage": {"inputTokens": 5, "outputTokens": 3, "totalTokens": 8},
            "metrics": {"latencyMs": 999}
        });

        // Act
        let ev: StreamMetadata = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(ev.usage.unwrap().input_tokens, 5);
        assert_eq!(ev.metrics.unwrap().latency_ms, 999);
    }

    #[test]
    fn stream_reasoning_delta_text_only_deserializes() {
        // Arrange: AWS picks ONE member per delta. Text-only is the
        // common chain-of-thought streaming path.
        let raw = json!({"text": "thinking..."});

        // Act
        let d: StreamReasoningDelta = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(d.text.as_deref(), Some("thinking..."));
        assert!(d.signature.is_none());
        assert!(d.redacted_content.is_none());
    }

    #[test]
    fn response_unknown_top_level_fields_are_silently_dropped() {
        // Arrange: unknown top-level fields (e.g. a future AWS key)
        // must not cause a deserialization error now that the
        // flattened extras catch-all has been removed. serde's default
        // behavior for named structs is to ignore unknown keys.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hi"}]
                }
            },
            "stopReason": "end_turn",
            "unknownFutureField": {"some": "data"},
            "anotherNewField": 42
        });

        // Act: must not panic or error on unknown top-level fields.
        let resp: ConverseResponse = serde_json::from_value(raw).unwrap();

        // Assert: known fields still parse cleanly.
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.output.message.role, "assistant");
    }
}
