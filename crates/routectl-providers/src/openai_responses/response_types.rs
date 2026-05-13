//! OpenAI Responses API response + streaming wire types.
//!
//! Sibling of `types.rs` (which carries the Serialize-only request
//! shapes). Splitting the deserialize-only types here keeps each file
//! under the 800-line cap and makes ownership obvious:
//! - `types.rs`           -- request body (egress -> upstream)
//! - `response_types.rs`  -- response body + SSE event payloads
//!   (upstream -> ingress)
//!
//! Reference:
//! - `codex-rs/codex-api/src/common.rs` -- ResponsesResponse +
//!   ResponseCompleted shapes.
//! - `codex-rs/codex-api/src/sse/responses.rs:179-192` -- the
//!   `ResponsesStreamEvent` flat-schema shape (codex deserializes
//!   every Responses SSE event into the same struct with all fields
//!   optional and dispatches on `kind`/`type`).
//! - `codex-rs/app-server-protocol/schema/typescript/ResponseItem.ts`
//!   -- per-item-type schemas (reasoning, message, function_call,
//!   function_call_output).
//!
//! All types tolerate missing fields (`#[serde(default)]`) so a
//! minimal upstream reply parses cleanly. Forward compat: every
//! tagged-union (`ResponseOutputItem`, `ResponsesOutputContent`,
//! `ReasoningContent`) has an `Other(Value)` catch-all so a new wire
//! shape doesn't break the decoder.

use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Non-streaming response body (POST /responses with stream:false)
// ---------------------------------------------------------------------------

/// Top-level OpenAI Responses response body. Mirrors the codex
/// `ResponseCompleted` + `ResponsesResponse` shapes (codex splits the
/// "I see a complete response" wire object across two structs; we keep
/// the one-shot path on a single struct to match Anthropic/Bedrock
/// patterns).
#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesResponse {
    #[serde(default)]
    pub(crate) id: String,
    /// Echo of `"response"` from upstream. Currently unused but kept on
    /// the wire-shape struct for forward compat.
    #[serde(default, rename = "object")]
    pub(crate) _object: Option<String>,
    #[serde(default)]
    pub(crate) created_at: i64,
    /// `"completed" | "in_progress" | "incomplete" | "failed" |
    /// "cancelled"`. Optional because a minimal reply (or a
    /// generic-error JSON envelope from a misconfigured upstream) may
    /// omit it; the translator falls back to `None`.
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) error: Option<Value>,
    #[serde(default)]
    pub(crate) incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) output: Vec<ResponseOutputItem>,
    #[serde(default)]
    pub(crate) usage: Option<ResponsesUsage>,
}

/// `incomplete_details: { reason: "..." }`. Reason values seen in the
/// codex test fixtures: `"max_output_tokens"`, `"content_filter"`.
#[derive(Debug, Deserialize)]
pub(crate) struct IncompleteDetails {
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

/// One entry in `response.output[]`. Dispatched on `"type"` by a
/// custom Deserialize impl (see below) so the `Other` variant can
/// preserve the full raw JSON for forward-compat egress passthrough.
///
/// - `message`        -- assistant content (text or refusal)
/// - `reasoning`      -- chain-of-thought summary + optional
///   encrypted_content signature
/// - `function_call`  -- tool invocation
/// - `Other(Value)`   -- forward compat (function_call_output on the
///   response side, future custom_tool_call, web_search, mcp_call,
///   etc.). Carries the original JSON value verbatim so the response
///   translator can lift the `type` tag into `ContentPart::Other.type_tag`
///   and the remaining fields into `extras`.
///
/// `#[allow(dead_code)]` on the wire-only fields (id, role, status)
/// because they're deserialized for forward-compat round-trip but
/// the translator doesn't consume them today.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum ResponseOutputItem {
    Message {
        id: String,
        role: String,
        status: Option<String>,
        content: Vec<ResponsesOutputContent>,
    },
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummary>,
        content: Vec<ReasoningContent>,
        /// Replay signature. Present on items the upstream wants the
        /// client to echo back verbatim on the next turn (mirrors
        /// codex `arc_monitor.rs:325-336`).
        encrypted_content: Option<String>,
        status: Option<String>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
    /// Forward-compat catchall. Preserves the original `{type, ...}`
    /// JSON verbatim so the response translator emits a
    /// `ContentPart::Other { type_tag, extras }` carrying every field.
    Other(Value),
}

