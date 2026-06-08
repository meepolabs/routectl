//! OpenAI Responses API wire types.
//!
//! Serialize-only structs mirroring the request body that the codex
//! reference implementation sends to either:
//!
//!   - `https://chatgpt.com/backend-api/codex/responses`
//!     (ChatGPT subscription / `auth_kind = "chatgpt-oauth"`)
//!   - `https://api.openai.com/v1/responses`
//!     (standard API key / `auth_kind = "api-key"`)
//!
//! Response-side types land in the relevant stage (`response_types.rs`); this file
//! covers only the egress request shape so the relevant stage can complete the
//! translation pipeline.
//!
//! Reference: `codex-rs/codex-api/src/common.rs::ResponsesApiRequest`
//! and `codex-rs/app-server-protocol/schema/typescript/ResponseItem.ts`
//! for the input-item union.

use serde::Serialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Top-level request body
// ---------------------------------------------------------------------------

/// Top-level OpenAI Responses request body.
///
/// Field order matches codex's `ResponsesApiRequest` for byte-stable
/// diffs when wiring up the live smoke test. Optionals skip when None
/// so the emitted JSON omits absent fields rather than rendering null.
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesRequest {
    pub(crate) model: String,

    // Always serialized, even when empty. The chatgpt-oauth backend returns
    // {"detail":"Instructions are required"} (400) when the field is absent
    // entirely; an empty string "" is accepted and treated as "no system
    // prompt" by the server.
    pub(crate) instructions: String,

    pub(crate) input: Vec<ResponseInputItem>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<ResponsesTool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<Value>,

    pub(crate) parallel_tool_calls: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<ResponsesReasoning>,

    /// Whether the server should persist the response. Hardcoded to
    /// `false` for the ChatGPT-OAuth surface (codex parity); other
    /// auth_kinds may surface this as an operator-visible knob later.
    pub(crate) store: bool,

    pub(crate) stream: bool,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) include: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) service_tier: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextControls>,

    /// Operator-supplied cost-attribution / request-tagging metadata.
    /// Forwarded verbatim from `provider_extras["client_metadata"]`.
    /// Accepts any JSON shape so the egress does not force a
    /// string-string constraint that upstream may not require.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) client_metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Input items
// ---------------------------------------------------------------------------

/// One entry in the `input` array. Tagged-union shape: every variant
/// emits a top-level `type` discriminant. Maps to the codex
/// `ResponseItem` typescript union.
///
/// Reasoning replay: `Reasoning` items must carry `encrypted_content`
/// (possibly empty when no prior signature exists). Codex re-injects
/// reasoning only when `encrypted_content` is non-empty.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponseInputItem {
    Message {
        role: String,
        content: Vec<ResponsesContentItem>,
    },
    Reasoning {
        /// Upstream-stable item id (e.g. "rs_1"). Skipped when None so
        /// fresh client-side Thinking blocks (no upstream provenance)
        /// don't ship a synthetic id. When the canonical envelope
        /// preserves the upstream id via `reasoning_details[].id`, the
        /// replay path forwards it verbatim so server-side dedup works.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        summary: Vec<ReasoningSummaryItem>,
        /// Optional inner content array (reasoning_text /
        /// reasoning_encrypted entries). Skipped when empty so the
        /// fresh-Thinking-block path produces minimal JSON.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ReasoningContentItem>,
        /// Echo-back signature for multi-turn reasoning replay. Empty
        /// string when the canonical Thinking block lacks a signature
        /// (e.g. fresh first-turn requests); the server treats empty as
        /// "no prior reasoning to re-inject".
        encrypted_content: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: FunctionCallOutputBody,
    },
}

/// The `output` field of a `function_call_output` item. When all tool
/// result parts are plain text the body collapses to a flat string
/// (codex parity). When any part is non-text (e.g. an image returned by
/// a visual tool) the body becomes an array of typed content items.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum FunctionCallOutputBody {
    /// All parts were plain text; concatenated with newlines (most
    /// common case; avoids wrapping overhead for simple tool results).
    Text(String),
    /// At least one part is non-text; every part is represented as a
    /// typed item so the server can handle the mixed payload.
    Items(Vec<FunctionCallOutputContentItem>),
}

