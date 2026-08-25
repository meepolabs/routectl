//! Anthropic Messages API wire types.
//!
//! These are internal to the provider; consumers only see routectl-core types.
//!
//! v0.4.0: extended to round-trip cache_control and forward-compat block
//! types. ContentBlock gains Image / Document / Other; AnthropicTool
//! splits into Custom / Builtin; AnthropicRequest grows top-level
//! cache_control + anthropic_beta + structured system. Usage and
//! SseDeltaUsage carry the cache_creation / cache_read tallies.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use routectl_core::CacheControl;

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<AnthropicMessage>,
    pub(crate) max_tokens: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<AnthropicSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<ThinkingConfig>,
    /// Top-level effort knob for the Opus 4.7+ adaptive thinking path.
    /// Only emitted alongside `ThinkingConfig::Adaptive`. See
    /// `OutputConfig` and `ThinkingConfig::Adaptive` for the wire
    /// shape; the request normalizer decides whether to populate this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<Value>,

    /// Top-level cache breakpoint (auto-cache mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cache_control: Option<CacheControl>,

    /// Body-level beta flags. Bedrock keeps the body shape (its
    /// validator reads from here), but the direct
    /// `api.anthropic.com` egress strips this from the wire body
    /// before sending and emits the `anthropic-beta` HTTP header
    /// instead. The strip is inlined in `complete()` and `stream()`
    /// via `obj.remove("anthropic_beta")` in `mod.rs`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) anthropic_beta: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub(crate) role: AnthropicRole,
    pub(crate) content: AnthropicContent,
}

/// Wire role vocabulary of `messages[]`. `System` is a real Anthropic
/// Messages API role: a mid-conversation system turn must precede an
/// `assistant` turn or end the array, and support is model-gated.
///
/// Deliberately NOT `#[non_exhaustive]`: an exhaustive match on a wire
/// vocabulary is the compile-time forcing function that makes a new role
/// impossible to ignore at every site that dispatches on one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicRole {
    User,
    Assistant,
    System,
}

/// Content is either a plain string (outgoing user messages) or an array of
/// content blocks (assistant turns that may carry thinking blocks).
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Top-level system field. Anthropic accepts a flat string or an array
/// of typed text blocks with per-block cache_control.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicSystem {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_control: Option<CacheControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) citations: Option<Value>,
}

/// One content block in an Anthropic message.
///
/// `Other` captures forward-compat block types (server_tool_use,
/// web_search_tool_result, code_execution_tool_result, ...) so the
/// Anthropic-in / Anthropic-out path keeps working when Anthropic
/// ships a new block type before routectl knows about it.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text {
        text: String,
        cache_control: Option<CacheControl>,
        citations: Option<Value>,
    },
    Image {
        source: Value,
        cache_control: Option<CacheControl>,
    },
    Document {
        source: Value,
        cache_control: Option<CacheControl>,
        title: Option<String>,
        citations: Option<Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
        cache_control: Option<CacheControl>,
    },
    RedactedThinking {
        data: String,
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        cache_control: Option<CacheControl>,
        is_error: Option<bool>,
    },
    Other {
        type_tag: String,
        cache_control: Option<CacheControl>,
        extras: Map<String, Value>,
    },
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // Build a JSON Value with the right `type` tag, then serialize
        // it. Easier than a manual SerializeStruct dance per variant.
        use serde_json::json;
        let v: Value = match self {
            Self::Text {
                text,
                cache_control,
                citations,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("text".into()));
                obj.insert("text".into(), Value::String(text.clone()));
                if let Some(c) = citations {
                    obj.insert("citations".into(), c.clone());
                }
                merge_cc(Value::Object(obj), cache_control)
            }
            Self::Image {
                source,
                cache_control,
            } => merge_cc(json!({"type": "image", "source": source}), cache_control),
            Self::Document {
                source,
                cache_control,
                title,
                citations,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("document".into()));
                obj.insert("source".into(), source.clone());
                if let Some(t) = title {
                    obj.insert("title".into(), Value::String(t.clone()));
                }
                if let Some(c) = citations {
                    obj.insert("citations".into(), c.clone());
                }
                merge_cc(Value::Object(obj), cache_control)
            }
            Self::Thinking {
                thinking,
                signature,
                cache_control,
            } => merge_cc(
                json!({"type": "thinking", "thinking": thinking, "signature": signature}),
                cache_control,
            ),
            Self::RedactedThinking {
                data,
                cache_control,
            } => merge_cc(
                json!({"type": "redacted_thinking", "data": data}),
                cache_control,
            ),
            Self::ToolUse {
                id,
                name,
                input,
                cache_control,
            } => merge_cc(
                json!({"type": "tool_use", "id": id, "name": name, "input": input}),
                cache_control,
            ),
            Self::ToolResult {
                tool_use_id,
                content,
                cache_control,
                is_error,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("tool_result".into()));
                obj.insert("tool_use_id".into(), Value::String(tool_use_id.clone()));
                obj.insert("content".into(), content.clone());
                if let Some(e) = is_error {
                    obj.insert("is_error".into(), Value::Bool(*e));
                }
                merge_cc(Value::Object(obj), cache_control)
            }
            Self::Other {
                type_tag,
                cache_control,
                extras,
            } => {
                let mut obj = extras.clone();
                obj.insert("type".into(), Value::String(type_tag.clone()));
                merge_cc(Value::Object(obj), cache_control)
            }
        };
        v.serialize(ser)
    }
}

