//! Anthropic SSE event state machine.
//!
//! Anthropic streams a sequence of typed events. The state machine here tracks
//! which content block is currently open so that deltas are attributed to the
//! correct block type (text / thinking / tool_use).
//!
//! The normalize_chunk method on the trait is stateless (one raw line -> one
//! option). The actual stateful accumulation lives inside the stream() method
//! in mod.rs which owns an SseState and drives parse_event() directly.

use serde_json::{json, Value};
use uuid::Uuid;

use routectl_core::{
    schema::{CacheCreation, ChunkChoice, ChunkDelta, UsageDelta},
    ChatChunk, Error, ReasoningDetail, ReasoningDetailKind, Result,
};

use super::response::map_stop_reason;
use super::types::SseEvent;

const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Which kind of content block is currently open.
#[derive(Debug, Clone)]
pub enum OpenBlockKind {
    Text,
    Thinking {
        /// Accumulated text so we can emit a signature-only delta.
        accumulated: String,
        /// Block index in reasoning_details array.
        detail_index: u32,
    },
    ToolUse {
        id: String,
        name: String,
        /// Index in the tool_calls array being built.
        call_index: u32,
    },
}

/// Persistent state across SSE events for one streaming response.
#[derive(Debug, Default)]
pub struct SseState {
    pub id: String,
    pub model: String,
    pub next_detail_index: u32,
    pub next_call_index: u32,
    pub open_block: Option<OpenBlockKind>,
}

impl SseState {
    /// Parse one raw SSE data line (the JSON string after "data: ").
    /// Returns Ok(None) for housekeeping events, Ok(Some(chunk)) for content.
    pub fn parse_event(&mut self, provider_id: &str, data: &str) -> Result<Option<ChatChunk>> {
        let event: SseEvent = serde_json::from_str(data)
            .map_err(|e| Error::Streaming(format!("bad sse json: {e}")))?;

        match event {
            SseEvent::MessageStart { message } => {
                self.id = message.id;
                self.model = message.model;
                Ok(None)
            }

            SseEvent::ContentBlockStart {
                index: _,
                content_block,
            } => {
                use super::types::SseContentBlockStart;
                match content_block {
                    SseContentBlockStart::Text { .. } => {
                        self.open_block = Some(OpenBlockKind::Text);
                    }
                    SseContentBlockStart::Thinking { .. } => {
                        let di = self.next_detail_index;
                        self.next_detail_index += 1;
                        self.open_block = Some(OpenBlockKind::Thinking {
                            accumulated: String::new(),
                            detail_index: di,
                        });
                    }
                    SseContentBlockStart::ToolUse { id, name } => {
                        let ci = self.next_call_index;
                        self.next_call_index += 1;
                        self.open_block = Some(OpenBlockKind::ToolUse {
                            id,
                            name,
                            call_index: ci,
                        });
                    }
                    SseContentBlockStart::RedactedThinking { data } => {
                        // No per-token deltas follow a redacted_thinking
                        // block. Emit it immediately as a synthesized
                        // reasoning_details entry; the open_block stays
                        // None so the next block_start opens cleanly.
                        let di = self.next_detail_index;
                        self.next_detail_index += 1;
                        let detail = ReasoningDetail {
                            kind: ReasoningDetailKind::Encrypted,
                            id: Some(Uuid::new_v4().to_string()),
                            format: Some(ANTHROPIC_FORMAT.to_string()),
                            index: Some(di),
                            payload: json!({"data": data}),
                        };
                        return Ok(Some(ChatChunk {
                            id: self.id.clone(),
                            model: self.model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChunkDelta {
                                    reasoning_details: vec![detail],
                                    ..Default::default()
                                },
                                finish_reason: None,
                            }],
                            usage: None,
                        }));
                    }
                }
                Ok(None)
            }