impl<'de> Deserialize<'de> for ResponseOutputItem {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Step 1: deserialize to a generic JSON Value so the "type"
        // discriminant can be inspected without committing to a
        // tagged-enum decode that would either reject unknown types or
        // drop their payload. Mirrors the pattern in
        // `bedrock/converse/response_types.rs::ConverseResponseContentBlock`
        // which uses untagged + Other(Value) for the same reason.
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "message" => {
                // Each known variant has a small typed payload struct
                // immediately below; we serde-translate the captured
                // Value into that struct. A schema mismatch on a known
                // type is still an error (the translator depends on
                // typed fields), but UNKNOWN types fall to Other and
                // preserve their JSON.
                let m: MessageItemPayload =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(ResponseOutputItem::Message {
                    id: m.id,
                    role: m.role,
                    status: m.status,
                    content: m.content,
                })
            }
            "reasoning" => {
                let r: ReasoningItemPayload =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(ResponseOutputItem::Reasoning {
                    id: r.id,
                    summary: r.summary,
                    content: r.content,
                    encrypted_content: r.encrypted_content,
                    status: r.status,
                })
            }
            "function_call" => {
                let f: FunctionCallItemPayload =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(ResponseOutputItem::FunctionCall {
                    id: f.id,
                    call_id: f.call_id,
                    name: f.name,
                    arguments: f.arguments,
                    status: f.status,
                })
            }
            _ => Ok(ResponseOutputItem::Other(value)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MessageItemPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    content: Vec<ResponsesOutputContent>,
}

#[derive(Debug, Deserialize)]
struct ReasoningItemPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    summary: Vec<ReasoningSummary>,
    #[serde(default)]
    content: Vec<ReasoningContent>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FunctionCallItemPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    status: Option<String>,
}

/// Outer-level `Other` carries no fields because serde's `#[serde(other)]`
/// only matches the tag. For forward-compat extras we use a second
/// untagged-fallback variant via a wrapper enum below.
///
/// Variants of `ResponsesOutputContent` (inside Message.content):
///
///   - `output_text` -- assistant text + optional annotations
///   - `refusal`     -- safety refusal string
///   - `Other(Value)` -- forward compat
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ResponsesOutputContent {
    OutputText {
        #[serde(default)]
        text: String,
        /// Annotations array (citations etc.) -- preserved for forward
        /// compat but not consumed by the translator.
        #[serde(default)]
        annotations: Vec<Value>,
    },
    Refusal {
        #[serde(default)]
        refusal: String,
    },
    /// Catchall via the serde `other` discriminant. Inner payload not
    /// preserved -- callers that need verbatim passthrough should add
    /// the missing variant.
    #[serde(other)]
    Other,
}

/// One summary block on a `Reasoning` output item.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningSummary {
    SummaryText {
        #[serde(default)]
        text: String,
    },
    #[serde(other)]
    Other,
}

/// One content block on a `Reasoning` output item.
///
/// - `reasoning_text`      -- visible chain-of-thought text
/// - `text`                -- alias for `reasoning_text` emitted by
///   some Responses-API model variants (codex
///   `protocol/src/models.rs:1198-1203` documents both tags coexisting
///   on the `ReasoningItemContent` union)
/// - `reasoning_encrypted` -- safety-redacted reasoning
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningContent {
    ReasoningText {
        #[serde(default)]
        text: String,
    },
    /// Plain `"text"` discriminant, identical payload to
    /// `reasoning_text`. Treated as a reasoning-text block by the
    /// translator (mapped to `ReasoningDetailKind::Text`).
    Text {
        #[serde(default)]
        text: String,
    },
    ReasoningEncrypted {
        #[serde(default)]
        encrypted_content: String,
    },
    #[serde(other)]
    Other,
}

/// Token usage shape. Fields all optional via `#[serde(default)]` so a
/// minimal reply (e.g. an error envelope that still happens to include
/// `usage: {}`) doesn't trip serde.
#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesUsage {
    #[serde(default)]
    pub(crate) input_tokens: u32,
    /// `{ cached_tokens: N, ... }`. Captured as a free-form Value so
    /// future detail fields ride through without a rebuild.
    #[serde(default)]
    pub(crate) input_tokens_details: Option<Value>,
    #[serde(default)]
    pub(crate) output_tokens: u32,
    /// `{ reasoning_tokens: N, ... }`. Same forward-compat shape as
    /// `input_tokens_details`.
    #[serde(default)]
    pub(crate) output_tokens_details: Option<Value>,
    #[serde(default)]
    pub(crate) total_tokens: u32,
}

// ---------------------------------------------------------------------------
// SSE streaming event payloads
// ---------------------------------------------------------------------------