fn merge_cc(mut v: Value, cc: &Option<CacheControl>) -> Value {
    if let (Some(cc), Some(obj)) = (cc.as_ref(), v.as_object_mut())
        && let Ok(cc_v) = serde_json::to_value(cc)
    {
        obj.insert("cache_control".into(), cc_v);
    }
    v
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let mut v: Value = Value::deserialize(de)?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("expected object"))?;

        let type_tag = obj
            .remove("type")
            .and_then(|t| t.as_str().map(std::string::ToString::to_string))
            .ok_or_else(|| D::Error::custom("missing `type` field"))?;
        let cache_control = obj
            .remove("cache_control")
            .map(serde_json::from_value::<CacheControl>)
            .transpose()
            .map_err(D::Error::custom)?;

        match type_tag.as_str() {
            "text" => Ok(Self::Text {
                text: take_str(obj, "text").map_err(D::Error::custom)?,
                cache_control,
                citations: obj.remove("citations"),
            }),
            "image" => Ok(Self::Image {
                source: obj
                    .remove("source")
                    .ok_or_else(|| D::Error::custom("image missing source"))?,
                cache_control,
            }),
            "document" => {
                let source = obj
                    .remove("source")
                    .ok_or_else(|| D::Error::custom("document missing source"))?;
                let title = obj
                    .remove("title")
                    .and_then(|v| v.as_str().map(std::string::ToString::to_string));
                let citations = obj.remove("citations");
                Ok(Self::Document {
                    source,
                    cache_control,
                    title,
                    citations,
                })
            }
            "thinking" => Ok(Self::Thinking {
                thinking: take_str(obj, "thinking").map_err(D::Error::custom)?,
                signature: take_str(obj, "signature").map_err(D::Error::custom)?,
                cache_control,
            }),
            "redacted_thinking" => Ok(Self::RedactedThinking {
                data: take_str(obj, "data").map_err(D::Error::custom)?,
                cache_control,
            }),
            "tool_use" => Ok(Self::ToolUse {
                id: take_str(obj, "id").map_err(D::Error::custom)?,
                name: take_str(obj, "name").map_err(D::Error::custom)?,
                input: obj
                    .remove("input")
                    .unwrap_or_else(|| Value::Object(Map::new())),
                cache_control,
            }),
            "tool_result" => {
                let tool_use_id = take_str(obj, "tool_use_id").map_err(D::Error::custom)?;
                let content = obj.remove("content").unwrap_or(Value::Null);
                let is_error = obj.remove("is_error").and_then(|v| v.as_bool());
                Ok(Self::ToolResult {
                    tool_use_id,
                    content,
                    cache_control,
                    is_error,
                })
            }
            other => Ok(Self::Other {
                type_tag: other.to_string(),
                cache_control,
                // Anything left in obj is the forward-compat payload.
                extras: std::mem::take(obj),
            }),
        }
    }
}

fn take_str(obj: &mut Map<String, Value>, key: &str) -> Result<String, String> {
    obj.remove(key)
        .and_then(|v| v.as_str().map(std::string::ToString::to_string))
        .ok_or_else(|| format!("missing string field `{key}`"))
}

// ---------------------------------------------------------------------------
// Thinking config
// ---------------------------------------------------------------------------

/// `thinking.display` -- whether Anthropic returns thinking text or an
/// empty (but still signed) thinking block. A closed two-value enum
/// upstream; anything else 400s at the API, so the ingress rejects
/// unknown values rather than forwarding them.
///
/// The default is model-dependent, so an absent `display` must stay
/// absent on the wire: serializing an explicit value the caller never
/// sent would override a newer model's own default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Enabled {
        budget_tokens: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    /// Opus 4.7+ wire shape. The model picks its own budget; the
    /// operator steers via top-level `output_config.effort` (a string
    /// like "low" | "medium" | "high" | "xhigh" | "max"). No
    /// `budget_tokens` field. Older Claude models (4.5/4.6 family)
    /// still want the `Enabled` shape, so this variant only ships
    /// when `req.routectl_internal.supports_adaptive_thinking` is `true`
    /// -- the request normalizer in `request.rs` decides which variant
    /// to emit per-call.
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Disabled,
}