            SseEvent::ContentBlockDelta { index: _, delta } => {
                use super::types::SseDelta;
                match delta {
                    SseDelta::TextDelta { text } => Ok(Some(self.make_text_chunk(text))),
                    SseDelta::ThinkingDelta { thinking } => {
                        // Accumulate for later signature association.
                        if let Some(OpenBlockKind::Thinking { accumulated, .. }) =
                            &mut self.open_block
                        {
                            accumulated.push_str(&thinking);
                        }
                        Ok(Some(self.make_thinking_delta_chunk(provider_id, thinking)?))
                    }
                    SseDelta::SignatureDelta { signature } => {
                        Ok(Some(self.make_signature_chunk(signature)))
                    }
                    SseDelta::InputJsonDelta { partial_json } => {
                        Ok(Some(self.make_tool_delta_chunk(partial_json)))
                    }
                }
            }

            SseEvent::ContentBlockStop { .. } => {
                self.open_block = None;
                Ok(None)
            }

            SseEvent::MessageDelta { delta, usage } => {
                let finish_reason = map_stop_reason(delta.stop_reason.as_deref());
                let usage_delta = usage.as_ref().map(|u| UsageDelta {
                    completion_tokens: u.output_tokens,
                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                    cache_read_input_tokens: u.cache_read_input_tokens,
                    cache_creation: u.cache_creation.as_ref().map(|c| CacheCreation {
                        ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
                        ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
                    }),
                    ..Default::default()
                });
                // Emit a chunk if either side carries information; an
                // empty MessageDelta (no stop_reason and no usage) is
                // a keepalive in spirit -- skip.
                if finish_reason.is_none() && usage_delta.is_none() {
                    return Ok(None);
                }
                Ok(Some(ChatChunk {
                    id: self.id.clone(),
                    model: self.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta::default(),
                        finish_reason,
                    }],
                    usage: usage_delta,
                }))
            }

            SseEvent::MessageStop | SseEvent::Ping => Ok(None),
            // Anthropic spec: a 200 response can carry an in-band
            // error event mid-stream (e.g. `overloaded_error`,
            // `api_error`). Surface as `Error::Streaming` so the
            // ingress wrapper terminates the SSE stream with a
            // visible failure instead of silently completing. The
            // payload is JSON-shaped (`{"type": "...", "message":
            // "..."}`); we serialize it raw to preserve detail for
            // operators reading logs.
            SseEvent::Error { error } => Err(Error::Streaming(format!(
                "anthropic in-stream error: {}",
                error,
            ))),
        }
    }

    // ------------------------------------------------------------------
    // Chunk constructors
    // ------------------------------------------------------------------

    fn make_text_chunk(&self, text: String) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some(text),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn make_thinking_delta_chunk(&self, _provider_id: &str, thinking: String) -> Result<ChatChunk> {
        let detail_index = match &self.open_block {
            Some(OpenBlockKind::Thinking { detail_index, .. }) => *detail_index,
            _ => 0,
        };

        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some(Uuid::new_v4().to_string()),
            format: Some(ANTHROPIC_FORMAT.to_string()),
            index: Some(detail_index),
            // No signature yet -- will arrive via SignatureDelta.
            payload: json!({"text": thinking}),
        };

        Ok(ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning: Some(thinking),
                    reasoning_details: vec![detail],
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        })
    }

    fn make_signature_chunk(&self, signature: String) -> ChatChunk {
        let detail_index = match &self.open_block {
            Some(OpenBlockKind::Thinking { detail_index, .. }) => *detail_index,
            _ => 0,
        };

        // Emit a reasoning_details entry that carries ONLY the signature so
        // clients can attach it to the accumulated thinking block.
        let detail = ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some(Uuid::new_v4().to_string()),
            format: Some(ANTHROPIC_FORMAT.to_string()),
            index: Some(detail_index),
            payload: json!({"signature": signature}),
        };

        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    reasoning_details: vec![detail],
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn make_tool_delta_chunk(&self, partial_json: String) -> ChatChunk {
        let (tool_id, tool_name, call_index) = match &self.open_block {
            Some(OpenBlockKind::ToolUse {
                id,
                name,
                call_index,
            }) => (id.clone(), name.clone(), *call_index),
            _ => (String::new(), String::new(), 0),
        };

        let tool_call_delta: Value = json!({
            "index": call_index,
            "id": tool_id,
            "type": "function",
            "function": {"name": tool_name, "arguments": partial_json}
        });

        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![tool_call_delta]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }
}

