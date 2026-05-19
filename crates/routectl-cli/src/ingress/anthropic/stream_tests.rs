use serde_json::json;

use crate::ingress::anthropic::{AnthropicIngress, ANTHROPIC_FORMAT};
use crate::ingress::IngressAdapter;

use super::*;

fn fresh_state() -> AnthropicStreamState {
    AnthropicStreamState::default()
}

// -------- request parsing --------

#[test]
fn stream_second_finish_reason_drops_when_pending_already_set() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    // Body text.
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    // First finish_reason (no usage) -- buffers.
    let _ = render_chunk_internal(text_chunk("", Some("stop")), &mut s).unwrap();
    assert_eq!(s.pending_finish_reason.as_deref(), Some("stop"));
    // Second finish_reason chunk (different reason, no usage) --
    // must be dropped; the buffered "stop" must still hold.
    let dup_events = render_chunk_internal(text_chunk("", Some("tool_calls")), &mut s).unwrap();
    // The chunk produces a few cleanup events (flush + close are
    // re-fired, harmlessly) but no new buffered fr.
    let dup_names: Vec<&str> = dup_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert!(
        !dup_names.contains(&"message_delta"),
        "second finish_reason must not produce a message_delta: {dup_names:?}",
    );
    assert_eq!(
        s.pending_finish_reason.as_deref(),
        Some("stop"),
        "first-wins: original finish_reason preserved",
    );
    // Usage chunk flushes with the ORIGINAL "stop", not the dropped "tool_calls".
    let usage_chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
    };
    let flush_events = render_chunk_internal(usage_chunk, &mut s).unwrap();
    let flush_names: Vec<&str> = flush_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert_eq!(flush_names, vec!["message_delta", "message_stop"]);
    let payload: Value = serde_json::from_str(&flush_events[0].data).unwrap();
    // "stop" maps to Anthropic "end_turn"; "tool_calls" would have mapped to "tool_use".
    assert_eq!(payload["delta"]["stop_reason"], "end_turn");
}

/// Review follow-up to Bug B: chunks arriving after `message_stop`
/// has fired must be DROPPED entirely (no content_block_*,
/// no message_delta), regardless of whether they carry deltas,
/// finish_reasons, or usage. A WARN log surfaces the misbehaving
/// upstream.
#[test]
fn stream_chunks_after_message_stop_are_dropped() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    // Normal stream: text + finish-with-inline-usage to close cleanly.
    let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![routectl_core::ChunkChoice {
            index: 0,
            delta: routectl_core::ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
    };
    let close_events = render_chunk_internal(closing, &mut s).unwrap();
    let close_names: Vec<&str> = close_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert!(close_names.contains(&"message_stop"));
    assert!(s.finished, "stream should be in finished state");

    // Straggler 1: text delta after stop. Must produce no events.
    let stray_text = render_chunk_internal(text_chunk(" more text", None), &mut s).unwrap();
    assert!(
        stray_text.is_empty(),
        "post-stop text chunk must produce no events: {stray_text:?}",
    );

    // Straggler 2: usage-only chunk after stop. Must produce no events.
    let stray_usage = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(20),
            completion_tokens: Some(10),
            total_tokens: Some(30),
            ..Default::default()
        }),
    };
    let stray_events = render_chunk_internal(stray_usage, &mut s).unwrap();
    assert!(
        stray_events.is_empty(),
        "post-stop usage chunk must produce no events: {stray_events:?}",
    );

    // Straggler 3: a second finish_reason chunk after stop. Must produce no events.
    let stray_fr = render_chunk_internal(text_chunk("", Some("tool_calls")), &mut s).unwrap();
    assert!(
        stray_fr.is_empty(),
        "post-stop finish_reason chunk must produce no events: {stray_fr:?}",
    );
}

// -------- streaming --------

fn ingress() -> AnthropicIngress {
    AnthropicIngress
}

fn text_chunk(text: &str, finish: Option<&str>) -> ChatChunk {
    use routectl_core::{ChunkChoice, ChunkDelta};
    ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: finish.map(|s| s.into()),
            matched_stop_sequence: None,
        }],
        usage: None,
    }
}

#[test]
fn stream_emits_message_start_then_text_block() {
    let mut s = fresh_state();
    let events = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta"
        ]
    );
}

/// Finish_reason WITHOUT usage on the same chunk: the renderer must
/// buffer the finish_reason and emit `message_delta + message_stop`
/// either on the next usage chunk OR at `render_eos`. Emitting
/// `message_delta(fr, None)` immediately and then a SECOND
/// `message_delta` after `message_stop` when a trailing usage
/// chunk arrives is a protocol violation -- the trailing delta
/// arrives post-stop.
#[test]
fn stream_finish_without_usage_defers_until_eos() {
    let mut state = ingress().new_stream_state();
    let _ = ingress()
        .render_chunk(text_chunk("hello", None), state.as_mut())
        .unwrap();
    let finish_events = ingress()
        .render_chunk(text_chunk("", Some("stop")), state.as_mut())
        .unwrap();
    let finish_names: Vec<&str> = finish_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    // Without usage, the finish chunk only closes the open block.
    assert_eq!(finish_names, vec!["content_block_stop"]);
    // Stream ends with no usage chunk: render_eos flushes the
    // buffered finish_reason as a delta (no usage) + stop.
    let eos_events = ingress().render_eos(state.as_mut());
    let eos_names: Vec<&str> = eos_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert_eq!(eos_names, vec!["message_delta", "message_stop"]);
}

