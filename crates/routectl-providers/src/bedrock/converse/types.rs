//! AWS Bedrock Converse wire types.
//!
//! Strictly-typed serde structs mirroring the AWS Converse request body.
//! Field names are AWS camelCase (`maxTokens`, `topP`, `stopSequences`,
//! `toolUseId`, `inputSchema`, ...). Optionals skip when None so the
//! emitted JSON omits absent fields rather than rendering `null`.
//!
//! Response- and streaming-side wire types live in `response_types.rs`;
//! the file split keeps each side under the 800-line cap and lets
//! `Deserialize`-only types stay separate from the `Serialize`-only
//! request shapes.
//!
//! Internal to the provider; consumers only see routectl-core types.

use serde::Serialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

/// Top-level Converse request body. The model id is in the URL path
/// (`/model/{modelId}/converse`), not the body, so it is intentionally
/// absent here. `additionalModelResponseFieldPaths` opts the response
/// bag into surfacing `/stop_sequence` so the canonical
/// `matched_stop_sequence` round-trips through Converse identically to
/// the Bedrock-Invoke path.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseRequest {
    pub(crate) messages: Vec<ConverseMessage>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<Vec<ConverseSystemBlock>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inference_config: Option<InferenceConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_config: Option<ToolConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) additional_model_request_fields: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) additional_model_response_field_paths: Option<Vec<String>>,
}

/// Inference parameters supported by every Converse-eligible model.
/// Model-specific knobs (top_k, anthropic_beta, thinking) ride in
/// `additionalModelRequestFields`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_sequences: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// One entry inside `system: [...]`. AWS models the union as a single-key
/// object: `{text: "..."}` for prompt blocks, `{cachePoint: {type:
/// "default"}}` for inline cache breakpoints between system blocks.
/// `guardContent` exists in the AWS schema too but routectl does not
/// currently populate it (covered by the forward-compat catchall in
/// `additional_model_request_fields` if an operator asks for it).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConverseSystemBlock {
    #[serde(rename = "text")]
    Text(String),
    #[serde(rename = "cachePoint")]
    CachePoint(CachePoint),
}

/// AWS cache breakpoint marker. `type` is required and currently the only
/// valid value is `"default"`. `ttl` is optional and accepts `"5m"` |
/// `"1h"` (extended-TTL caching).
#[derive(Debug, Serialize)]
pub struct CachePoint {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ttl: Option<String>,
}

