//! Typed content blocks for `MessageContent::Parts`.
//!
//! The hub stores message content blocks in a typed form so the Anthropic
//! ingress and Anthropic / Bedrock-Invoke egresses can preserve every
//! Anthropic-specific field (cache_control, signature, document source,
//! tool blocks) without losing information. Unknown block types fall to
//! `ContentPart::Other`, which carries the original `type` discriminant
//! and arbitrary fields verbatim. Forward-compatibility is not aspirational:
//! a new Anthropic block type ships through routectl with zero code edits
//! on the all-Anthropic path.
//!
//! Layout:
//! - `Known(KnownContentPart)` -- typed Anthropic and OpenAI block shapes.
//! - `Other { type, cache_control, extras }` -- catchall with named
//!   `cache_control` so cache breakpoints inside future block types still
//!   show up to the validator.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::cache_control::CacheControl;

/// One block inside a message's `content: [...]` array. See module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContentPart {
    Known(KnownContentPart),
    /// Forward-compat catchall. Captures the original `type` discriminant,
    /// any `cache_control` marker, and all other fields verbatim. The
    /// Anthropic egress re-emits this verbatim; non-Anthropic egresses
    /// drop with a `tracing::warn!`.
    Other {
        #[serde(rename = "type")]
        type_tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(flatten)]
        extras: Map<String, Value>,
    },
}

