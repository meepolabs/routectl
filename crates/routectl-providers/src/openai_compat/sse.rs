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
///
/// `reasoning_index` is a per-stream, monotonically incrementing counter
/// owned by the streaming caller (see `stream()`); dialects that lift
/// streamed reasoning thread it into each emitted detail's `index`.
pub fn parse_chunk(
    id: &str,
    raw: &str,
    dialect: ReasoningDialect,
    reasoning_index: &mut u32,
) -> Result<Option<ChatChunk>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
        return Ok(None);
    }

    let mut val: Value = serde_json::from_str(trimmed)
        .map_err(|e| Error::Streaming(format!("provider `{id}`: SSE JSON: {e}")))?;

    // Detect mid-stream error envelope before any normalization. Some
    // upstreams (Azure content filter, OpenRouter overload, NIM auth-wall)
    // emit a JSON error object inside a 200-OK SSE stream rather than a
    // top-level HTTP error. Treating it as a bad ChatChunk deserialize
    // would produce a misleading NormalizeResponse error.
    if val.get("error").is_some() {
        let status = val
            .pointer("/error/code")
            .and_then(|v| v.as_u64())
            .map(|n| n as u16)
            .unwrap_or(502);
        let message = val
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("upstream error in stream")
            .to_string();
        return Err(Error::upstream(id, status, message));
    }

    // Coalesce reasoning_content -> reasoning across all delta objects so
    // serde sees a single canonical key and dialect-specific lifters can
    // operate on a uniform shape.
    coalesce_chunk_reasoning_keys(&mut val);

    // Lift OpenAI/DeepSeek usage sub-bags into canonical typed slots
    // BEFORE serde deserialization. `UsageDelta` does not carry a
    // `#[serde(flatten)] extras` catchall, so unknown sub-fields are
    // silently dropped at deserialize time -- without this lift, the
    // terminal-chunk's `completion_tokens_details.reasoning_tokens`
    // and `prompt_cache_hit_tokens` / `prompt_tokens_details
    // .cached_tokens` would never reach canonical.
    lift_chunk_usage_subbags(&mut val);

    apply_chunk_dialect(id, &mut val, dialect, reasoning_index)?;

    let chunk: ChatChunk = serde_json::from_value(val)
        .map_err(|e| Error::normalize_response(id, format!("chunk deserialize: {e}")))?;

    Ok(Some(chunk))
}

/// JSON-side mirror of `response::lift_and_strip_usage_extras` for the
/// SSE path. Operates on the chunk JSON before serde deserialization
/// because `UsageDelta` has no extras catchall; deserialization would
/// otherwise silently drop the sub-bags. Idempotent: only writes the
/// top-level fields when they are not already populated by the
/// upstream.
fn lift_chunk_usage_subbags(val: &mut Value) {
    let Some(usage) = val.get_mut("usage").and_then(|v| v.as_object_mut()) else {
        return;
    };

    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(|v| v.as_u64());
    if let Some(n) = reasoning_tokens {
        // Drop a present `null` sentinel before `or_insert`: a top-level
        // `reasoning_tokens: null` from the upstream would otherwise block
        // the sub-bag lift because `entry().or_insert()` treats an occupied
        // entry -- even one holding `null` -- as already set.
        if matches!(usage.get("reasoning_tokens"), Some(Value::Null)) {
            usage.remove("reasoning_tokens");
        }
        usage.entry("reasoning_tokens").or_insert(Value::from(n));
    }

    let cache_read = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
        });
    if let Some(n) = cache_read {
        // Same null-sentinel guard as `reasoning_tokens` above.
        if matches!(usage.get("cache_read_input_tokens"), Some(Value::Null)) {
            usage.remove("cache_read_input_tokens");
        }
        usage
            .entry("cache_read_input_tokens")
            .or_insert(Value::from(n));
    }

    // Sub-bags themselves are unknown to UsageDelta and would be
    // dropped by serde regardless; an explicit remove makes the
    // intention readable in the trace and keeps any future extras
    // flatten on UsageDelta from accidentally readmitting them.
    for k in [
        "prompt_cache_hit_tokens",
        "prompt_cache_miss_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
    ] {
        usage.remove(k);
    }
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