impl CachePoint {
    pub(crate) fn default_with_ttl(ttl: Option<String>) -> Self {
        Self {
            kind: "default".to_string(),
            ttl,
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ConverseMessage {
    /// `"user"` or `"assistant"`. Converse rejects anything else
    /// (system goes in the top-level `system` array; tool results ride
    /// inside a user-role `toolResult` content block).
    pub(crate) role: String,
    pub(crate) content: Vec<ConverseContentBlock>,
}

/// One content block inside a message. Single-key untagged union: each
/// variant emits exactly one top-level key (`text` / `image` / `toolUse`
/// / `toolResult` / `cachePoint` / `document` / `reasoningContent`).
/// Forward-compat: unknown content shapes from canonical fall through
/// `ConverseContentBlock::Other` which serializes whatever
/// serde_json::Value the operator supplied.
#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(dead_code)] // Other(Value) is the forward-compat passthrough variant
pub enum ConverseContentBlock {
    Text {
        text: String,
    },
    Image {
        image: ConverseImage,
    },
    Document {
        document: ConverseDocument,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: ConverseToolUse,
    },
    ToolResult {
        #[serde(rename = "toolResult")]
        tool_result: ConverseToolResult,
    },
    CachePoint {
        #[serde(rename = "cachePoint")]
        cache_point: CachePoint,
    },
    /// Reasoning content block. Required for multi-turn replay against
    /// thinking-enabled Claude on Converse: the prior assistant's
    /// reasoning (text + signature, or redacted base64 bytes) must echo
    /// back verbatim. AWS schema mirrors the response side -- a single
    /// `reasoningContent` key wrapping a union of `reasoningText` (text +
    /// optional signature) or `redactedContent` (base64 string). The
    /// signature is "Required: No" per AWS docs, but Anthropic 400s on
    /// replay without it; the canonical -> Converse translator surfaces
    /// the missing signature locally as a clean NormalizeRequest error.
    ReasoningContent {
        #[serde(rename = "reasoningContent")]
        reasoning_content: ConverseRequestReasoningBlock,
    },
    /// Forward-compat passthrough -- caller-supplied raw JSON. Used when
    /// an operator's `provider_extras` contains a future block type
    /// (citationsContent, video, ...) routectl doesn't model yet. The
    /// Value is serialized as-is.
    Other(Value),
}

/// Request-side reasoning block. AWS models this as a tagged union:
/// either `reasoningText: {text, signature?}` for visible chain-of-thought
/// or `redactedContent: <base64-string>` for safety-redacted bytes.
/// Untagged + skip-when-None on each arm so the wire form emits exactly
/// one top-level key per AWS's "only one member can be specified" rule.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ConverseRequestReasoningBlock {
    ReasoningText {
        #[serde(rename = "reasoningText")]
        reasoning_text: ConverseRequestReasoningText,
    },
    RedactedContent {
        #[serde(rename = "redactedContent")]
        redacted_content: String,
    },
}

/// AWS `ReasoningTextBlock`. `text` is required; `signature` is required
/// in practice for replay even though AWS docs mark it optional, so the
/// translator errors on absent signature rather than serializing the
/// field as `null`.
#[derive(Debug, Serialize)]
pub struct ConverseRequestReasoningText {
    pub(crate) text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) signature: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConverseImage {
    /// `"png" | "jpeg" | "gif" | "webp"`. AWS validates this strictly.
    pub(crate) format: String,
    pub(crate) source: ConverseImageSource,
}

#[derive(Debug, Serialize)]
pub struct ConverseImageSource {
    /// Base64-encoded image bytes. AWS reference doc says "if you use an
    /// AWS SDK, you don't need to encode the image bytes in base64"; we
    /// don't use the SDK and we send JSON, so the wire form IS base64.
    pub(crate) bytes: String,
}

/// AWS Converse `document` content block. AWS schema:
/// `{format: "pdf"|"csv"|"doc"|"docx"|"xls"|"xlsx"|"html"|"txt"|"md",
///   name: "...",
///   source: {bytes: <base64>}}`. The `name` field is required by AWS,
/// validated against `^[a-zA-Z0-9-()[\]_ ]{1,200}$`. We emit a
/// best-effort name from the canonical Document `title` field; when
/// absent we use a generic placeholder so AWS doesn't reject the block
/// outright.
#[derive(Debug, Serialize)]
pub struct ConverseDocument {
    /// Document MIME format: pdf, csv, doc, docx, xls, xlsx, html, txt, md.
    pub(crate) format: String,
    /// Filename for the document. Required by AWS.
    pub(crate) name: String,
    pub(crate) source: ConverseDocumentSource,
}

#[derive(Debug, Serialize)]
pub struct ConverseDocumentSource {
    /// Base64-encoded document bytes.
    pub(crate) bytes: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseToolUse {
    pub(crate) tool_use_id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseToolResult {
    pub(crate) tool_use_id: String,
    pub(crate) content: Vec<ConverseToolResultContent>,
    /// `"success" | "error"`. Only honored by Nova and Claude 3+.
    /// Optional in the wire schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
}

/// One block inside a `toolResult.content` array. Single-key untagged
/// union mirroring the AWS schema. `text` is the fast path for stringly
/// tool results; `json` carries structured tool output verbatim;
/// `image` and `document` ride along when the tool returns multimodal
/// data.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ConverseToolResultContent {
    Text { text: String },
    Json { json: Value },
    Image { image: ConverseImage },
    Document { document: Value },
}

// ---------------------------------------------------------------------------
// Tool config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    pub(crate) tools: Vec<ConverseToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<ConverseToolChoice>,
}

/// One tool entry. Single-key union: `{toolSpec}` for declared tools,
/// `{cachePoint}` for cache breakpoints between tools (Bedrock honors
/// these between tool definitions just like between system blocks).
/// AWS also accepts `{systemTool}` for vendor-defined builtins; routectl
/// currently exposes that surface only via raw `provider_extras` because
/// no canonical caller emits it yet.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ConverseToolDef {
    Spec {
        #[serde(rename = "toolSpec")]
        tool_spec: ConverseToolSpec,
    },
    CachePoint {
        #[serde(rename = "cachePoint")]
        cache_point: CachePoint,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseToolSpec {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) input_schema: ConverseInputSchema,
}

#[derive(Debug, Serialize)]
pub struct ConverseInputSchema {
    pub(crate) json: Value,
}

/// AWS toolChoice union: `{auto:{}}`, `{any:{}}`, `{tool:{name}}`. None
/// of the variants carry shape beyond `name` for `Tool`. Empty objects
/// for `auto` / `any` matter -- AWS rejects `null` and rejects bare
/// strings.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ConverseToolChoice {
    Auto { auto: EmptyObject },
    Any { any: EmptyObject },
    Tool { tool: ConverseSpecificTool },
}

/// Empty struct that serializes to `{}`. AWS requires the empty-object
/// shape on the auto / any variants.
#[derive(Debug, Serialize)]
pub struct EmptyObject {}

#[derive(Debug, Serialize)]
pub struct ConverseSpecificTool {
    pub(crate) name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inference_config_skips_none_fields() {
        // Arrange
        let cfg = InferenceConfig {
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            stop_sequences: None,
        };

        // Act
        let v = serde_json::to_value(&cfg).unwrap();

        // Assert
        assert_eq!(v, json!({"maxTokens": 1024}));
    }

    #[test]
    fn system_block_text_serializes_with_text_key() {
        // Arrange
        let block = ConverseSystemBlock::Text("be helpful".to_string());

        // Act
        let v = serde_json::to_value(&block).unwrap();

        // Assert
        assert_eq!(v, json!({"text": "be helpful"}));
    }