/// Flat-schema SSE event shape mirroring codex's
/// `ResponsesStreamEvent`. Every Responses SSE event is JSON of this
/// form -- the `type` discriminant drives dispatch in `sse.rs`.
///
/// All payload-bearing fields are optional because each event only
/// populates a small subset. We intentionally do NOT model this as a
/// per-type tagged enum: codex itself uses the flat shape, OpenAI adds
/// new event kinds quarterly, and a tagged enum would refuse to parse
/// future kinds (forward-compat regression). The flat shape parses
/// every event the same way; unknown `type` values land in `sse.rs`'s
/// default arm and log at debug.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ResponsesStreamEvent {
    #[serde(rename = "type", default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) item: Option<Value>,
    /// Item id correlated with this event (some events carry it
    /// alongside `output_index`). Preserved for forward compat.
    #[serde(default)]
    pub(crate) item_id: Option<String>,
    #[serde(default)]
    pub(crate) output_index: Option<u32>,
    /// content_index on reasoning_text deltas (codex models it; we
    /// route via `output_index` alone today).
    #[serde(default)]
    pub(crate) content_index: Option<u32>,
    /// summary_index on reasoning_summary_text deltas. Same forward-
    /// compat reason as `content_index`.
    #[serde(default)]
    pub(crate) summary_index: Option<u32>,
    /// call_id on function_call.* deltas (alternative dispatch key for
    /// future event kinds that don't carry output_index).
    #[serde(default)]
    pub(crate) call_id: Option<String>,
    #[serde(default)]
    pub(crate) delta: Option<String>,
    /// Final text on `*.done` events. Today the state machine doesn't
    /// rely on this -- accumulated deltas suffice -- but the field is
    /// here for completeness and future use.
    #[serde(default)]
    pub(crate) text: Option<String>,
    /// Finalized arguments on function_call_arguments.done. Same
    /// rationale as `text`.
    #[serde(default)]
    pub(crate) arguments: Option<String>,
    #[serde(default)]
    pub(crate) response: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_response_text_only_deserializes() {
        // Arrange
        let raw = json!({
            "id": "resp_01",
            "object": "response",
            "created_at": 1700000000,
            "status": "completed",
            "model": "gpt-5-codex",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15
            }
        });

        // Act
        let resp: ResponsesResponse = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(resp.id, "resp_01");
        assert_eq!(resp.status.as_deref(), Some("completed"));
        assert_eq!(resp.output.len(), 1);
        match &resp.output[0] {
            ResponseOutputItem::Message { content, role, .. } => {
                assert_eq!(role, "assistant");
                match &content[0] {
                    ResponsesOutputContent::OutputText { text, .. } => assert_eq!(text, "hi"),
                    other => panic!("expected OutputText, got {other:?}"),
                }
            }
            other => panic!("expected Message, got {other:?}"),
        }
        assert_eq!(resp.usage.unwrap().output_tokens, 5);
    }

    #[test]
    fn responses_response_reasoning_with_encrypted_content_deserializes() {
        // Arrange
        let raw = json!({
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "ENC123"
            }]
        });

        // Act
        let resp: ResponsesResponse = serde_json::from_value(raw).unwrap();

        // Assert
        match &resp.output[0] {
            ResponseOutputItem::Reasoning {
                summary,
                encrypted_content,
                ..
            } => {
                assert_eq!(summary.len(), 1);
                match &summary[0] {
                    ReasoningSummary::SummaryText { text } => assert_eq!(text, "thinking"),
                    other => panic!("expected SummaryText, got {other:?}"),
                }
                assert_eq!(encrypted_content.as_deref(), Some("ENC123"));
            }
            other => panic!("expected Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn responses_response_function_call_deserializes() {
        // Arrange
        let raw = json!({
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"loc\":\"Tokyo\"}"
            }]
        });

        // Act
        let resp: ResponsesResponse = serde_json::from_value(raw).unwrap();

        // Assert
        match &resp.output[0] {
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(call_id, "call_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments, "{\"loc\":\"Tokyo\"}");
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn responses_response_unknown_output_type_falls_to_other() {
        // Arrange
        let raw = json!({
            "output": [{"type": "web_search_call", "id": "ws_1"}]
        });

        // Act
        let resp: ResponsesResponse = serde_json::from_value(raw).unwrap();

        // Assert: Other now carries the raw Value so the response
        // translator can lift the type tag and extras.
        match &resp.output[0] {
            ResponseOutputItem::Other(v) => {
                assert_eq!(
                    v.get("type").and_then(|t| t.as_str()),
                    Some("web_search_call")
                );
                assert_eq!(v.get("id").and_then(|t| t.as_str()), Some("ws_1"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn responses_stream_event_flat_schema_deserializes() {
        // Arrange
        let raw = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        });

        // Act
        let ev: ResponsesStreamEvent = serde_json::from_value(raw).unwrap();

        // Assert
        assert_eq!(ev.kind, "response.output_text.delta");
        assert_eq!(ev.output_index, Some(0));
        assert_eq!(ev.delta.as_deref(), Some("hello"));
    }

    #[test]
    fn responses_stream_event_minimal_unknown_kind_deserializes() {
        // Forward compat: an unknown event still parses; sse.rs's
        // default arm logs at debug + continues.
        let raw = json!({"type": "response.future_event"});
        let ev: ResponsesStreamEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(ev.kind, "response.future_event");
        assert!(ev.output_index.is_none());
    }
}