/// Finish_reason WITH inline usage: emit `message_delta(fr, usage)`
/// + `message_stop` immediately on the same chunk. No deferral.
#[test]
fn stream_finish_with_inline_usage_emits_immediately() {
    use routectl_core::{ChunkChoice, ChunkDelta, UsageDelta};
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
    };
    let events = render_chunk_internal(closing, &mut s).unwrap();
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec!["content_block_stop", "message_delta", "message_stop"]
    );
}

/// OpenRouter / OpenAI pattern: finish_reason on one chunk, usage
/// on the next. The renderer must hold message_delta + message_stop
/// until the usage chunk arrives, then emit ONE message_delta
/// carrying BOTH stop_reason and usage, followed by message_stop.
/// Two message_deltas wrapping message_stop is a protocol violation.
#[test]
fn stream_finish_then_separate_usage_chunk_emits_single_delta() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    // Body text.
    let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
    // Finish chunk with no usage -- buffered, only close fires.
    let finish_events = render_chunk_internal(text_chunk("", Some("stop")), &mut s).unwrap();
    let finish_names: Vec<&str> = finish_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert_eq!(finish_names, vec!["content_block_stop"]);
    // Trailing usage-only chunk -- emits combined delta + stop.
    let usage_chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            ..Default::default()
        }),
    };
    let usage_events = render_chunk_internal(usage_chunk, &mut s).unwrap();
    let usage_names: Vec<&str> = usage_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert_eq!(usage_names, vec!["message_delta", "message_stop"]);
    // The single message_delta carries both stop_reason AND usage.
    let payload: Value = serde_json::from_str(&usage_events[0].data).unwrap();
    assert_eq!(payload["delta"]["stop_reason"], "end_turn");
    assert_eq!(payload["usage"]["input_tokens"], 100);
    assert_eq!(payload["usage"]["output_tokens"], 50);
}

#[test]
fn stream_eos_emits_message_stop_when_not_yet_finished() {
    let mut state = ingress().new_stream_state();
    // Drive at least one chunk so message_start fires.
    let _ = ingress()
        .render_chunk(text_chunk("hi", None), state.as_mut())
        .unwrap();
    let events = ingress().render_eos(state.as_mut());
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names, vec!["content_block_stop", "message_stop"]);
}

#[test]
fn stream_two_concurrent_tool_calls_each_get_their_own_block() {
    // Verify both tool calls open their own blocks
    // and arguments-deltas land on the right block index.
    use routectl_core::{ChunkChoice, ChunkDelta};
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![
                    json!({
                        "index": 0,
                        "id": "toolu_01",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{\"a\":"}
                    }),
                    json!({
                        "index": 1,
                        "id": "toolu_02",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"q\":"}
                    }),
                ]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
    };
    let mut s = fresh_state();
    let events = render_chunk_internal(chunk, &mut s).unwrap();
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names, vec!["message_start"]);
    assert_eq!(s.tool_blocks.len(), 2);
    assert_eq!(s.tool_blocks[0].id, "toolu_01");
    assert_eq!(s.tool_blocks[1].id, "toolu_02");
}

#[test]
fn stream_interleaved_tool_call_chunks_flush_in_valid_order_at_finish() {
    use routectl_core::{ChunkChoice, ChunkDelta};
    let mut s = fresh_state();
    let first = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![
                    json!({
                        "index": 0,
                        "id": "toolu_01",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{\"a\":"}
                    }),
                    json!({
                        "index": 1,
                        "id": "toolu_02",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"q\":"}
                    }),
                ]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
    };
    // Second chunk carries inline usage so the renderer emits
    // message_delta + message_stop immediately. Hosts that split
    // finish_reason and usage across two chunks are covered by
    // `stream_finish_then_separate_usage_chunk_emits_single_delta`.
    let second = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![
                    json!({
                        "index": 1,
                        "function": {"arguments": "\"rust\"}"}
                    }),
                    json!({
                        "index": 0,
                        "function": {"arguments": "1}"}
                    }),
                ]),
                ..Default::default()
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(routectl_core::UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
    };

    let _ = render_chunk_internal(first, &mut s).unwrap();
    let events = render_chunk_internal(second, &mut s).unwrap();
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
}

#[test]
fn usage_only_chunk_emits_null_stop_reason() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();
    let usage_only = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta::default()),
    };
    let events = render_chunk_internal(usage_only, &mut s).unwrap();
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert!(payload["delta"]["stop_reason"].is_null());
}