    #[test]
    fn system_block_cache_point_serializes_with_cache_point_key() {
        // Arrange
        let block = ConverseSystemBlock::CachePoint(CachePoint {
            kind: "default".to_string(),
            ttl: None,
        });

        // Act
        let v = serde_json::to_value(&block).unwrap();

        // Assert
        assert_eq!(v, json!({"cachePoint": {"type": "default"}}));
    }

    #[test]
    fn cache_point_with_ttl_serializes_with_ttl_field() {
        // Arrange
        let cp = CachePoint {
            kind: "default".to_string(),
            ttl: Some("1h".to_string()),
        };

        // Act
        let v = serde_json::to_value(&cp).unwrap();

        // Assert
        assert_eq!(v, json!({"type": "default", "ttl": "1h"}));
    }

    #[test]
    fn tool_choice_auto_serializes_to_auto_with_empty_object() {
        // Arrange
        let tc = ConverseToolChoice::Auto {
            auto: EmptyObject {},
        };

        // Act
        let v = serde_json::to_value(&tc).unwrap();

        // Assert
        assert_eq!(v, json!({"auto": {}}));
    }

    #[test]
    fn tool_choice_any_serializes_to_any_with_empty_object() {
        // Arrange
        let tc = ConverseToolChoice::Any {
            any: EmptyObject {},
        };

        // Act
        let v = serde_json::to_value(&tc).unwrap();

        // Assert
        assert_eq!(v, json!({"any": {}}));
    }

    #[test]
    fn tool_choice_specific_serializes_to_tool_with_name() {
        // Arrange
        let tc = ConverseToolChoice::Tool {
            tool: ConverseSpecificTool {
                name: "get_weather".to_string(),
            },
        };

        // Act
        let v = serde_json::to_value(&tc).unwrap();

        // Assert
        assert_eq!(v, json!({"tool": {"name": "get_weather"}}));
    }

    #[test]
    fn tool_def_spec_serializes_with_tool_spec_key() {
        // Arrange
        let td = ConverseToolDef::Spec {
            tool_spec: ConverseToolSpec {
                name: "get_weather".to_string(),
                description: Some("look up weather".to_string()),
                input_schema: ConverseInputSchema {
                    json: json!({"type": "object"}),
                },
            },
        };

        // Act
        let v = serde_json::to_value(&td).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "toolSpec": {
                    "name": "get_weather",
                    "description": "look up weather",
                    "inputSchema": {"json": {"type": "object"}}
                }
            })
        );
    }

    #[test]
    fn content_block_text_serializes_with_text_key() {
        // Arrange
        let cb = ConverseContentBlock::Text {
            text: "hello".to_string(),
        };

        // Act
        let v = serde_json::to_value(&cb).unwrap();

        // Assert
        assert_eq!(v, json!({"text": "hello"}));
    }

    #[test]
    fn content_block_tool_use_uses_camel_case_fields() {
        // Arrange
        let cb = ConverseContentBlock::ToolUse {
            tool_use: ConverseToolUse {
                tool_use_id: "tu_123".to_string(),
                name: "calc".to_string(),
                input: json!({"x": 1}),
            },
        };

        // Act
        let v = serde_json::to_value(&cb).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "toolUse": {
                    "toolUseId": "tu_123",
                    "name": "calc",
                    "input": {"x": 1}
                }
            })
        );
    }

    #[test]
    fn content_block_image_emits_format_and_base64_bytes() {
        // Arrange
        let cb = ConverseContentBlock::Image {
            image: ConverseImage {
                format: "png".to_string(),
                source: ConverseImageSource {
                    bytes: "AAAA".to_string(),
                },
            },
        };

        // Act
        let v = serde_json::to_value(&cb).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "image": {
                    "format": "png",
                    "source": {"bytes": "AAAA"}
                }
            })
        );
    }

    #[test]
    fn tool_result_content_text_emits_text_key() {
        // Arrange
        let trc = ConverseToolResultContent::Text {
            text: "result".to_string(),
        };

        // Act
        let v = serde_json::to_value(&trc).unwrap();

        // Assert
        assert_eq!(v, json!({"text": "result"}));
    }

    #[test]
    fn reasoning_content_text_serializes_with_text_and_signature() {
        // Arrange: Anthropic-on-Converse multi-turn replay shape.
        let block = ConverseContentBlock::ReasoningContent {
            reasoning_content: ConverseRequestReasoningBlock::ReasoningText {
                reasoning_text: ConverseRequestReasoningText {
                    text: "step 1".to_string(),
                    signature: Some("sig123".to_string()),
                },
            },
        };

        // Act
        let v = serde_json::to_value(&block).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "reasoningContent": {
                    "reasoningText": {"text": "step 1", "signature": "sig123"}
                }
            })
        );
    }

    #[test]
    fn reasoning_content_redacted_serializes_with_base64_string() {
        // Arrange
        let block = ConverseContentBlock::ReasoningContent {
            reasoning_content: ConverseRequestReasoningBlock::RedactedContent {
                redacted_content: "AAECAwQF".to_string(),
            },
        };

        // Act
        let v = serde_json::to_value(&block).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({"reasoningContent": {"redactedContent": "AAECAwQF"}})
        );
    }
}