/// Top-level `output_config` field on `AnthropicRequest`. Only emitted
/// alongside `ThinkingConfig::Adaptive`. Anthropic validates the
/// `effort` string -- routectl passes whatever the canonical
/// `req.reasoning.effort` carries through verbatim ("low", "medium",
/// "high", "xhigh", "max", or anything Anthropic adds later).
#[derive(Debug, Serialize, Deserialize)]
pub struct OutputConfig {
    pub(crate) effort: String,
}

// ---------------------------------------------------------------------------
// Tool shapes
// ---------------------------------------------------------------------------

/// Wire-side tool definition. `Custom` is the canonical Anthropic
/// custom tool with cache_control / defer_loading / strict; `Builtin`
/// passes through arbitrary JSON for builtin and forward-compat tool
/// shapes (`bash_*`, `code_execution_*`, `web_search_*`, ...).
#[derive(Debug, Clone)]
pub enum AnthropicTool {
    Custom {
        name: String,
        description: Option<String>,
        input_schema: Value,
        cache_control: Option<CacheControl>,
        defer_loading: Option<bool>,
        strict: Option<bool>,
        type_tag: Option<String>,
    },
    Builtin(Value),
}

impl Serialize for AnthropicTool {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Custom {
                name,
                description,
                input_schema,
                cache_control,
                defer_loading,
                strict,
                type_tag,
            } => {
                let mut obj = serde_json::Map::new();
                if let Some(t) = type_tag {
                    obj.insert("type".into(), Value::String(t.clone()));
                }
                obj.insert("name".into(), Value::String(name.clone()));
                if let Some(d) = description {
                    obj.insert("description".into(), Value::String(d.clone()));
                }
                obj.insert("input_schema".into(), input_schema.clone());
                if let Some(cc) = cache_control
                    && let Ok(cc_v) = serde_json::to_value(cc)
                {
                    obj.insert("cache_control".into(), cc_v);
                }
                if let Some(d) = defer_loading {
                    obj.insert("defer_loading".into(), Value::Bool(*d));
                }
                if let Some(s) = strict {
                    obj.insert("strict".into(), Value::Bool(*s));
                }
                Value::Object(obj).serialize(ser)
            }
            Self::Builtin(v) => v.serialize(ser),
        }
    }
}