/// One item inside a `FunctionCallOutputBody::Items` array.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum FunctionCallOutputContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// One block inside `Message.content`. Discriminated by `type`:
/// `input_text` for user / system content, `output_text` for assistant
/// content, `input_image` for user image parts (base64 data URL or
/// external URL, forwarded verbatim in the `image_url` field).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesContentItem {
    InputText {
        text: String,
    },
    OutputText {
        text: String,
    },
    /// Image block on a user message. `image_url` is either a
    /// `data:<media_type>;base64,<data>` URI (for base64 sources) or an
    /// https URL (for url sources). `detail` is forwarded verbatim when
    /// present; "auto" is the Responses API default and is omitted rather
    /// than serialized to keep the wire shape minimal.
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// File block on a user message. Carries either inline base64
    /// (`file_data` as a `data:<mime>;base64,<...>` URI) or a reference
    /// to a previously-uploaded file (`file_id`), plus an optional
    /// `filename`. All payload fields are optional except the `type`
    /// tag; absent fields are omitted so the wire shape stays minimal.
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
}

/// One summary block on a `Reasoning` input item. Mirrors the codex
/// reasoning replay shape: an array of `{type: "summary_text", text}`
/// entries. Empty `text` is permitted for replay purposes.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningSummaryItem {
    SummaryText { text: String },
}

/// One content block on a `Reasoning` input item (the inner `content`
/// array, sibling to `summary`). Codex's `ReasoningItemContent` carries
/// both `reasoning_text` and the plain `text` alias; the egress emits
/// `reasoning_text` for byte-stable replay since that's the spelling
/// codex itself sends and a few model variants strict-match on it.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningContentItem {
    ReasoningText { text: String },
    ReasoningEncrypted { encrypted_content: String },
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// One entry inside `tools`. `Function` carries the canonical custom
/// tool shape; `Other` is the forward-compat catchall for
/// `ToolDef::Other` values that the egress passes through verbatim
/// (Anthropic builtins / future shapes).
///
/// Wire shape: the chatgpt-oauth backend rejects the chat-completions
/// nested shape `{type,function:{name,...}}` with:
///   "Missing required parameter: 'tools[0].name'"
/// and requires the flat Responses shape:
///   {type:"function", name:"X", description:"...", parameters:{}}
/// Smoke-confirmed 2026-05-12.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ResponsesTool {
    /// Flat Responses-shape function tool. All fields are top-level
    /// (no nested `function` object). The chat-completions nested shape
    /// (`{type,function:{name,...}}`) 400s on the codex backend.
    Function {
        #[serde(rename = "type")]
        kind: ResponsesFunctionTag,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        parameters: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
    Other(Value),
}

/// The single accepted discriminant on `ResponsesTool::Function`. Kept
/// as a one-variant enum (rather than a hardcoded string) so a future
/// rename surfaces as a compile error.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponsesFunctionTag {
    Function,
}

// ---------------------------------------------------------------------------
// Reasoning + text controls
// ---------------------------------------------------------------------------

/// Reasoning controls on the Responses API. Mirrors codex's
/// `Reasoning` struct: `effort` is the canonical knob, `summary` is a
/// constant `"auto"` so the server emits summary deltas back to the
/// client. routectl maps `req.reasoning.effort` directly into this.
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
}