/// Typed content blocks the hub knows how to introspect. The Anthropic
/// names (`text`, `image`, `document`, `tool_use`, `tool_result`,
/// `thinking`, `redacted_thinking`) are the canonical wire form. The
/// OpenAI-specific `image_url` block is also a known shape because
/// existing OpenAI clients rely on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownContentPart {
    /// Plain text block. The most common content shape; carried by
    /// every wire dialect routectl translates.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Anthropic-shape image block. `source` is `{type: "base64",
    /// media_type, data}` or `{type: "url", url}` depending on Anthropic
    /// API version.
    Image {
        source: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// OpenAI-shape image block (`{type: "image_url", image_url: {url,
    /// detail}}`). Kept distinct from `Image` so each ingress emits its
    /// native shape and round-tripping is byte-stable.
    ImageUrl {
        image_url: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// OpenAI-shape file block (`{type: "file", file: {filename,
    /// file_data}}` for a base64 upload, or `{type: "file", file:
    /// {file_id}}` for a previously-uploaded reference). Kept distinct
    /// from `Document` so the OpenAI-compat egress round-trips it
    /// byte-stable in its native shape; the Anthropic / Bedrock egresses
    /// translate the base64 `file_data` form into an Anthropic document
    /// block. `file` carries the raw nested object verbatim, mirroring
    /// how `ImageUrl` carries `image_url`.
    File {
        file: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Anthropic-shape document block. `source` is the document payload
    /// (base64 / url / text); `title` and `citations` carry retrieval
    /// metadata for the model. OpenAI-shape inputs do not have an
    /// equivalent and lift `title` / `citations` only on Anthropic
    /// egresses.
    Document {
        source: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Tool-call request block emitted by an assistant turn. `id`
    /// matches the corresponding `ToolResult.tool_use_id` on the
    /// follow-up user turn. `input` is the JSON-encoded args object the
    /// model decided to pass to the tool.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Tool-call response block emitted by a user turn carrying the
    /// tool's output back to the model. `tool_use_id` matches the
    /// corresponding `ToolUse.id` from the prior assistant turn.
    /// `is_error` flags structured tool errors so the model can
    /// disambiguate from successful results.
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Extended thinking block. `signature` is mandatory for multi-turn
    /// tool-use continuity on Anthropic and is preserved verbatim.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Redacted-by-Anthropic thinking block. The `data` field is an
    /// opaque encrypted payload AWS / Anthropic substituted for content
    /// their safety system flagged. Preserve verbatim on round-trip;
    /// never log the contents.
    RedactedThinking { data: String },
}

impl ContentPart {
    /// `cache_control` if the block carries one. Used by the validator and
    /// by egresses that need to count breakpoints.
    pub fn cache_control(&self) -> Option<&CacheControl> {
        match self {
            ContentPart::Known(k) => k.cache_control(),
            ContentPart::Other { cache_control, .. } => cache_control.as_ref(),
        }
    }

    /// Wire-shape `type` discriminant (e.g. `"text"`, `"image"`,
    /// `"server_tool_use"` for an Other variant).
    pub fn type_tag(&self) -> &str {
        match self {
            ContentPart::Known(k) => k.type_tag(),
            ContentPart::Other { type_tag, .. } => type_tag,
        }
    }
}

impl KnownContentPart {
    pub fn cache_control(&self) -> Option<&CacheControl> {
        match self {
            KnownContentPart::Text { cache_control, .. }
            | KnownContentPart::Image { cache_control, .. }
            | KnownContentPart::ImageUrl { cache_control, .. }
            | KnownContentPart::File { cache_control, .. }
            | KnownContentPart::Document { cache_control, .. }
            | KnownContentPart::ToolUse { cache_control, .. }
            | KnownContentPart::ToolResult { cache_control, .. } => cache_control.as_ref(),
            KnownContentPart::Thinking { .. } | KnownContentPart::RedactedThinking { .. } => None,
        }
    }

    pub fn type_tag(&self) -> &'static str {
        match self {
            KnownContentPart::Text { .. } => "text",
            KnownContentPart::Image { .. } => "image",
            KnownContentPart::ImageUrl { .. } => "image_url",
            KnownContentPart::File { .. } => "file",
            KnownContentPart::Document { .. } => "document",
            KnownContentPart::ToolUse { .. } => "tool_use",
            KnownContentPart::ToolResult { .. } => "tool_result",
            KnownContentPart::Thinking { .. } => "thinking",
            KnownContentPart::RedactedThinking { .. } => "redacted_thinking",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_block_round_trips_with_cache_control() {
        let v = json!({
            "type": "text",
            "text": "hello",
            "cache_control": {"type": "ephemeral", "ttl": "5m"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            &part,
            ContentPart::Known(KnownContentPart::Text { text, .. }) if text == "hello"
        ));
        assert_eq!(part.cache_control().unwrap().effective_ttl(), "5m");
        let back = serde_json::to_value(&part).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn anthropic_image_block_round_trips() {
        let v = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"},
            "cache_control": {"type": "ephemeral"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            &part,
            ContentPart::Known(KnownContentPart::Image { .. })
        ));
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn openai_image_url_block_round_trips() {
        let v = json!({
            "type": "image_url",
            "image_url": {"url": "https://example.com/x.png"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            &part,
            ContentPart::Known(KnownContentPart::ImageUrl { .. })
        ));
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn openai_file_block_deserializes_to_known_file() {
        let v = json!({
            "type": "file",
            "file": {
                "filename": "draft.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0xLjQ="
            }
        });
        let part: ContentPart = serde_json::from_value(v).unwrap();
        match &part {
            ContentPart::Known(KnownContentPart::File { file, .. }) => {
                assert_eq!(file["filename"], "draft.pdf");
                assert_eq!(
                    file["file_data"],
                    "data:application/pdf;base64,JVBERi0xLjQ="
                );
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn openai_file_block_round_trips_byte_stable() {
        let v = json!({
            "type": "file",
            "file": {
                "filename": "draft.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0xLjQ="
            }
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn openai_file_id_only_block_round_trips() {
        let v = json!({
            "type": "file",
            "file": {"file_id": "file-abc"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            &part,
            ContentPart::Known(KnownContentPart::File { .. })
        ));
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn file_block_type_tag_is_file() {
        let part = ContentPart::Known(KnownContentPart::File {
            file: json!({"file_id": "file-abc"}),
            cache_control: None,
        });
        assert_eq!(part.type_tag(), "file");
    }

    #[test]
    fn file_block_honors_cache_control() {
        let v = json!({
            "type": "file",
            "file": {"file_id": "file-abc"},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(part.cache_control().unwrap().effective_ttl(), "1h");
        // cache_control rides at the block level, not inside `file`, so
        // the round-trip is byte-stable.
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn thinking_block_preserves_signature() {
        let v = json!({
            "type": "thinking",
            "thinking": "step by step",
            "signature": "sha256:abc"
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        match &part {
            ContentPart::Known(KnownContentPart::Thinking {
                thinking,
                signature,
            }) => {
                assert_eq!(thinking, "step by step");
                assert_eq!(signature.as_deref(), Some("sha256:abc"));
            }
            _ => panic!("expected Thinking, got {part:?}"),
        }
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn tool_use_block_preserves_input() {
        let v = json!({
            "type": "tool_use",
            "id": "toolu_01",
            "name": "calculator",
            "input": {"a": 1, "b": 2}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(
            &part,
            ContentPart::Known(KnownContentPart::ToolUse { .. })
        ));
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn unknown_block_type_falls_to_other() {
        let v = json!({
            "type": "server_tool_use",
            "id": "srvtu_01",
            "name": "web_search",
            "input": {"query": "rust"},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let part: ContentPart = serde_json::from_value(v.clone()).unwrap();
        match &part {
            ContentPart::Other {
                type_tag,
                cache_control,
                extras,
            } => {
                assert_eq!(type_tag, "server_tool_use");
                assert_eq!(cache_control.as_ref().unwrap().effective_ttl(), "1h");
                assert_eq!(extras["id"], "srvtu_01");
                assert_eq!(extras["name"], "web_search");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        // Round-trip preserves the original JSON.
        assert_eq!(serde_json::to_value(&part).unwrap(), v);
    }

    #[test]
    fn cache_control_is_accessible_through_either_variant() {
        let known = ContentPart::Known(KnownContentPart::Text {
            text: "hi".into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
        });
        assert!(known.cache_control().is_some());
        let other = ContentPart::Other {
            type_tag: "future_block".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            extras: Map::new(),
        };
        assert_eq!(other.cache_control().unwrap().effective_ttl(), "1h");
    }
}
