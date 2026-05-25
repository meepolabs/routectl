//! Forward-compat catchalls for the three strict-tagged Anthropic SSE
//! enums.
//!
//! Extracted from `types.rs` so the parent stays under the project's
//! 800-LOC ceiling. Each public enum gains an `Other(Value)` arm:
//! unknown wire variants land there verbatim instead of failing serde
//! and propagating as `Error::Streaming` (which silently drops the
//! rest of the stream). The downstream consumer (the SSE state
//! machine in `sse.rs`) extracts the `type` tag from the held Value
//! via `v.get("type").and_then(Value::as_str)` for logging /
//! `OpenBlockKind::Unknown` / opaque-events forwarding.
//!
//! Production triggers that motivated this:
//! - `server_tool_use` / `web_search_tool_result` block-start types
//!   (web search beta, ships in
//!   `content_block_start.content_block`).
//! - `citations_delta` event type (emitted inside
//!   `web_search_tool_result` blocks).
//! - Any future top-level event Anthropic adds without a wire-shape
//!   migration.
//!
//! Pattern: the public enum is `#[derive(Debug)]` plus a custom
//! `Deserialize` impl. A private `Known*` mirror enum drives serde's
//! built-in tagged-union dispatch on the happy path; unknown tags
//! fall to `Other(Value)`. We deliberately do NOT use serde's
//! `#[serde(other)]` unit-variant catchall here -- it loses the
//! `type` tag and the rest of the payload, both of which the SSE
//! state machine needs.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::types::{SseDeltaUsage, SseMessage, SseMessageDelta};

// ---------------------------------------------------------------------------
// SseEvent -- top-level event envelope
// ---------------------------------------------------------------------------

/// Top-level SSE event envelope. `Other(Value)` is the forward-compat
/// catchall: a `type` tag not in the known set deserializes here with
/// the full event Value preserved (including the `type` field) so the
/// SSE state machine can log it and decide what to do.
#[derive(Debug)]
#[allow(dead_code)] // index/text/thinking captured for forward-compat replay
pub(crate) enum SseEvent {
    MessageStart {
        message: SseMessage,
    },
    ContentBlockStart {
        index: u32,
        content_block: SseContentBlockStart,
    },
    ContentBlockDelta {
        index: u32,
        delta: SseDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: SseMessageDelta,
        usage: Option<SseDeltaUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
    /// Forward-compat. Unknown top-level event types land here
    /// verbatim. Read `value.get("type")` for the wire tag.
    Other(Value),
}

impl<'de> Deserialize<'de> for SseEvent {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Private mirror that drives serde's built-in tagged-union
        // dispatch. Mirror exists only because adding `Other(Value)`
        // to the public enum forces a custom impl; the mirror absorbs
        // the happy path so we don't hand-roll per-variant field
        // extraction.
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known {
            MessageStart {
                message: SseMessage,
            },
            ContentBlockStart {
                index: u32,
                content_block: SseContentBlockStart,
            },
            ContentBlockDelta {
                index: u32,
                delta: SseDelta,
            },
            ContentBlockStop {
                index: u32,
            },
            MessageDelta {
                delta: SseMessageDelta,
                usage: Option<SseDeltaUsage>,
            },
            MessageStop,
            Ping,
            Error {
                error: Value,
            },
        }

        const KNOWN_TAGS: &[&str] = &[
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
            "ping",
            "error",
        ];

