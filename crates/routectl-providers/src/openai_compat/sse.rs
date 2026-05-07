//! SSE chunk parsing and per-chunk normalization.
//!
//! `parse_chunk` is stateless: it handles one `data: ...` line and returns
//! `Ok(None)` for `[DONE]` / keepalives, or `Ok(Some(ChatChunk))` for
//! content chunks. Reasoning is lifted where the upstream field is simple
//! (DeepSeek, vLLM, OpenAI, OpenRouter).
//!
//! The `<think>` tag state machine for `RawThinkTag` is NOT stateless, so it
//! lives in `ThinkTagAccumulator` which the `stream()` caller owns across
//! chunks. Call `ThinkTagAccumulator::process` instead of `parse_chunk` when
//! the dialect is `RawThinkTag`.

use serde_json::Value;

use routectl_core::schema::{ChunkChoice, ChunkDelta};
use routectl_core::{ChatChunk, Error, Result};

use super::dialect::ReasoningDialect;
use super::util::build_reasoning_detail;

// ---------------------------------------------------------------------------
// Stateless path
// ---------------------------------------------------------------------------

/// Parse one SSE data line for dialects that do not need cross-chunk state.
///
/// `raw` is the value portion after `data: `, e.g. `{"id":"...","choices":[...]}`.
/// Returns `Ok(None)` for `[DONE]`, empty lines, and comment lines.
pub fn parse_chunk(id: &str, raw: &str, dialect: ReasoningDialect) -> Result<Option<ChatChunk>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
        return Ok(None);
    }

    let mut val: Value = serde_json::from_str(trimmed)
        .map_err(|e| Error::Streaming(format!("provider `{id}`: SSE JSON: {e}")))?;

    // Coalesce reasoning_content -> reasoning across all delta objects so
    // serde sees a single canonical key and dialect-specific lifters can
    // operate on a uniform shape.
    coalesce_chunk_reasoning_keys(&mut val);

    apply_chunk_dialect(id, &mut val, dialect)?;

    let chunk: ChatChunk = serde_json::from_value(val)
        .map_err(|e| Error::normalize_response(id, format!("chunk deserialize: {e}")))?;

    Ok(Some(chunk))
}

/// Walk `choices[].delta` and merge `reasoning_content` into `reasoning`.
/// See `merge_reasoning_keys` in `response.rs` for the rules.
fn coalesce_chunk_reasoning_keys(val: &mut Value) {
    let Some(choices) = val.get_mut("choices").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for choice in choices.iter_mut() {
        if let Some(delta) = choice.get_mut("delta").and_then(|v| v.as_object_mut()) {
            super::response::merge_reasoning_keys(delta);
        }
    }
}

fn apply_chunk_dialect(id: &str, val: &mut Value, dialect: ReasoningDialect) -> Result<()> {
    dialect.as_dyn().apply_chunk(id, val)
}

// ---------------------------------------------------------------------------
// Stateful <think> tag accumulator (used by stream() for RawThinkTag dialect)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
enum ThinkState {
    #[default]
    Outside,
    Inside,
}

/// Cross-chunk state machine for stripping `<think>...</think>` from a
/// streaming content delta. Each call to `process` consumes one SSE data
/// line and returns a `ChatChunk` with the content/reasoning fields separated.
pub struct ThinkTagAccumulator {
    state: ThinkState,
    chunk_index: u32,
}

impl ThinkTagAccumulator {
    pub fn new() -> Self {
        Self {
            state: ThinkState::default(),
            chunk_index: 0,
        }
    }

