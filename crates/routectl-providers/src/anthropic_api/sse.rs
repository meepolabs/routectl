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
    /// Captured from `message_start.message.usage`. Anthropic emits
    /// the input side of usage exactly once, in `message_start`; the
    /// streaming `message_delta` events carry only output-side updates.
    /// We carry the captured input fields forward so the final
    /// `message_delta` chunk we emit downstream has full prompt_tokens
    /// (sum of input + cache_creation + cache_read), matching what
    /// OpenAI clients expect on the closing usage frame.
    pub captured_input_usage: Option<CapturedInputUsage>,
}

#[derive(Debug, Default, Clone)]
pub struct CapturedInputUsage {
    pub input_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation: Option<CacheCreation>,
}

impl CapturedInputUsage {
    fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
    }
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
                if let Some(u) = message.usage {
                    self.captured_input_usage = Some(CapturedInputUsage {
                        input_tokens: u.input_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                        cache_creation: u.cache_creation.as_ref().map(|c| CacheCreation {
                            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
                            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
                        }),
                    });
                }
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
                // Anthropic emits input usage only on message_start, so
                // the closing chunk must carry it forward for OpenAI
                // clients to see full prompt_tokens.
                let captured = self.captured_input_usage.clone();
                let usage_delta = if usage.is_some() || captured.is_some() {
                    let cap = captured.as_ref();
                    // Real Anthropic does NOT emit `input_tokens` on
                    // `message_delta`; only a chained routectl does, and
                    // there it carries the already-summed prompt total
                    // from `crates/routectl-cli/src/ingress/anthropic.rs`.
                    // So `d` non-zero is treated as the pre-summed total;
                    // zero/missing falls back to summing cap on the
                    // message_start side. A future upstream that emits
                    // raw (non-summed) input_tokens on message_delta
                    // would silently undercount -- tests below pin both
                    // branches so the regression is loud.
                    let prompt_tokens = match (
                        usage.as_ref().and_then(|u| u.input_tokens),
                        cap.map(|c| c.prompt_tokens()),
                    ) {
                        (Some(d), _) if d > 0 => Some(d),
                        (_, Some(c)) if c > 0 => Some(c),
                        (Some(d), _) => Some(d),
                        (_, c) => c,
                    };
                    let completion_tokens = usage.as_ref().and_then(|u| u.output_tokens);
                    let total_tokens = match (prompt_tokens, completion_tokens) {
                        (Some(p), Some(c)) => Some(p.saturating_add(c)),
                        (Some(p), None) => Some(p),
                        (None, Some(c)) => Some(c),
                        (None, None) => None,
                    };
                    // Prefer delta when present and non-zero; fall back
                    // to captured. Some(0) is "no info", not
                    // "authoritative zero" -- placeholder restatements
                    // must not blow away non-zero captured numbers.
                    let pick = |delta: Option<u32>, cap_v: Option<u32>| -> Option<u32> {
                        match (delta, cap_v) {
                            (Some(d), _) if d > 0 => Some(d),
                            (_, Some(c)) if c > 0 => Some(c),
                            (Some(d), Some(c)) => Some(d.max(c)),
                            (Some(0), None) | (None, Some(0)) => None,
                            (Some(d), None) => Some(d),
                            (None, c) => c,
                        }
                    };
                    let cache_creation_input_tokens = pick(
                        usage.as_ref().and_then(|u| u.cache_creation_input_tokens),
                        cap.and_then(|c| c.cache_creation_input_tokens),
                    );
                    let cache_read_input_tokens = pick(
                        usage.as_ref().and_then(|u| u.cache_read_input_tokens),
                        cap.and_then(|c| c.cache_read_input_tokens),
                    );
                    // Per-TTL merge via the same `pick` so a delta with
                    // partial/empty `cache_creation` doesn't wholesale-
                    // replace the richer message_start object.
                    let delta_cc = usage.as_ref().and_then(|u| u.cache_creation.as_ref());
                    let cap_cc = cap.and_then(|c| c.cache_creation.as_ref());
                    let cache_creation_5m = pick(
                        delta_cc.and_then(|c| c.ephemeral_5m_input_tokens),
                        cap_cc.and_then(|c| c.ephemeral_5m_input_tokens),
                    );
                    let cache_creation_1h = pick(
                        delta_cc.and_then(|c| c.ephemeral_1h_input_tokens),
                        cap_cc.and_then(|c| c.ephemeral_1h_input_tokens),
                    );
                    let cache_creation =
                        if cache_creation_5m.is_some() || cache_creation_1h.is_some() {
                            Some(CacheCreation {
                                ephemeral_5m_input_tokens: cache_creation_5m,
                                ephemeral_1h_input_tokens: cache_creation_1h,
                            })
                        } else {
                            None
                        };
                    Some(UsageDelta {
                        prompt_tokens,
                        completion_tokens,
                        total_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                        cache_creation,
                        ..Default::default()
                    })
                } else {
                    None
                };
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

    /// `message_start.usage` carries the input side of token accounting.
    /// Real Anthropic emits non-zero `input_tokens` plus cache numbers
    /// here; some upstream variants (and routectl's own Anthropic
    /// ingress today) emit zeros that get corrected later in
    /// `message_delta`. The state must capture whatever's in
    /// `message_start.usage` so the closing chunk can sum them into
    /// `prompt_tokens`.
    #[test]
    fn message_start_captures_input_usage_for_summing() {
        let mut state = SseState::default();
        let payload = r#"{
            "type":"message_start",
            "message": {
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "claude-opus-4-7",
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 200,
                    "cache_read_input_tokens": 300
                }
            }
        }"#;
        let _ = state.parse_event("test", payload).unwrap();
        let cap = state
            .captured_input_usage
            .as_ref()
            .expect("input usage captured from message_start");
        assert_eq!(cap.input_tokens, 100);
        assert_eq!(cap.cache_creation_input_tokens, Some(200));
        assert_eq!(cap.cache_read_input_tokens, Some(300));
        // sum surfaces as the prompt_tokens helper.
        assert_eq!(cap.prompt_tokens(), 600);
    }

    /// Closing `message_delta` chunk must carry the full
    /// `prompt_tokens` (sum of input + cache_creation + cache_read)
    /// so OpenAI clients see the cumulative context size at end-of-
    /// stream, not zero (the prior bug) or just the new turn's
    /// non-cached count.
    #[test]
    fn message_delta_emits_prompt_tokens_from_captured_input() {
        let mut state = SseState::default();
        // message_start with non-trivial input + cache numbers.
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message": {
                        "id":"msg_01","type":"message","role":"assistant",
                        "content":[],"model":"claude-opus-4-7",
                        "stop_reason":null,"stop_sequence":null,
                        "usage": {
                            "input_tokens": 50,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 100,
                            "cache_read_input_tokens": 200
                        }
                    }
                }"#,
            )
            .unwrap();
        // message_delta with output usage and stop_reason.
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta": {"stop_reason":"end_turn","stop_sequence":null},
                    "usage": {"output_tokens": 25}
                }"#,
            )
            .unwrap()
            .expect("closing chunk emitted");
        let usage = chunk.usage.expect("usage on closing chunk");
        // 50 + 100 + 200 = 350
        assert_eq!(usage.prompt_tokens, Some(350));
        assert_eq!(usage.completion_tokens, Some(25));
        assert_eq!(usage.total_tokens, Some(375));
    }

    /// When `message_delta.usage.input_tokens` is present (e.g. from
    /// routectl's own Anthropic ingress emitting the post-cache total),
    /// prefer it over the captured `message_start` value. This is the
    /// "chained routectl" scenario: an upstream routectl renders the
    /// final input count to `message_delta.usage.input_tokens` because
    /// `message_start.usage` was hardcoded to zero.
    #[test]
    fn message_delta_input_tokens_overrides_captured_zero() {
        let mut state = SseState::default();
        // message_start with zero input (the upstream-hardcoded case).
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message": {
                        "id":"msg_01","type":"message","role":"assistant",
                        "content":[],"model":"claude-opus-4-7",
                        "stop_reason":null,"stop_sequence":null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }"#,
            )
            .unwrap();
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta": {"stop_reason":"end_turn","stop_sequence":null},
                    "usage": {
                        "input_tokens": 12345,
                        "output_tokens": 50,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }"#,
            )
            .unwrap()
            .expect("closing chunk emitted");
        let usage = chunk.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, Some(12345));
        assert_eq!(usage.completion_tokens, Some(50));
    }

    /// Pin the chained-routectl invariant: when both message_start AND
    /// message_delta carry non-zero input_tokens, the delta value
    /// (which an upstream routectl writes as the already-summed
    /// prompt_tokens) wins. Use DISTINCT values so the test
    /// distinguishes "delta wins" from "captured wins" -- a regression
    /// flipping the match arm order would fail this test.
    #[test]
    fn message_delta_input_tokens_wins_over_captured_when_both_nonzero() {
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message": {
                        "id":"msg_01","type":"message","role":"assistant",
                        "content":[],"model":"claude-opus-4-7",
                        "stop_reason":null,"stop_sequence":null,
                        "usage": {"input_tokens": 50, "output_tokens": 0}
                    }
                }"#,
            )
            .unwrap();
        // Delta sends a DIFFERENT non-zero input count (the chained-
        // upstream's pre-summed value, e.g. after a cache hit on the
        // upstream side that wasn't visible to our message_start).
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta": {"stop_reason":"end_turn","stop_sequence":null},
                    "usage": {"input_tokens": 99, "output_tokens": 10}
                }"#,
            )
            .unwrap()
            .expect("closing chunk emitted");
        let usage = chunk.usage.expect("usage");
        // 99 (delta) wins, NOT 50 (captured) and NOT 149 (sum).
        assert_eq!(usage.prompt_tokens, Some(99));
        assert_eq!(usage.completion_tokens, Some(10));
        assert_eq!(usage.total_tokens, Some(109));
    }

    /// Pin the cache-merge zero-aware fallback: if the closing
    /// `message_delta.usage` restates `cache_*` as explicit zero while
    /// `message_start` had non-zero captured values, keep the captured
    /// values. Otherwise a placeholder restatement would erase real
    /// cache stats.
    #[test]
    fn message_delta_cache_zero_does_not_overwrite_captured_nonzero() {
        let mut state = SseState::default();
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message": {
                        "id":"msg_01","type":"message","role":"assistant",
                        "content":[],"model":"claude-opus-4-7",
                        "stop_reason":null,"stop_sequence":null,
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 100,
                            "cache_read_input_tokens": 200
                        }
                    }
                }"#,
            )
            .unwrap();
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta": {"stop_reason":"end_turn","stop_sequence":null},
                    "usage": {
                        "output_tokens": 7,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }"#,
            )
            .unwrap()
            .expect("closing chunk");
        let usage = chunk.usage.expect("usage");
        assert_eq!(usage.cache_creation_input_tokens, Some(100));
        assert_eq!(usage.cache_read_input_tokens, Some(200));
    }

    /// Pin the per-TTL `cache_creation` field-level merge: a delta
    /// carrying a partial or empty `cache_creation` object must NOT
    /// wholesale-replace the captured object's per-TTL detail. Each
    /// field falls back independently through the same zero-aware
    /// pick.
    #[test]
    fn message_delta_partial_cache_creation_object_merges_per_ttl() {
        let mut state = SseState::default();
        // message_start with both TTL buckets set.
        let _ = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_start",
                    "message": {
                        "id":"msg_01","type":"message","role":"assistant",
                        "content":[],"model":"claude-opus-4-7",
                        "stop_reason":null,"stop_sequence":null,
                        "usage": {
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cache_creation": {
                                "ephemeral_5m_input_tokens": 50,
                                "ephemeral_1h_input_tokens": 100
                            }
                        }
                    }
                }"#,
            )
            .unwrap();
        // Delta restates ONLY the 5m bucket (e.g. an upstream that
        // tracks 5m only); 1h is absent in the wire payload.
        let chunk = state
            .parse_event(
                "test",
                r#"{
                    "type":"message_delta",
                    "delta": {"stop_reason":"end_turn","stop_sequence":null},
                    "usage": {
                        "output_tokens": 5,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 75
                        }
                    }
                }"#,
            )
            .unwrap()
            .expect("closing chunk");
        let usage = chunk.usage.expect("usage");
        let cc = usage.cache_creation.expect("cache_creation present");
        // 5m: delta's 75 wins over captured 50.
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(75));
        // 1h: absent from delta, falls back to captured 100. Pre-fix
        // this would be None (whole-object replacement lost it).
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(100));
    }
}