fn apply_chunk_dialect(
    id: &str,
    val: &mut Value,
    dialect: ReasoningDialect,
    reasoning_index: &mut u32,
) -> Result<()> {
    dialect.as_dyn().apply_chunk(id, val, reasoning_index)
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
///
/// The accumulator is correct across chunk boundaries that fall INSIDE
/// a tag: e.g. one chunk delivers `"<thi"` and the next delivers
/// `"nk>secret</think>"`. Without buffering, the partial `"<thi"` would
/// be emitted as visible content because `find("<think>")` would not
/// match. The accumulator holds back the longest suffix of the
/// pending output that could be a partial `<think>` (Outside) or
/// `</think>` (Inside) tag, and prepends it on the next call.
pub struct ThinkTagAccumulator {
    state: ThinkState,
    chunk_index: u32,
    /// Bytes held back from the previous chunk because they could be
    /// a partial tag. Bounded by the longer tag length (8 bytes for
    /// `</think>`) so this never grows beyond a handful of chars.
    pending: String,
}

impl Default for ThinkTagAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkTagAccumulator {
    pub fn new() -> Self {
        Self {
            state: ThinkState::default(),
            chunk_index: 0,
            pending: String::new(),
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
                .and_then(|r| serde_json::from_value(serde_json::Value::String(r.into())).ok());
            let tool_calls = choice_val
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .filter(|v| !v.is_null())
                .and_then(|v| v.as_array().cloned());

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
                // saturating_add so a multi-day-running stream
                // (4B+ reasoning chunks) wraps to a no-op rather
                // than silently colliding with index 0 on rollover
                // and breaking downstream consumers that key on
                // index uniqueness within a stream.
                self.chunk_index = self.chunk_index.saturating_add(1);
            }

            new_choices.push(ChunkChoice {
                index,
                delta,
                finish_reason,
                matched_stop_sequence: None,
            });
        }

        Ok(Some(ChatChunk {
            id: chunk_id,
            model,
            choices: new_choices,
            usage: None,
            opaque_events: Vec::new(),
        }))
    }

    /// Split `text` into (outside_think_content, inside_think_content) while
    /// advancing the internal `<think>` / `</think>` state machine.
    ///
    /// Handles three cases:
    ///   1. Tag boundary falls mid-chunk (e.g. only `<think>` arrives with no `</think>`).
    ///   2. Full `<think>...</think>` inside one chunk.
    ///   3. Mixed: text before tag + tag content + text after tag.
    ///
    /// Cross-chunk safety: a partial tag at the END of `text` (e.g.
    /// `"...prefix <thi"`) is held back in `self.pending` and
    /// prepended on the next call so a `</thi` + `nk>secret</think>`
    /// split is still detected and the secret stays in `inside`.
    fn split_think_tags(&mut self, text: &str) -> (String, String) {
        // Prepend any held-back partial tag from the previous chunk.
        let pending = std::mem::take(&mut self.pending);
        let combined: String;
        let combined_ref: &str = if pending.is_empty() {
            text
        } else {
            combined = format!("{pending}{text}");
            &combined
        };

        let mut outside = String::new();
        let mut inside = String::new();
        let mut remaining = combined_ref;

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

        // Hold back any trailing partial-tag prefix so a tag straddling
        // a chunk boundary isn't missed. The candidate buffer is whichever
        // string we just appended into based on terminal state.
        match self.state {
            ThinkState::Outside => {
                let cut = longest_partial_tag_suffix(&outside, "<think>");
                if cut > 0 {
                    self.pending = outside[outside.len() - cut..].to_string();
                    outside.truncate(outside.len() - cut);
                }
            }
            ThinkState::Inside => {
                let cut = longest_partial_tag_suffix(&inside, "</think>");
                if cut > 0 {
                    self.pending = inside[inside.len() - cut..].to_string();
                    inside.truncate(inside.len() - cut);
                }
            }
        }

        (outside, inside)
    }
}