    /// Process one raw SSE data value. Returns `Ok(None)` for `[DONE]` / keepalive.
    pub fn process(&mut self, provider_id: &str, raw: &str) -> Result<Option<ChatChunk>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
            return Ok(None);
        }

        let val: Value = serde_json::from_str(trimmed)
            .map_err(|e| Error::Streaming(format!("provider `{provider_id}`: SSE JSON: {e}")))?;

        let chunk_id = val
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("chunk")
            .to_string();
        let model = val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let choices_raw = match val.get("choices").and_then(|v| v.as_array()) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };

        let mut new_choices = Vec::with_capacity(choices_raw.len());
        for choice_val in &choices_raw {
            let index = choice_val
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let finish_reason = choice_val
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let delta_content = choice_val
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let role = choice_val
                .get("delta")
                .and_then(|d| d.get("role"))
                .and_then(|v| v.as_str())
                .map(|r| serde_json::from_value(serde_json::Value::String(r.into())).ok())
                .flatten();
            let tool_calls = choice_val
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .cloned()
                .and_then(|v| if v.is_null() { None } else { Some(v) })
                .and_then(|v| v.as_array().cloned())
                .map(|arr| arr.into_iter().collect::<Vec<_>>());

            let (outside_content, inside_reasoning) = self.split_think_tags(&delta_content);

            let mut delta = ChunkDelta {
                role,
                tool_calls,
                ..Default::default()
            };
            if !outside_content.is_empty() {
                delta.content = Some(outside_content);
            }
            if !inside_reasoning.is_empty() {
                delta.reasoning = Some(inside_reasoning.clone());
                let detail = build_reasoning_detail(
                    &inside_reasoning,
                    ReasoningDialect::RawThinkTag.format_tag(),
                    self.chunk_index,
                );
                delta.reasoning_details.push(detail);
                self.chunk_index += 1;
            }

            new_choices.push(ChunkChoice {
                index,
                delta,
                finish_reason,
            });
        }

        Ok(Some(ChatChunk {
            id: chunk_id,
            model,
            choices: new_choices,
        }))
    }

    /// Split `text` into (outside_think_content, inside_think_content) while
    /// advancing the internal `<think>` / `</think>` state machine.
    ///
    /// Handles three cases:
    ///   1. Tag boundary falls mid-chunk (e.g. only `<think>` arrives with no `</think>`).
    ///   2. Full `<think>...</think>` inside one chunk.
    ///   3. Mixed: text before tag + tag content + text after tag.
    fn split_think_tags(&mut self, text: &str) -> (String, String) {
        let mut outside = String::new();
        let mut inside = String::new();
        let mut remaining = text;

        loop {
            match self.state {
                ThinkState::Outside => {
                    if let Some(pos) = remaining.find("<think>") {
                        outside.push_str(&remaining[..pos]);
                        remaining = &remaining[pos + "<think>".len()..];
                        self.state = ThinkState::Inside;
                    } else {
                        outside.push_str(remaining);
                        break;
                    }
                }
                ThinkState::Inside => {
                    if let Some(pos) = remaining.find("</think>") {
                        inside.push_str(&remaining[..pos]);
                        remaining = &remaining[pos + "</think>".len()..];
                        self.state = ThinkState::Outside;
                    } else {
                        // No closing tag yet -- everything is inside.
                        inside.push_str(remaining);
                        break;
                    }
                }
            }
        }

        (outside, inside)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta_chunk(content: Option<&str>, reasoning_content: Option<&str>) -> String {
        let mut delta = json!({});
        if let Some(c) = content {
            delta["content"] = json!(c);
        }
        if let Some(r) = reasoning_content {
            delta["reasoning_content"] = json!(r);
        }
        let chunk = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": null
            }]
        });
        chunk.to_string()
    }

    #[test]
    fn done_returns_none() {
        assert!(parse_chunk("t", "[DONE]", ReasoningDialect::OpenAi)
            .unwrap()
            .is_none());
        assert!(parse_chunk("t", "  [DONE]  ", ReasoningDialect::OpenAi)
            .unwrap()
            .is_none());
        assert!(parse_chunk("t", "", ReasoningDialect::OpenAi)
            .unwrap()
            .is_none());
    }

    #[test]
    fn openai_basic_delta() {
        let raw = delta_chunk(Some("hello"), None);
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
        assert!(chunk.choices[0].delta.reasoning_details.is_empty());
    }

    #[test]
    fn deepseek_lifts_reasoning_content_in_delta() {
        let raw = delta_chunk(Some("answer"), Some("chain of thought"));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::DeepSeek)
            .unwrap()
            .unwrap();
        let details = &chunk.choices[0].delta.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
        assert_eq!(details[0].payload["text"], "chain of thought");
        // Original content preserved
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("answer"));
    }

    #[test]
    fn vllm_lifts_reasoning_content_in_delta() {
        let raw = delta_chunk(None, Some("vllm trace"));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::Vllm)
            .unwrap()
            .unwrap();
        let details = &chunk.choices[0].delta.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
    }

    // --- ThinkTagAccumulator tests ---

    #[test]
    fn think_tag_whole_in_one_chunk() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = delta_chunk(Some("<think>reasoning</think>answer"), None);
        let chunk = acc.process("t", &raw).unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("answer"));
        assert_eq!(
            chunk.choices[0].delta.reasoning.as_deref(),
            Some("reasoning")
        );
    }

    #[test]
    fn think_tag_open_in_first_chunk_close_in_second() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: opens but does not close the tag
        let raw1 = delta_chunk(Some("<think>partial reason"), None);
        let c1 = acc.process("t", &raw1).unwrap().unwrap();
        assert!(c1.choices[0].delta.content.is_none());
        assert_eq!(
            c1.choices[0].delta.reasoning.as_deref(),
            Some("partial reason")
        );

        // Chunk 2: closes the tag, continues with normal content
        let raw2 = delta_chunk(Some("ing continued</think>normal text"), None);
        let c2 = acc.process("t", &raw2).unwrap().unwrap();
        assert_eq!(
            c2.choices[0].delta.reasoning.as_deref(),
            Some("ing continued")
        );
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("normal text"));
    }

    #[test]
    fn think_tag_no_tag_passthrough() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = delta_chunk(Some("no tags here"), None);
        let chunk = acc.process("t", &raw).unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("no tags here")
        );
        assert!(chunk.choices[0].delta.reasoning.is_none());
    }

    #[test]
    fn think_tag_split_across_three_chunks() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: before tag
        let c1 = acc
            .process("t", &delta_chunk(Some("before<think>"), None))
            .unwrap()
            .unwrap();
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("before"));
        assert!(c1.choices[0].delta.reasoning.is_none());

        // Chunk 2: inside tag, no close
        let c2 = acc
            .process("t", &delta_chunk(Some("thinking..."), None))
            .unwrap()
            .unwrap();
        assert!(c2.choices[0].delta.content.is_none());
        assert_eq!(
            c2.choices[0].delta.reasoning.as_deref(),
            Some("thinking...")
        );

        // Chunk 3: close tag + after
        let c3 = acc
            .process("t", &delta_chunk(Some("</think>after"), None))
            .unwrap()
            .unwrap();
        // Empty reasoning (closing tag with no content before it)
        assert!(c3.choices[0].delta.reasoning.is_none());
        assert_eq!(c3.choices[0].delta.content.as_deref(), Some("after"));
    }
}