        let v: Value = Value::deserialize(de)?;
        let is_known = v
            .get("type")
            .and_then(Value::as_str)
            .map(|t| KNOWN_TAGS.contains(&t))
            .unwrap_or(false);
        if !is_known {
            return Ok(SseEvent::Other(v));
        }
        let known: Known = serde_json::from_value(v).map_err(D::Error::custom)?;
        Ok(match known {
            Known::MessageStart { message } => SseEvent::MessageStart { message },
            Known::ContentBlockStart {
                index,
                content_block,
            } => SseEvent::ContentBlockStart {
                index,
                content_block,
            },
            Known::ContentBlockDelta { index, delta } => {
                SseEvent::ContentBlockDelta { index, delta }
            }
            Known::ContentBlockStop { index } => SseEvent::ContentBlockStop { index },
            Known::MessageDelta { delta, usage } => SseEvent::MessageDelta { delta, usage },
            Known::MessageStop => SseEvent::MessageStop,
            Known::Ping => SseEvent::Ping,
            Known::Error { error } => SseEvent::Error { error },
        })
    }
}

// ---------------------------------------------------------------------------
// SseContentBlockStart -- block-open shapes
// ---------------------------------------------------------------------------

/// `content_block_start.content_block` payload. `Other(Value)` is the
/// forward-compat catchall: production triggers are `server_tool_use`
/// and `web_search_tool_result`. The held Value preserves the
/// `type` tag so the state machine can map it to
/// `OpenBlockKind::Unknown` and propagate as opaque-events.
#[derive(Debug)]
#[allow(dead_code)] // text/thinking captured for forward-compat replay
pub(crate) enum SseContentBlockStart {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    /// Encrypted thinking block (server emits the data verbatim; no
    /// per-token deltas follow). Required so a streamed response
    /// containing `redacted_thinking` deserializes cleanly instead of
    /// erroring on an unknown variant -- which silently dropped the
    /// rest of the stream in v0.4.0 pre-fix.
    RedactedThinking {
        data: String,
    },
    /// Forward-compat. Unknown `content_block.type` tags land here
    /// verbatim (full block Value, including `type`).
    Other(Value),
}

impl<'de> Deserialize<'de> for SseContentBlockStart {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known {
            Text { text: String },
            Thinking { thinking: String },
            ToolUse { id: String, name: String },
            RedactedThinking { data: String },
        }

        const KNOWN_TAGS: &[&str] = &["text", "thinking", "tool_use", "redacted_thinking"];

        let v: Value = Value::deserialize(de)?;
        let is_known = v
            .get("type")
            .and_then(Value::as_str)
            .map(|t| KNOWN_TAGS.contains(&t))
            .unwrap_or(false);
        if !is_known {
            return Ok(SseContentBlockStart::Other(v));
        }
        let known: Known = serde_json::from_value(v).map_err(D::Error::custom)?;
        Ok(match known {
            Known::Text { text } => SseContentBlockStart::Text { text },
            Known::Thinking { thinking } => SseContentBlockStart::Thinking { thinking },
            Known::ToolUse { id, name } => SseContentBlockStart::ToolUse { id, name },
            Known::RedactedThinking { data } => SseContentBlockStart::RedactedThinking { data },
        })
    }
}

// ---------------------------------------------------------------------------
// SseDelta -- delta shapes
// ---------------------------------------------------------------------------

/// `content_block_delta.delta` payload. `Other(Value)` is the
/// forward-compat catchall: production trigger is `citations_delta`
/// emitted inside `web_search_tool_result` blocks.
#[derive(Debug)]
#[allow(dead_code)] // Other(Value) wire body read via Value::get from the state machine
#[allow(clippy::enum_variant_names)] // wire shape: Anthropic prefixes every delta with `*Delta`
pub(crate) enum SseDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    /// Forward-compat. Unknown `delta.type` tags land here verbatim
    /// (full delta Value, including `type`).
    Other(Value),
}