/// Return the byte length of the longest suffix of `s` that is a
/// non-empty prefix of `tag`. O(tag.len()^2), bounded by the tag
/// length (max 8 for `</think>`) so cost is constant per call.
fn longest_partial_tag_suffix(s: &str, tag: &str) -> usize {
    let max_len = tag.len().min(s.len());
    for take in (1..=max_len).rev() {
        let start = s.len() - take;
        if !s.is_char_boundary(start) {
            continue;
        }
        let suffix = &s[start..];
        // A full tag match (suffix == tag) means the tag's already
        // been processed by the main loop; we should never reach
        // here with a complete tag, but guard against it just in
        // case to avoid holding back a complete tag forever.
        if suffix.len() < tag.len() && tag.starts_with(suffix) {
            return take;
        }
    }
    0
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
        assert!(parse_chunk("t", "[DONE]", ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .is_none());
        assert!(
            parse_chunk("t", "  [DONE]  ", ReasoningDialect::OpenAi, &mut 0)
                .unwrap()
                .is_none()
        );
        assert!(parse_chunk("t", "", ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn openai_basic_delta() {
        let raw = delta_chunk(Some("hello"), None);
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
        assert!(chunk.choices[0].delta.reasoning_details.is_empty());
    }

    #[test]
    fn deepseek_lifts_reasoning_content_in_delta() {
        let raw = delta_chunk(Some("answer"), Some("chain of thought"));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::DeepSeek, &mut 0)
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
        let chunk = parse_chunk("t", &raw, ReasoningDialect::Vllm, &mut 0)
            .unwrap()
            .unwrap();
        let details = &chunk.choices[0].delta.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
    }

    /// Pin the streaming reasoning-index contract: successive DeepSeek
    /// reasoning chunks driven through `parse_chunk` with a single shared
    /// per-stream counter (exactly as `stream()` threads it) must carry
    /// `index` 0, 1, 2, ... -- NOT all 0. Before the fix the lifter
    /// hardcoded index 0, collapsing every streamed delta onto one block.
    #[test]
    fn deepseek_streaming_reasoning_detail_index_increments() {
        let mut reasoning_index: u32 = 0;
        for expected in [0u32, 1, 2] {
            let raw = delta_chunk(Some("answer"), Some("step"));
            let chunk = parse_chunk("t", &raw, ReasoningDialect::DeepSeek, &mut reasoning_index)
                .unwrap()
                .unwrap();
            let details = &chunk.choices[0].delta.reasoning_details;
            assert_eq!(details.len(), 1);
            assert_eq!(
                details[0].index,
                Some(expected),
                "streamed reasoning detail index must increment per chunk"
            );
        }
    }

    /// A chunk with no reasoning content must NOT advance the counter, so
    /// the index stays aligned to actual reasoning deltas (0, then 1 after
    /// the gap -- not 0 then 2).
    #[test]
    fn vllm_streaming_reasoning_index_skips_non_reasoning_chunks() {
        let mut reasoning_index: u32 = 0;

        let c0 = parse_chunk(
            "t",
            &delta_chunk(None, Some("first")),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c0.choices[0].delta.reasoning_details[0].index, Some(0));

        // Plain content chunk: no reasoning -> counter unchanged.
        let c1 = parse_chunk(
            "t",
            &delta_chunk(Some("visible"), None),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert!(c1.choices[0].delta.reasoning_details.is_empty());

        let c2 = parse_chunk(
            "t",
            &delta_chunk(None, Some("second")),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c2.choices[0].delta.reasoning_details[0].index, Some(1));
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

    /// Pin: open `<think>` tag SPLIT across chunk boundaries.
    /// Without buffering, the partial `<thi` in chunk 1 emits as
    /// visible content; chunk 2 (`nk>secret</think>`) also fails to
    /// match `<think>` and goes to visible content, LEAKING the
    /// reasoning text. The accumulator buffers the partial-tag suffix
    /// and prepends it on the next call.
    #[test]
    fn think_open_tag_split_across_chunks_does_not_leak() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: only the first 4 bytes of `<think>` arrive.
        let c1 = acc
            .process("t", &delta_chunk(Some("<thi"), None))
            .unwrap()
            .unwrap();
        // The partial `<thi` is held back -- nothing visible yet.
        assert!(c1.choices[0].delta.content.is_none());
        assert!(c1.choices[0].delta.reasoning.is_none());

        // Chunk 2: rest of the open tag + secret + close + visible.
        let c2 = acc
            .process("t", &delta_chunk(Some("nk>secret</think>visible"), None))
            .unwrap()
            .unwrap();
        // `secret` MUST be in reasoning, NOT visible content.
        assert_eq!(c2.choices[0].delta.reasoning.as_deref(), Some("secret"));
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("visible"));
    }

    /// Pin: close `</think>` tag SPLIT across chunk boundaries. The
    /// reasoning text up to the partial close must not flow back as
    /// visible content; the close tag must be detected on the next
    /// chunk.
    #[test]
    fn think_close_tag_split_across_chunks() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: open tag + reasoning + first few bytes of close tag.
        let c1 = acc
            .process("t", &delta_chunk(Some("<think>reason</thi"), None))
            .unwrap()
            .unwrap();
        // `reason` is reasoning. The `</thi` is held back, so reasoning
        // emitted on this chunk is just `reason` (no extra suffix).
        assert_eq!(c1.choices[0].delta.reasoning.as_deref(), Some("reason"));
        assert!(c1.choices[0].delta.content.is_none());

        // Chunk 2: rest of close tag + visible.
        let c2 = acc
            .process("t", &delta_chunk(Some("nk>after"), None))
            .unwrap()
            .unwrap();
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("after"));
        assert!(c2.choices[0].delta.reasoning.is_none());
    }

    /// Pin: a one-character chunk that happens to be `<` does not
    /// flood `pending` with arbitrary content -- the buffer is
    /// bounded by the tag length.
    #[test]
    fn think_tag_lone_lt_holds_back_at_most_tag_length() {
        let mut acc = ThinkTagAccumulator::new();
        let _ = acc
            .process("t", &delta_chunk(Some("<"), None))
            .unwrap()
            .unwrap();
        // Now follow with content that is NOT a think tag.
        let c = acc
            .process("t", &delta_chunk(Some("not a think tag"), None))
            .unwrap()
            .unwrap();
        // The held-back `<` flows through with the next chunk.
        assert_eq!(
            c.choices[0].delta.content.as_deref(),
            Some("<not a think tag")
        );
    }

    /// Pin: innocent content that ends with a partial-tag-looking
    /// suffix gets delayed by exactly one chunk (no leak, no loss).
    #[test]
    fn think_tag_innocent_partial_suffix_delayed_one_chunk() {
        let mut acc = ThinkTagAccumulator::new();
        let c1 = acc
            .process("t", &delta_chunk(Some("say <th"), None))
            .unwrap()
            .unwrap();
        // `<th` looks like a partial open tag; held back. `say ` flows.
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("say "));

        let c2 = acc
            .process("t", &delta_chunk(Some("anks!"), None))
            .unwrap()
            .unwrap();
        // Combined `<thanks!` is not a tag; whole thing flows visibly.
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("<thanks!"));
    }

    // --- Terminal-chunk usage sub-bag lift tests ---

    fn terminal_chunk_with_usage(usage: serde_json::Value) -> String {
        json!({
            "id": "chunk-final",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": usage
        })
        .to_string()
    }

    /// OpenAI terminal chunk delivers `completion_tokens_details
    /// .reasoning_tokens` in the final `usage` object. `UsageDelta`
    /// has no extras catchall; the sub-bag must be lifted into the
    /// canonical typed `reasoning_tokens` field BEFORE serde would
    /// otherwise drop it.
    #[test]
    fn terminal_chunk_lifts_reasoning_tokens() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "completion_tokens_details": {"reasoning_tokens": 7}
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    /// DeepSeek terminal chunk delivers `prompt_cache_hit_tokens`
    /// directly on `usage`. Lift into canonical `cache_read_input_tokens`.
    #[test]
    fn terminal_chunk_lifts_cache_read_from_deepseek() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::DeepSeek, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    /// OpenAI terminal chunk delivers `prompt_tokens_details
    /// .cached_tokens`. Lift into canonical `cache_read_input_tokens`.
    #[test]
    fn terminal_chunk_lifts_cache_read_from_openai_prompt_tokens_details() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(64));
    }

    /// Idempotency mirror of the response-side test: when the
    /// upstream already set a top-level canonical field, the lift
    /// from the sub-bag does NOT overwrite it.
    #[test]
    fn terminal_chunk_lift_does_not_clobber_already_set_field() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "reasoning_tokens": 11,
            "completion_tokens_details": {"reasoning_tokens": 99}
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.usage.unwrap().reasoning_tokens, Some(11));
    }

    /// Regression: when the upstream emits `reasoning_tokens: null` at
    /// the top level (a present-but-null sentinel) the sub-bag lift from
    /// `completion_tokens_details.reasoning_tokens` must still land.
    /// Before the fix, `entry().or_insert()` treated the null-occupied
    /// entry as already set and silently dropped the sub-bag value.
    #[test]
    fn terminal_chunk_null_sentinel_does_not_block_subbag_lift() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "reasoning_tokens": null,
            "completion_tokens_details": {"reasoning_tokens": 5}
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.usage.unwrap().reasoning_tokens,
            Some(5),
            "null sentinel must be replaced by sub-bag value"
        );
    }

    /// Regression: when the upstream emits `cache_read_input_tokens: null`
    /// at the top level the sub-bag lift must still set the field.
    #[test]
    fn terminal_chunk_null_cache_sentinel_does_not_block_subbag_lift() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "cache_read_input_tokens": null,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        let chunk = parse_chunk("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.usage.unwrap().cache_read_input_tokens,
            Some(64),
            "null cache sentinel must be replaced by sub-bag value"
        );
    }

    /// A mid-stream JSON error envelope (Azure content filter, OpenRouter
    /// overload, NIM auth-wall) must produce `Error::Upstream`, not
    /// `Error::NormalizeResponse("chunk deserialize: ...")`.
    #[test]
    fn mid_stream_error_envelope_returns_upstream_error() {
        let raw = json!({
            "error": {
                "message": "content management policy violation",
                "code": 403
            }
        })
        .to_string();
        let err = parse_chunk("test-provider", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream {
                provider,
                status,
                body,
            } => {
                assert_eq!(provider, "test-provider");
                assert_eq!(status, 403);
                assert!(
                    body.contains("content management policy violation"),
                    "error body must contain upstream message, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// When the error envelope has no numeric `code`, status defaults to 502.
    #[test]
    fn mid_stream_error_envelope_non_numeric_code_defaults_to_502() {
        let raw = json!({
            "error": {
                "message": "service overloaded",
                "code": "rate_limit_exceeded"
            }
        })
        .to_string();
        let err = parse_chunk("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 502),
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }
}