/// Closing chunk's `usage.prompt_tokens` must be rendered into
/// Anthropic's `input_tokens` on message_delta carries the RAW
/// input portion (cache_creation and cache_read are separate
/// fields per spec). routectl computes raw = prompt_tokens -
/// cache_creation - cache_read so the wire body matches the
/// Anthropic spec rather than echoing canonical's summed
/// prompt_tokens. A receiver-side anthropic SSE state machine
/// (`anthropic_api/sse.rs`) sums raw + cache fields back into
/// canonical prompt_tokens.
#[test]
fn message_delta_renders_raw_input_tokens_per_anthropic_spec() {
    use routectl_core::{schema::CacheCreation, UsageDelta};
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(12345),
            completion_tokens: Some(50),
            total_tokens: Some(12395),
            cache_creation_input_tokens: Some(100),
            cache_read_input_tokens: Some(200),
            cache_creation: Some(CacheCreation {
                ephemeral_5m_input_tokens: Some(50),
                ephemeral_1h_input_tokens: Some(50),
            }),
            ..Default::default()
        }),
    };
    let events = render_chunk_internal(closing, &mut s).unwrap();
    let delta_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta emitted");
    let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
    let wire_usage = &payload["usage"];
    // raw input = 12345 - 100 - 200 = 12045 (per Anthropic spec).
    assert_eq!(wire_usage["input_tokens"], 12045);
    assert_eq!(wire_usage["output_tokens"], 50);
    assert_eq!(wire_usage["cache_creation_input_tokens"], 100);
    assert_eq!(wire_usage["cache_read_input_tokens"], 200);
}

#[test]
fn stream_distinct_thinking_indices_emit_separate_blocks() {
    // Two reasoning details with different `index` values must
    // emit as TWO Anthropic content blocks, not one merged
    // block. Pre-fix, `ensure_block` only checked
    // `OpenBlockKind == Thinking` and merged them. Now the
    // OpenBlockKind variant carries `detail_index` so the
    // second detail forces a content_block_stop on the first
    // block and a content_block_start on the new block.
    use routectl_core::{
        schema::{ChunkChoice, ChunkDelta},
        ReasoningDetail, ReasoningDetailKind,
    };
    let first = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: None,
                    format: Some(ANTHROPIC_FORMAT.into()),
                    index: Some(0),
                    payload: json!({"text": "first thought"}),
                }],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
    };
    let second = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: None,
                    format: Some(ANTHROPIC_FORMAT.into()),
                    index: Some(1),
                    payload: json!({"text": "second thought"}),
                }],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
    };

    let mut s = fresh_state();
    let first_events = render_chunk_internal(first, &mut s).unwrap();
    let second_events = render_chunk_internal(second, &mut s).unwrap();

    // First chunk: message_start + content_block_start (idx=0 thinking)
    // + content_block_delta. Find the start event and capture its idx.
    let first_start_idx = first_events
        .iter()
        .find_map(|ev| {
            ev.event
                .as_deref()
                .filter(|n| *n == "content_block_start")
                .map(|_| ())
        })
        .map(|_| 0_usize);
    assert!(
        first_start_idx.is_some(),
        "first chunk must open a thinking block; got events: {:?}",
        first_events
            .iter()
            .map(|e| e.event.as_deref())
            .collect::<Vec<_>>()
    );

    // Second chunk MUST emit content_block_stop for the previous
    // index THEN content_block_start for a new index. Without
    // the fix, neither would appear (just delta on the same
    // open block).
    let names: Vec<&str> = second_events
        .iter()
        .filter_map(|ev| ev.event.as_deref())
        .collect();
    let stop_pos = names.iter().position(|n| *n == "content_block_stop");
    let start_pos = names.iter().position(|n| *n == "content_block_start");
    assert!(
            stop_pos.is_some(),
            "second-chunk thinking with new detail_index must emit content_block_stop; events: {names:?}"
        );
    assert!(
            start_pos.is_some(),
            "second-chunk thinking with new detail_index must emit content_block_start; events: {names:?}"
        );
    assert!(
        stop_pos.unwrap() < start_pos.unwrap(),
        "content_block_stop must precede content_block_start; events: {names:?}"
    );
}

#[test]
fn stream_tool_call_index_above_cap_returns_streaming_error() {
    // note: tool_blocks Vec growth bound.
    use routectl_core::{ChunkChoice, ChunkDelta};
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![json!({
                    "index": 1_000_000_u64,
                    "id": "toolu_evil",
                    "type": "function",
                    "function": {"name": "x", "arguments": "{}"}
                })]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
    };
    let mut s = fresh_state();
    let err = render_chunk_internal(chunk, &mut s).unwrap_err();
    assert!(
        err.to_string().contains("exceeds maximum"),
        "expected streaming error with 'exceeds maximum', got: {err}"
    );
}