// ---------------------------------------------------------------------------
// Response body
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AnthropicResponse {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) content: Vec<ContentBlock>,
    pub(crate) stop_reason: Option<String>,
    /// The matched stop sequence (when `stop_reason == "stop_sequence"`).
    /// Anthropic and Bedrock-Invoke set this on the response when the
    /// upstream hit one of the request's `stop_sequences`. Pulled out
    /// of the flatten-extras catchall so the egress normalizer can
    /// surface it via `Choice.matched_stop_sequence`.
    #[serde(default)]
    pub(crate) stop_sequence: Option<String>,
    pub(crate) usage: Option<AnthropicUsage>,
    /// Forward-compat catchall for any top-level field not in the
    /// canonical Anthropic Messages baseline. Captures recently-added
    /// spec fields like `context_management` (from the
    /// `context-management-2025-06-27` beta) and Bedrock-specific
    /// extensions, so the egress can round-trip them verbatim.
    /// Drop-list of egress-time strips (e.g. `stop_details` on the
    /// Bedrock-only path) lives in the Anthropic ingress's
    /// `render_messages_response`, NOT here -- this struct is the
    /// deserialization seam, not the policy seam.
    #[serde(flatten)]
    pub(crate) extras: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicUsage {
    pub(crate) input_tokens: u32,
    pub(crate) output_tokens: u32,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation: Option<AnthropicCacheCreation>,
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u32>,
    /// Server-side tool invocation counts (e.g. `web_search_requests`).
    /// Pulled out of the flatten-extras catchall so the egress
    /// normalizer can lift it onto the typed canonical
    /// `Usage.server_tool_use` field. Opaque JSON for forward-compat.
    #[serde(default)]
    pub(crate) server_tool_use: Option<Value>,
    /// Forward-compat catchall for `usage` sub-fields routectl
    /// doesn't yet have a typed slot for. Captures `service_tier`
    /// (returned by every Anthropic / Bedrock-Invoke response) and
    /// any future spec additions.
    #[serde(flatten)]
    pub(crate) extras: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicCacheCreation {
    #[serde(default)]
    pub(crate) ephemeral_5m_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) ephemeral_1h_input_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// SSE event shapes
// ---------------------------------------------------------------------------
//
// `SseEvent`, `SseContentBlockStart`, and `SseDelta` (the three
// strict-tagged enums that gain forward-compat `Other(Value)` arms)
// live in the sibling `types_sse` module so this file stays under the
// project's 800-LOC ceiling. They re-export from here so consumers
// keep importing via `super::types::SseEvent`.
pub use super::types_sse::{SseContentBlockStart, SseDelta, SseEvent};

#[derive(Debug, Deserialize)]
pub struct SseMessage {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
pub struct SseMessageDelta {
    pub(crate) stop_reason: Option<String>,
    /// Matched stop sequence on the `message_delta.delta` payload.
    /// Anthropic streaming emits this alongside `stop_reason` when
    /// the upstream stopped because of a matched sequence. None
    /// otherwise. Lifted into `ChunkChoice.matched_stop_sequence`
    /// when present so the Anthropic ingress can render the wire
    /// `stop_sequence` field on `message_delta`.
    #[serde(default)]
    pub(crate) stop_sequence: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SseDeltaUsage {
    /// Real Anthropic and routectl's own Anthropic ingress render
    /// `input_tokens` in `message_delta.usage` (mirroring the value
    /// from `message_start.usage` with the final post-cache count).
    /// Optional because some upstream variants only emit it on
    /// `message_start`.
    #[serde(default)]
    pub(crate) input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation: Option<AnthropicCacheCreation>,
    /// Server-side tool invocation counts streamed on
    /// `message_delta.usage`. Opaque JSON lifted onto the canonical
    /// chunk usage's `server_tool_use` field.
    #[serde(default)]
    pub(crate) server_tool_use: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_block_serializes_with_cache_control() {
        let b = ContentBlock::Text {
            text: "hi".into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["cache_control"]["type"], "ephemeral");
        assert_eq!(v["cache_control"]["ttl"], "5m");
    }

    #[test]
    fn image_block_round_trips() {
        let v = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&b, ContentBlock::Image { .. }));
        let back = serde_json::to_value(&b).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn unknown_block_type_falls_to_other_and_round_trips() {
        let v = json!({
            "type": "server_tool_use",
            "id": "srvtu_01",
            "name": "web_search",
            "input": {"query": "rust"},
            "cache_control": {"type": "ephemeral"}
        });
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        if let ContentBlock::Other {
            type_tag,
            cache_control,
            extras,
        } = &b
        {
            assert_eq!(type_tag, "server_tool_use");
            assert!(cache_control.is_some());
            assert_eq!(extras["id"], "srvtu_01");
        } else {
            panic!("expected Other");
        }
        let back = serde_json::to_value(&b).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn document_block_round_trips() {
        let v = json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "AAAA"},
            "title": "spec.pdf",
            "cache_control": {"type": "ephemeral"}
        });
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&b, ContentBlock::Document { .. }));
        let back = serde_json::to_value(&b).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn anthropic_request_serializes_anthropic_beta_when_non_empty() {
        let req = AnthropicRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![],
            max_tokens: 100,
            system: None,
            thinking: None,
            output_config: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            cache_control: None,
            anthropic_beta: vec!["context-1m-2025-08-07".into()],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["anthropic_beta"], json!(["context-1m-2025-08-07"]));
    }

    #[test]
    fn anthropic_request_skips_empty_anthropic_beta() {
        let req = AnthropicRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![],
            max_tokens: 100,
            system: None,
            thinking: None,
            output_config: None,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            cache_control: None,
            anthropic_beta: vec![],
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("anthropic_beta"));
    }

    #[test]
    fn custom_tool_serializes_with_cache_control() {
        let t = AnthropicTool::Custom {
            name: "calculator".into(),
            description: Some("do math".into()),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_1h()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "calculator");
        assert_eq!(v["cache_control"]["ttl"], "1h");
        assert!(v.get("type").is_none()); // type_tag absent
    }

    #[test]
    fn builtin_tool_round_trips() {
        let raw = json!({
            "type": "bash_20250124",
            "name": "bash",
            "cache_control": {"type": "ephemeral"}
        });
        let t = AnthropicTool::Builtin(raw.clone());
        assert_eq!(serde_json::to_value(&t).unwrap(), raw);
    }

    #[test]
    fn sse_delta_usage_parses_cache_fields() {
        let v = json!({
            "output_tokens": 42,
            "cache_creation_input_tokens": 1024,
            "cache_read_input_tokens": 4096,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 512,
                "ephemeral_1h_input_tokens": 512
            }
        });
        let u: SseDeltaUsage = serde_json::from_value(v).unwrap();
        assert_eq!(u.output_tokens, Some(42));
        assert_eq!(u.cache_creation_input_tokens, Some(1024));
        assert_eq!(u.cache_read_input_tokens, Some(4096));
        let cc = u.cache_creation.as_ref().unwrap();
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(512));
    }
}