impl<'de> Deserialize<'de> for SseDelta {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(clippy::enum_variant_names)] // wire shape: every variant is a `*Delta`
        enum Known {
            TextDelta { text: String },
            ThinkingDelta { thinking: String },
            SignatureDelta { signature: String },
            InputJsonDelta { partial_json: String },
        }

        const KNOWN_TAGS: &[&str] = &[
            "text_delta",
            "thinking_delta",
            "signature_delta",
            "input_json_delta",
        ];

        let v: Value = Value::deserialize(de)?;
        let is_known = v
            .get("type")
            .and_then(Value::as_str)
            .map(|t| KNOWN_TAGS.contains(&t))
            .unwrap_or(false);
        if !is_known {
            return Ok(SseDelta::Other(v));
        }
        let known: Known = serde_json::from_value(v).map_err(D::Error::custom)?;
        Ok(match known {
            Known::TextDelta { text } => SseDelta::TextDelta { text },
            Known::ThinkingDelta { thinking } => SseDelta::ThinkingDelta { thinking },
            Known::SignatureDelta { signature } => SseDelta::SignatureDelta { signature },
            Known::InputJsonDelta { partial_json } => SseDelta::InputJsonDelta { partial_json },
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------
    // SseEvent::Other
    // -----------------------------------------------------------------

    /// Unknown top-level event tag deserializes into `Other(Value)`
    /// without a serde error. Pin the type_tag and a payload field
    /// are both reachable via the held Value so the state machine
    /// can log the wire shape.
    #[test]
    fn sse_event_other_captures_unknown_top_level_event() {
        // Arrange
        let raw = json!({"type": "new_top_level_event", "data": 42});

        // Act
        let parsed: SseEvent = serde_json::from_value(raw.clone()).expect("must not error");

        // Assert
        match parsed {
            SseEvent::Other(v) => {
                assert_eq!(
                    v.get("type").and_then(Value::as_str),
                    Some("new_top_level_event")
                );
                assert_eq!(v.get("data").and_then(Value::as_i64), Some(42));
                assert_eq!(v, raw, "Other must hold the full wire Value");
            }
            other => panic!("expected SseEvent::Other, got: {other:?}"),
        }
    }

    /// Known top-level event types (here: `ping`) keep deserializing
    /// to their typed variant; the catchall must NOT swallow them.
    #[test]
    fn sse_event_known_ping_still_deserializes_to_typed_variant() {
        // Arrange
        let raw = json!({"type": "ping"});

        // Act
        let parsed: SseEvent = serde_json::from_value(raw).unwrap();

        // Assert
        assert!(matches!(parsed, SseEvent::Ping));
    }

    /// `error` event keeps surfacing as `SseEvent::Error` so the SSE
    /// wrapper can map it to `Error::Streaming`. This pins the
    /// in-stream error contract that
    /// `in_stream_error_event_surfaces_as_streaming_error` (in
    /// sse.rs) relies on.
    #[test]
    fn sse_event_error_variant_not_swallowed_by_other() {
        // Arrange
        let raw = json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "slow down"},
        });

        // Act
        let parsed: SseEvent = serde_json::from_value(raw).unwrap();

        // Assert
        match parsed {
            SseEvent::Error { error } => {
                assert_eq!(
                    error.get("type").and_then(Value::as_str),
                    Some("overloaded_error"),
                );
            }
            other => panic!("expected SseEvent::Error, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // SseContentBlockStart::Other
    // -----------------------------------------------------------------

    /// Production failure trigger #1: `server_tool_use` (web search
    /// beta). The exact wire shape lands in
    /// `content_block_start.content_block`. Pin that the type_tag is
    /// preserved AND the `input` payload survives verbatim so the
    /// state machine can forward it via opaque-events.
    #[test]
    fn sse_content_block_start_other_captures_server_tool_use_verbatim() {
        // Arrange -- exact wire shape from the web-search beta.
        let raw = json!({
            "type": "server_tool_use",
            "id": "srv_01",
            "name": "web_search",
            "input": {"query": "test"},
        });

        // Act
        let parsed: SseContentBlockStart =
            serde_json::from_value(raw.clone()).expect("must not error");

        // Assert
        match parsed {
            SseContentBlockStart::Other(v) => {
                assert_eq!(
                    v.get("type").and_then(Value::as_str),
                    Some("server_tool_use"),
                    "type_tag must survive in the held Value",
                );
                assert_eq!(v.get("id").and_then(Value::as_str), Some("srv_01"));
                assert_eq!(v.get("name").and_then(Value::as_str), Some("web_search"));
                assert_eq!(
                    v.get("input")
                        .and_then(|i| i.get("query"))
                        .and_then(Value::as_str),
                    Some("test"),
                    "input payload must survive verbatim",
                );
                assert_eq!(v, raw, "Other must hold the entire wire Value");
            }
            other => panic!("expected SseContentBlockStart::Other, got: {other:?}"),
        }
    }

    /// Production failure trigger #2: `web_search_tool_result`. Pin
    /// the `tool_use_id` and (empty) `content` array survive.
    #[test]
    fn sse_content_block_start_other_captures_web_search_tool_result() {
        // Arrange
        let raw = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srv_01",
            "content": [],
        });

        // Act
        let parsed: SseContentBlockStart =
            serde_json::from_value(raw.clone()).expect("must not error");

        // Assert
        match parsed {
            SseContentBlockStart::Other(v) => {
                assert_eq!(
                    v.get("type").and_then(Value::as_str),
                    Some("web_search_tool_result"),
                );
                assert_eq!(v.get("tool_use_id").and_then(Value::as_str), Some("srv_01"),);
                assert!(
                    v.get("content").map(Value::is_array).unwrap_or(false),
                    "content array must survive",
                );
                assert_eq!(v, raw);
            }
            other => panic!("expected SseContentBlockStart::Other, got: {other:?}"),
        }
    }

    /// Known content-block types (here: `redacted_thinking`) keep
    /// deserializing to their typed variant.
    #[test]
    fn sse_content_block_start_known_redacted_thinking_still_typed() {
        // Arrange
        let raw = json!({"type": "redacted_thinking", "data": "ENCRYPTED"});

        // Act
        let parsed: SseContentBlockStart = serde_json::from_value(raw).unwrap();

        // Assert
        match parsed {
            SseContentBlockStart::RedactedThinking { data } => {
                assert_eq!(data, "ENCRYPTED");
            }
            other => panic!("expected RedactedThinking, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // SseDelta::Other
    // -----------------------------------------------------------------

    /// Production failure trigger: `citations_delta` (emitted inside
    /// `web_search_tool_result` blocks). Pin the type_tag survives
    /// and the nested `citation` payload is reachable via Value::get.
    #[test]
    fn sse_delta_other_captures_citations_delta_verbatim() {
        // Arrange -- shape Anthropic emits per the web-search beta.
        let raw = json!({
            "type": "citations_delta",
            "citation": {
                "type": "web_search_result_location",
                "cited_text": "...",
                "url": "...",
            },
        });

        // Act
        let parsed: SseDelta = serde_json::from_value(raw.clone()).expect("must not error");

        // Assert
        match parsed {
            SseDelta::Other(v) => {
                assert_eq!(
                    v.get("type").and_then(Value::as_str),
                    Some("citations_delta"),
                );
                let citation = v.get("citation").expect("citation field preserved");
                assert_eq!(
                    citation.get("type").and_then(Value::as_str),
                    Some("web_search_result_location"),
                );
                assert_eq!(citation.get("url").and_then(Value::as_str), Some("..."),);
                assert_eq!(v, raw);
            }
            other => panic!("expected SseDelta::Other, got: {other:?}"),
        }
    }

    /// Known delta types (here: `text_delta`) keep deserializing to
    /// their typed variant.
    #[test]
    fn sse_delta_known_text_delta_still_typed() {
        // Arrange
        let raw = json!({"type": "text_delta", "text": "hello"});

        // Act
        let parsed: SseDelta = serde_json::from_value(raw).unwrap();

        // Assert
        match parsed {
            SseDelta::TextDelta { text } => assert_eq!(text, "hello"),
            other => panic!("expected TextDelta, got: {other:?}"),
        }
    }
}