/// Free-form passthrough for the `text` field on the Responses API.
/// codex models this as `TextControls { verbosity, format }` but the
/// canonical hub stores it as a raw `Value` inside `provider_extras`,
/// so we expose the same shape and forward operator-supplied content
/// verbatim.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct TextControls {
    pub(crate) inner: Value,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_input_item_emits_type_message() {
        // Arrange
        let item = ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ResponsesContentItem::InputText {
                text: "hi".to_string(),
            }],
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            })
        );
    }

    #[test]
    fn reasoning_input_item_emits_summary_and_encrypted_content() {
        // Arrange
        let item = ResponseInputItem::Reasoning {
            id: None,
            summary: vec![ReasoningSummaryItem::SummaryText {
                text: "step one".to_string(),
            }],
            content: Vec::new(),
            encrypted_content: "sig-abc".to_string(),
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert: id + content omitted because skip_serializing_if.
        assert_eq!(
            v,
            json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "step one"}],
                "encrypted_content": "sig-abc"
            })
        );
    }

    #[test]
    fn reasoning_input_item_with_id_and_content_serializes_full_shape() {
        // Arrange
        let item = ResponseInputItem::Reasoning {
            id: Some("rs_1".to_string()),
            summary: vec![ReasoningSummaryItem::SummaryText {
                text: "consider".to_string(),
            }],
            content: vec![ReasoningContentItem::ReasoningText {
                text: "detail".to_string(),
            }],
            encrypted_content: "SIG".to_string(),
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "consider"}],
                "content": [{"type": "reasoning_text", "text": "detail"}],
                "encrypted_content": "SIG"
            })
        );
    }

    #[test]
    fn function_call_input_item_emits_call_id_and_arguments() {
        // Arrange
        let item = ResponseInputItem::FunctionCall {
            call_id: "call_1".into(),
            name: "calc".into(),
            arguments: "{\"a\":1}".into(),
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "calc",
                "arguments": "{\"a\":1}"
            })
        );
    }

    #[test]
    fn function_call_output_item_text_body_emits_flat_string() {
        // Arrange
        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: FunctionCallOutputBody::Text("42".into()),
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert: output is a plain string when all parts are text.
        assert_eq!(
            v,
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "42"
            })
        );
    }

    #[test]
    fn function_call_output_item_items_body_emits_array() {
        // Arrange
        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call_2".into(),
            output: FunctionCallOutputBody::Items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "caption".into(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".into(),
                    detail: None,
                },
            ]),
        };

        // Act
        let v = serde_json::to_value(&item).unwrap();

        // Assert: output is an array of typed items.
        assert_eq!(
            v,
            json!({
                "type": "function_call_output",
                "call_id": "call_2",
                "output": [
                    {"type": "input_text", "text": "caption"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]
            })
        );
    }

    #[test]
    fn responses_tool_function_serializes_flat_shape() {
        // Arrange -- flat Responses shape (NOT the nested chat-completions shape).
        // The chatgpt-oauth backend 400s with "Missing required parameter:
        // 'tools[0].name'" on the nested {"type":"function","function":{...}}
        // shape; the flat shape is accepted (smoke 2026-05-12).
        let tool = ResponsesTool::Function {
            kind: ResponsesFunctionTag::Function,
            name: "calc".into(),
            description: Some("do math".into()),
            parameters: json!({"type": "object"}),
            strict: Some(true),
        };

        // Act
        let v = serde_json::to_value(&tool).unwrap();

        // Assert: flat shape -- name/description/parameters/strict are
        // top-level fields, NOT nested under a "function" key.
        assert_eq!(
            v,
            json!({
                "type": "function",
                "name": "calc",
                "description": "do math",
                "parameters": {"type": "object"},
                "strict": true
            })
        );
    }

    #[test]
    fn reasoning_controls_skip_none_fields() {
        // Arrange
        let r = ResponsesReasoning {
            effort: Some("high".into()),
            summary: None,
        };

        // Act
        let v = serde_json::to_value(&r).unwrap();

        // Assert
        assert_eq!(v, json!({"effort": "high"}));
    }

    #[test]
    fn text_controls_passthrough_inner_value() {
        // Arrange
        let tc = TextControls {
            inner: json!({"verbosity": "high"}),
        };

        // Act
        let v = serde_json::to_value(&tc).unwrap();

        // Assert
        assert_eq!(v, json!({"verbosity": "high"}));
    }
}