/// Stateless single-event parse -- used by the trait's normalize_chunk.
/// Since Anthropic SSE carries both an "event:" line and a "data:" line,
/// and eventsource-stream gives us the data payload separately, we just
/// parse the data JSON. Without state we can only handle text_delta and
/// message_delta safely.
///
/// **WARNING: do not use this for production streaming.** Production
/// callers should use `AnthropicApiProvider::stream` which owns a
/// stateful `SseState` and correctly maps thinking deltas, tool-use
/// deltas, signature deltas, and in-stream `error` events to surfaces
/// the router can act on. This function intentionally returns
/// `Ok(None)` for every event type that requires state OR an error
/// signal -- including `error` events. Routing through here would
/// hide upstream failures from the router's circuit breaker.
pub fn parse_stateless(_provider_id: &str, data: &str) -> Result<Option<ChatChunk>> {
    // Delegate to a throw-away state so we don't lose the id/model.
    // This is intentionally limited; the stateful path in stream() is preferred.
    let v: Value =
        serde_json::from_str(data).map_err(|e| Error::Streaming(format!("bad sse json: {e}")))?;

    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    if event_type == "content_block_delta" {
        let delta_type = v
            .pointer("/delta/type")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if delta_type == "text_delta" {
            let text = v
                .pointer("/delta/text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            return Ok(Some(ChatChunk {
                id: "stream".to_string(),
                model: "unknown".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        content: Some(text),
                        ..Default::default()
                    },
                    finish_reason: None,
                }],
                usage: None,
            }));
        }
    }

    // For message_delta carrying stop_reason:
    if event_type == "message_delta" {
        let stop_reason = v.pointer("/delta/stop_reason").and_then(|r| r.as_str());
        let finish_reason = map_stop_reason(stop_reason);
        if finish_reason.is_some() {
            return Ok(Some(ChatChunk {
                id: "stream".to_string(),
                model: "unknown".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason,
                }],
                usage: None,
            }));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anthropic spec allows a 200 response to carry an in-band
    /// `error` event mid-stream. Without explicit handling, the
    /// parser silently consumed it as housekeeping (Ok(None)) and
    /// the SSE wrapper happily emitted clean EOS to the client,
    /// hiding upstream failures + breaking router circuit-breaker
    /// health accounting. Pin the contract so a future change can't
    /// regress.
    #[test]
    fn in_stream_error_event_surfaces_as_streaming_error() {
        let mut state = SseState::default();
        let payload =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"slow down"}}"#;
        let err = state
            .parse_event("test-anthropic", payload)
            .expect_err("error event must surface as Err");
        match err {
            Error::Streaming(msg) => {
                assert!(msg.contains("anthropic in-stream error"), "msg: {msg}");
                assert!(msg.contains("overloaded_error"), "msg: {msg}");
            }
            other => panic!("expected Error::Streaming, got: {other:?}"),
        }
    }

    /// Counterpart to the above: housekeeping events still produce
    /// `Ok(None)`. Pinning this prevents a future change that
    /// over-corrects the error-mapping fix into surfacing pings as
    /// failures.
    #[test]
    fn ping_event_remains_ok_none() {
        let mut state = SseState::default();
        let got = state
            .parse_event("test-anthropic", r#"{"type":"ping"}"#)
            .unwrap();
        assert!(got.is_none(), "ping must be Ok(None), got: {got:?}");
    }
}
