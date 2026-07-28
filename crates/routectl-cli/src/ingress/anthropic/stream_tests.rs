use serde_json::json;

use crate::ingress::anthropic::{ANTHROPIC_FORMAT, AnthropicIngress};
use crate::ingress::{IngressAdapter, STREAM_ERROR_TYPE, StreamErrorClass, StreamRequestContext};

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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
            finish_reason: finish.map(std::convert::Into::into),
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
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
    let mut state = ingress().new_stream_state(&StreamRequestContext::default());
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
    let mut state = ingress().new_stream_state(&StreamRequestContext::default());
    // Drive at least one chunk so message_start fires.
    let _ = ingress()
        .render_chunk(text_chunk("hi", None), state.as_mut())
        .unwrap();
    let events = ingress().render_eos(state.as_mut());
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names, vec!["content_block_stop", "message_stop"]);
}

/// An upstream stream that produced zero chunks must still emit a valid
/// Anthropic SSE frame sequence: a synthetic `message_start` followed by
/// `message_stop`. Bare `message_stop` on an empty stream violates the
/// spec and breaks SDK consumers that count frames.
#[test]
fn stream_eos_on_empty_stream_emits_synthetic_message_start_then_stop() {
    // Arrange: fresh state, no chunks rendered (started=false,
    // finished=false).
    let mut state = ingress().new_stream_state(&StreamRequestContext::default());

    // Act
    let events = ingress().render_eos(state.as_mut());

    // Assert: the frame sequence begins with message_start and ends
    // with message_stop.
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(names, vec!["message_start", "message_stop"]);
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let events = render_chunk_internal(usage_only, &mut s).unwrap();
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert!(payload["delta"]["stop_reason"].is_null());
}

#[test]
fn stream_content_filter_finish_emits_refusal_stop_reason() {
    use routectl_core::{ChunkChoice, ChunkDelta, UsageDelta};
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "gpt-5".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("content_filter".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let events = render_chunk_internal(closing, &mut s).unwrap();
    let delta_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta emitted");
    let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
    assert_eq!(payload["delta"]["stop_reason"], "refusal");
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
    use routectl_core::{UsageDelta, schema::CacheCreation};
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        ReasoningDetail, ReasoningDetailKind,
        schema::{ChunkChoice, ChunkDelta},
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        opaque_events: Vec::new(),
        upstream_meta: None,
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
        .map(|()| 0_usize);
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

/// Build a single-chunk stream carrying one reasoning detail, so
/// `render_chunk_internal` drives it through `emit_delta_events`.
fn reasoning_chunk(detail: routectl_core::ReasoningDetail) -> ChatChunk {
    use routectl_core::schema::{ChunkChoice, ChunkDelta};
    ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![detail],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

#[test]
fn encrypted_reasoning_emits_redacted_thinking_block_start_stop_pair_from_data() {
    // Arrange: an Encrypted reasoning detail carrying the Anthropic /
    // OpenRouter `data` payload. Anthropic clients 400 on malformed
    // thinking blocks, so this must round-trip as a stand-alone
    // redacted_thinking content_block_start + content_block_stop.
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    const ENC: &str = "EnCrYpTeD-REDACTED-PAYLOAD-7c1e";
    let chunk = reasoning_chunk(ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: None,
        format: Some(ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: json!({"data": ENC}),
    });
    let mut s = fresh_state();

    // Act
    let events = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert: a redacted_thinking block_start carrying the data, then a
    // matching block_stop -- no incremental delta in between.
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    let start_pos = names
        .iter()
        .position(|n| *n == "content_block_start")
        .expect("redacted_thinking must emit content_block_start");
    let stop_pos = names
        .iter()
        .position(|n| *n == "content_block_stop")
        .expect("redacted_thinking must emit content_block_stop");
    assert!(
        start_pos < stop_pos,
        "block_start must precede block_stop: {names:?}"
    );
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_start"))
        .unwrap();
    assert!(
        start.data.contains("\"type\":\"redacted_thinking\""),
        "block must be redacted_thinking: {}",
        start.data
    );
    assert!(
        start.data.contains(&format!("\"data\":\"{ENC}\"")),
        "encrypted payload must be carried verbatim: {}",
        start.data
    );
    assert!(
        !names.contains(&"content_block_delta"),
        "redacted_thinking has no incremental delta path: {names:?}"
    );
}

#[test]
fn encrypted_reasoning_lifts_data_from_encrypted_content_alias() {
    // Arrange: the OpenAI-Responses passthrough names the field
    // `encrypted_content`; the emitter must pick it up as the
    // redacted_thinking `data` when `data` is absent.
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    const ENC: &str = "OPENAI-RESPONSES-ENCRYPTED-4b2e";
    let chunk = reasoning_chunk(ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: None,
        format: Some(ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: json!({"encrypted_content": ENC}),
    });
    let mut s = fresh_state();

    // Act
    let events = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert: data lifted from the alias into the redacted_thinking block.
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_start"))
        .expect("content_block_start emitted");
    assert!(
        start.data.contains("\"type\":\"redacted_thinking\""),
        "block must be redacted_thinking: {}",
        start.data
    );
    assert!(
        start.data.contains(&format!("\"data\":\"{ENC}\"")),
        "encrypted_content alias must supply the data: {}",
        start.data
    );
}

#[test]
fn summary_reasoning_lifts_text_into_thinking_delta() {
    // Arrange: an OpenAI-Responses Summary detail. Its text must lift
    // into a thinking_delta on a thinking block (so cc renders it and
    // the round-trip history keeps it), not a redacted block.
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    const SUMMARY: &str = "high-level summary of the reasoning";
    let chunk = reasoning_chunk(ReasoningDetail {
        kind: ReasoningDetailKind::Summary,
        id: None,
        format: Some(ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: json!({"text": SUMMARY}),
    });
    let mut s = fresh_state();

    // Act
    let events = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert: a thinking block opens and a thinking_delta carries the
    // summary text.
    let names: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert!(
        names.contains(&"content_block_start"),
        "summary must open a thinking block: {names:?}"
    );
    let delta = events
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_delta"))
        .expect("summary must emit a content_block_delta");
    assert!(
        delta.data.contains("\"type\":\"thinking_delta\""),
        "summary lifts to thinking_delta: {}",
        delta.data
    );
    assert!(
        delta.data.contains(SUMMARY),
        "summary text must be carried: {}",
        delta.data
    );
    assert!(
        !delta.data.contains("redacted_thinking"),
        "summary must not render as redacted_thinking: {}",
        delta.data
    );
}

#[test]
fn stream_tool_call_index_above_cap_returns_streaming_error() {
    // tool_blocks Vec growth bound.
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
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let mut s = fresh_state();
    let err = render_chunk_internal(chunk, &mut s).unwrap_err();
    assert!(
        err.to_string().contains("exceeds maximum"),
        "expected streaming error with 'exceeds maximum', got: {err}"
    );
}

// -------- opaque-events replay --------

/// Build a chunk that carries only opaque_events (no canonical
/// content). Mirrors what the Anthropic-API egress surfaces when an
/// unknown content_block (e.g. server_tool_use) flows through the
/// pipeline: empty choices, populated opaque_events.
fn opaque_only_chunk(events: Vec<routectl_core::OpaqueSseEvent>) -> ChatChunk {
    ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: None,
        opaque_events: events,
        upstream_meta: None,
    }
}

#[test]
fn opaque_event_only_chunk_emits_start_two_deltas_and_stop() {
    // Arrange
    use routectl_core::OpaqueSseEvent;
    let events_in = vec![
        OpaqueSseEvent::ContentBlockStart {
            upstream_index: 7,
            type_tag: "server_tool_use".into(),
            raw_data: br#"{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{}}"#
                .to_vec(),
        },
        OpaqueSseEvent::ContentBlockDelta {
            upstream_index: 7,
            raw_delta: br#"{"type":"input_json_delta","partial_json":"\"q\":"}"#.to_vec(),
        },
        OpaqueSseEvent::ContentBlockDelta {
            upstream_index: 7,
            raw_delta: br#"{"type":"input_json_delta","partial_json":"\"x\""}"#.to_vec(),
        },
        OpaqueSseEvent::ContentBlockStop { upstream_index: 7 },
    ];
    let chunk = opaque_only_chunk(events_in);
    let mut s = fresh_state();

    // Act
    let out = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert
    let names: Vec<&str> = out.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
        ],
        "expected message_start + 1 start + 2 deltas + 1 stop, got {names:?}"
    );
    // The opaque mapping must be cleared after stop.
    assert!(
        s.opaque_index_map.is_empty(),
        "opaque_index_map should be empty after content_block_stop"
    );
}

#[test]
fn opaque_event_index_allocation_is_sequential() {
    // Arrange: two distinct opaque blocks back-to-back, each
    // start/delta/stop, with different upstream_index values. The
    // ingress must allocate sequential ingress indexes (N, N+1).
    use routectl_core::OpaqueSseEvent;
    let events_in = vec![
        OpaqueSseEvent::ContentBlockStart {
            upstream_index: 10,
            type_tag: "server_tool_use".into(),
            raw_data: br#"{"type":"server_tool_use","id":"srv_a","name":"web_search","input":{}}"#
                .to_vec(),
        },
        OpaqueSseEvent::ContentBlockDelta {
            upstream_index: 10,
            raw_delta: br#"{"type":"input_json_delta","partial_json":"\"a\""}"#.to_vec(),
        },
        OpaqueSseEvent::ContentBlockStop { upstream_index: 10 },
        OpaqueSseEvent::ContentBlockStart {
            upstream_index: 11,
            type_tag: "web_search_tool_result".into(),
            raw_data: br#"{"type":"web_search_tool_result","tool_use_id":"srv_a","content":[]}"#
                .to_vec(),
        },
        OpaqueSseEvent::ContentBlockDelta {
            upstream_index: 11,
            raw_delta: br#"{"type":"citations_delta","citation":{"url":"https://x"}}"#.to_vec(),
        },
        OpaqueSseEvent::ContentBlockStop { upstream_index: 11 },
    ];
    let chunk = opaque_only_chunk(events_in);
    let mut s = fresh_state();

    // Act
    let out = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert: scan only content_block_start events; their `index` field
    // must be 0 then 1 (fresh state, no message_start steals an index).
    let start_indexes: Vec<i64> = out
        .iter()
        .filter(|e| e.event.as_deref() == Some("content_block_start"))
        .map(|e| {
            let v: Value = serde_json::from_str(&e.data).unwrap();
            v["index"].as_i64().unwrap()
        })
        .collect();
    assert_eq!(
        start_indexes,
        vec![0, 1],
        "two opaque blocks must allocate sequential ingress indexes; got {start_indexes:?}"
    );
    // next_index advanced past both blocks.
    assert_eq!(s.next_index, 2);
    // All upstream entries cleared.
    assert!(s.opaque_index_map.is_empty());
}

#[test]
fn opaque_event_data_payload_is_byte_for_byte() {
    // Arrange: a ContentBlockStart whose raw_data is a specific JSON
    // object. The emitted SSE data: payload must contain those bytes
    // VERBATIM as the `content_block` field (no re-serialization,
    // no key-order rewrite).
    use routectl_core::OpaqueSseEvent;
    let raw =
        br#"{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{"query":"x"}}"#
            .to_vec();
    let raw_clone = raw.clone();
    let chunk = opaque_only_chunk(vec![OpaqueSseEvent::ContentBlockStart {
        upstream_index: 0,
        type_tag: "server_tool_use".into(),
        raw_data: raw,
    }]);
    let mut s = fresh_state();

    // Act
    let out = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert
    let start_event = out
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_start"))
        .expect("content_block_start emitted");
    let raw_str = std::str::from_utf8(&raw_clone).unwrap();
    let expected =
        format!("{{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{raw_str}}}");
    assert_eq!(
        start_event.data, expected,
        "opaque content_block raw_data must embed byte-for-byte"
    );
    // And just to be explicit: the raw bytes must be a substring of
    // the emitted payload (catches any escaping/encoding regression).
    assert!(
        start_event.data.contains(raw_str),
        "raw bytes must appear unchanged inside the SSE data payload"
    );
}

#[test]
fn empty_opaque_events_no_op() {
    // Arrange: a normal text chunk (canonical-only) -- the legacy
    // path. Asserts behavior is unchanged when opaque_events is empty.
    let mut s = fresh_state();

    // Act
    let out = render_chunk_internal(text_chunk("hello", None), &mut s).unwrap();

    // Assert: identical event sequence to the legacy unit test
    // `stream_emits_message_start_then_text_block`.
    let names: Vec<&str> = out.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta"
        ],
    );
    assert!(s.opaque_index_map.is_empty());
}

#[test]
fn opaque_event_delta_without_prior_start_warns_and_skips() {
    // Arrange: malformed input where a delta arrives without a
    // preceding start. The replay path MUST NOT terminate the stream;
    // it logs at WARN and skips the event. Subsequent canonical
    // content still emits.
    use routectl_core::{ChunkChoice, ChunkDelta, OpaqueSseEvent};
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("hi".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: vec![OpaqueSseEvent::ContentBlockDelta {
            upstream_index: 99,
            raw_delta: b"{\"type\":\"input_json_delta\"}".to_vec(),
        }],
        upstream_meta: None,
    };
    let mut s = fresh_state();

    // Act
    let out = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert: the orphan delta is skipped; canonical text block still
    // emits message_start + content_block_start + content_block_delta.
    let names: Vec<&str> = out.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
        ],
        "orphan opaque delta must be skipped without terminating; got {names:?}"
    );
}

// Note on `serialization_failure_on_one_event_does_not_terminate`:
// the opaque carrier holds `Vec<u8>`. The replay path's only
// failure mode is non-UTF-8 raw bytes (Anthropic SSE is JSON-over-
// UTF-8 by spec). We could craft `raw_data: vec![0xFF, 0xFE]` to hit
// the non-UTF-8 branch, but it's not a realistic SSE payload --
// the egress would never produce it. The orphan-delta test above
// already pins the don't-terminate-on-failure contract; pinning
// "non-UTF-8 raw bytes get skipped" too is overspecification of an
// internal defensive guard. Skipped intentionally.

/// LOW-2 fix: an opaque content block opened via an opaque
/// ContentBlockStart but never closed (the upstream stream ended cleanly
/// before its ContentBlockStop arrived) must be closed at EOS. Before the
/// fix, `render_eos` closed only the canonical `state.open` block and left
/// the opaque block unclosed on the wire before `message_stop`. After the
/// fix, `render_eos` emits a `content_block_stop` for each lingering
/// opaque entry, clears the map, then emits `message_stop`.
#[test]
fn render_eos_closes_lingering_opaque_block_before_message_stop() {
    use routectl_core::OpaqueSseEvent;
    let mut state = ingress().new_stream_state(&StreamRequestContext::default());
    // Open an opaque block (start only, no stop). The egress would
    // attach a ContentBlockStop normally; here the stream ends first.
    let chunk = opaque_only_chunk(vec![OpaqueSseEvent::ContentBlockStart {
        upstream_index: 3,
        type_tag: "server_tool_use".into(),
        raw_data: br#"{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{}}"#
            .to_vec(),
    }]);
    let _ = ingress().render_chunk(chunk, state.as_mut()).unwrap();

    // Act: natural EOS.
    let eos_events = ingress().render_eos(state.as_mut());
    let names: Vec<&str> = eos_events
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();

    // Assert: the opaque block is closed before message_stop.
    assert_eq!(
        names,
        vec!["content_block_stop", "message_stop"],
        "lingering opaque block must be closed before message_stop; got {names:?}"
    );
    // The content_block_stop must carry the opaque block's ingress index
    // (0, the first allocated index on a fresh state).
    let stop_event = eos_events
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_stop"))
        .expect("content_block_stop emitted");
    let payload: Value = serde_json::from_str(&stop_event.data).unwrap();
    assert_eq!(
        payload["index"], 0,
        "content_block_stop must reference the opaque block's ingress index"
    );
    assert_eq!(payload["type"], "content_block_stop");
}

/// Terminal INLINE-usage chunk path with a lingering opaque block: a
/// finish_reason and usage arrive on the same chunk while an opaque
/// content_block_start has been replayed but never stopped. The
/// terminal ordering must be EXACTLY
/// content_block_stop -> message_delta -> message_stop, the opaque map
/// drained (no unclosed block), and no post-stop events. Before the
/// fix, this path emitted message_delta -> message_stop without
/// closing the lingering opaque block, since the drain lived only in
/// render_eos (which message_stop's `finished = true` no-ops).
#[test]
fn inline_usage_terminal_closes_lingering_opaque_block() {
    use routectl_core::{ChunkChoice, ChunkDelta, OpaqueSseEvent, UsageDelta};
    let mut s = fresh_state();
    // Replay an opaque content_block_start with no matching stop.
    let open = opaque_only_chunk(vec![OpaqueSseEvent::ContentBlockStart {
        upstream_index: 3,
        type_tag: "server_tool_use".into(),
        raw_data: br#"{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{}}"#
            .to_vec(),
    }]);
    let _ = render_chunk_internal(open, &mut s).unwrap();
    assert!(
        !s.opaque_index_map.is_empty(),
        "opaque block must be open before the terminal chunk"
    );

    // Terminal chunk: finish_reason + inline usage on the same chunk.
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
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let out = render_chunk_internal(closing, &mut s).unwrap();
    let names: Vec<&str> = out.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec!["content_block_stop", "message_delta", "message_stop"],
        "inline-usage terminal path must close the lingering opaque block first; got {names:?}"
    );
    // The content_block_stop references the opaque block's ingress index (0).
    let stop_payload: Value = serde_json::from_str(&out[0].data).unwrap();
    assert_eq!(stop_payload["type"], "content_block_stop");
    assert_eq!(stop_payload["index"], 0);
    // Drained exactly once: no unclosed block remains.
    assert!(
        s.opaque_index_map.is_empty(),
        "opaque map must be drained after the terminal chunk"
    );
}

/// Terminal DEFERRED-usage chunk path with a lingering opaque block:
/// the finish_reason buffers on one chunk and usage arrives on a later
/// chunk, while an opaque content_block_start was replayed but never
/// stopped. The buffered-finish chunk must NOT close the opaque block
/// prematurely; the finalize (usage) chunk's ordering must be EXACTLY
/// content_block_stop -> message_delta -> message_stop, drained once.
#[test]
fn deferred_usage_terminal_closes_lingering_opaque_block() {
    use routectl_core::{ChunkChoice, ChunkDelta, OpaqueSseEvent, UsageDelta};
    let mut s = fresh_state();
    // Replay an opaque content_block_start with no matching stop.
    let open = opaque_only_chunk(vec![OpaqueSseEvent::ContentBlockStart {
        upstream_index: 3,
        type_tag: "server_tool_use".into(),
        raw_data: br#"{"type":"server_tool_use","id":"srv_01","name":"web_search","input":{}}"#
            .to_vec(),
    }]);
    let _ = render_chunk_internal(open, &mut s).unwrap();

    // finish_reason with no usage -> buffers; must not drain the opaque block yet.
    let finish = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let finish_out = render_chunk_internal(finish, &mut s).unwrap();
    let finish_names: Vec<&str> = finish_out
        .iter()
        .filter_map(|e| e.event.as_deref())
        .collect();
    assert!(
        finish_names.is_empty(),
        "buffered finish must not drain the opaque block; got {finish_names:?}"
    );
    assert!(
        !s.opaque_index_map.is_empty(),
        "opaque block must still be open after a buffered finish_reason"
    );

    // Trailing usage-only chunk finalizes the stream.
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
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let out = render_chunk_internal(usage_chunk, &mut s).unwrap();
    let names: Vec<&str> = out.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec!["content_block_stop", "message_delta", "message_stop"],
        "deferred-usage finalize must close the lingering opaque block first; got {names:?}"
    );
    let stop_payload: Value = serde_json::from_str(&out[0].data).unwrap();
    assert_eq!(stop_payload["type"], "content_block_stop");
    assert_eq!(stop_payload["index"], 0);
    assert!(
        s.opaque_index_map.is_empty(),
        "opaque map must be drained exactly once after the finalize chunk"
    );
}

// -------- terminal-error event on upstream mid-stream failure --------

/// Mid-stream upstream failure on the Anthropic ingress: the adapter
/// must emit ONE `event: error` named SSE event matching the
/// Anthropic Messages SSE spec
/// (`{"type":"error","error":{"type":"api_error","message":...}}`),
/// and NOTHING ELSE. Per the Anthropic spec, the error event is
/// itself terminal: no further events follow. Without this, SDK
/// consumers (Claude Code SDK) treat the connection close as a
/// truncated stream and retry up to 5 times.
#[test]
fn render_error_eos_returns_anthropic_error_event() {
    // Arrange
    let mut s = fresh_state();
    let error_msg = "upstream stream error (HTTP 529)";
    let class =
        StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));

    // Act
    let events = render_error_eos_internal(&mut s, &error_msg, &class);

    // Assert
    // Exactly one event.
    assert_eq!(events.len(), 1, "Anthropic error event is terminal");
    // Named `event: error` per Anthropic SSE spec.
    assert_eq!(events[0].event.as_deref(), Some("error"));
    // Payload is `{"type":"error","error":{"type":"api_error","message":...}}`.
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(payload["type"], "error");
    assert_eq!(payload["error"]["type"], STREAM_ERROR_TYPE);
    assert!(
        payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream stream error")
    );
    // State is marked finished so any straggler chunks are dropped
    // by the `render_chunk_internal` post-stop guard.
    assert!(
        s.finished,
        "state.finished must be set so stragglers are dropped"
    );
}

/// Layer D: a 503/529 upstream stream error must carry
/// `overloaded_error` on the Anthropic terminal error event so stream
/// and non-stream classification agree.
#[test]
fn render_error_eos_emits_overloaded_for_529() {
    // Arrange
    let mut s = fresh_state();
    let class = StreamErrorClass::from_error(&routectl_core::Error::upstream("p", 529, "busy"));

    // Act
    let events = render_error_eos_internal(&mut s, &"boom", &class);

    // Assert
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(payload["error"]["type"], "overloaded_error");
}

/// Layer D: a non-overloaded upstream stream error stays `api_error`
/// on the Anthropic terminal event.
#[test]
fn render_error_eos_emits_api_error_for_502() {
    // Arrange
    let mut s = fresh_state();
    let class = StreamErrorClass::from_error(&routectl_core::Error::upstream("p", 502, "bad gw"));

    // Act
    let events = render_error_eos_internal(&mut s, &"boom", &class);

    // Assert
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(payload["error"]["type"], "api_error");
}

/// Counterpart: a chunk arriving after `render_error_eos` must be
/// dropped. Pins the `state.finished = true` invariant against any
/// future refactor that forgets to mark the state as terminal.
#[test]
fn render_error_eos_marks_state_finished_so_stragglers_dropped() {
    // Arrange
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    let class =
        StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));
    let _ = render_error_eos_internal(&mut s, &"upstream stream error", &class);

    // Act: a misbehaving upstream might deliver a straggler chunk
    // after the task tried to terminate -- defensive only since
    // the spawned task returns immediately after `render_error_eos`,
    // but the contract still holds inside the state machine.
    let stray = render_chunk_internal(text_chunk(" more", None), &mut s).unwrap();

    // Assert
    assert!(
        stray.is_empty(),
        "post-error chunk must produce no events: {stray:?}"
    );
}

/// Belt-and-suspenders sanitization. Control characters in the
/// caller's error message must be filtered before reaching the
/// SSE wire bytes, otherwise an attacker-controlled upstream body
/// could break SSE framing or forge log lines on downstream
/// text-format subscribers.
#[test]
fn render_error_eos_filters_control_chars_via_sanitize_for_log() {
    // Arrange: a message containing CR, LF, and an ANSI escape.
    let mut s = fresh_state();
    let dirty = "upstream stream error\r\n\x1b[31mexploit\x1b[0m";
    let class =
        StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));

    // Act
    let events = render_error_eos_internal(&mut s, &dirty, &class);

    // Assert: the emitted message has no raw \r, \n, or ESC bytes.
    let payload: Value = serde_json::from_str(&events[0].data).unwrap();
    let msg = payload["error"]["message"].as_str().unwrap();
    assert!(!msg.contains('\r'), "raw CR must be filtered: {msg:?}");
    assert!(!msg.contains('\n'), "raw LF must be filtered: {msg:?}");
    assert!(
        !msg.contains('\x1b'),
        "raw ESC byte must be filtered: {msg:?}"
    );
}

// -------- audit findings: behavioral fixes --------

/// Finding #1 (bug): closing `message_delta` must carry `input_tokens`
/// even when the upstream `UsageDelta` omits `prompt_tokens`. Before the
/// fix, the `if let Some(prompt) = u.prompt_tokens` guard silently dropped
/// the key, which violates the Anthropic spec requirement. After the fix,
/// a missing `prompt_tokens` defaults to 0 and `input_tokens: 0` always
/// appears on the wire.
#[test]
fn message_delta_missing_prompt_tokens_still_emits_input_tokens_zero() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    // UsageDelta with completion_tokens but NO prompt_tokens.
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: None,
            completion_tokens: Some(20),
            total_tokens: None,
            ..Default::default()
        }),
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let events = render_chunk_internal(closing, &mut s).unwrap();
    let delta_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta emitted");
    let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
    assert!(
        !payload["usage"]["input_tokens"].is_null(),
        "input_tokens must appear on the closing delta even when prompt_tokens is absent"
    );
    assert_eq!(
        payload["usage"]["input_tokens"], 0,
        "absent prompt_tokens must default input_tokens to 0"
    );
    assert_eq!(payload["usage"]["output_tokens"], 20);
}

/// Finding #2 (spec-drift): a fully-None `UsageDelta` must not produce an
/// empty `usage: {}` object in the `message_delta` payload. After fixes #1
/// and #2 compose, the wire output carries `input_tokens: 0` (from fix #1)
/// rather than an empty map, and `usage` is only inserted when at least one
/// key is present (fix #2 guard). The test pins both halves of the contract.
#[test]
fn message_delta_all_none_usage_emits_input_tokens_not_empty_object() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta::default()),
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let events = render_chunk_internal(chunk, &mut s).unwrap();
    let delta_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta emitted");
    let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
    // usage must be present (not absent) and carry at least input_tokens.
    assert!(
        !payload["usage"].is_null(),
        "usage object must be present when UsageDelta is Some"
    );
    // Must not be an empty object.
    assert!(
        payload["usage"].as_object().is_some_and(|m| !m.is_empty()),
        "usage must not be an empty map; got: {}",
        payload["usage"]
    );
    assert_eq!(
        payload["usage"]["input_tokens"], 0,
        "all-None UsageDelta: input_tokens must default to 0"
    );
}

/// Finding #4 (bug): `message_start` must use the request's resolved model
/// when upstream stream chunks carry no model string. `AnthropicStreamState`
/// carries a `req_model` field seeded from the `StreamRequestContext` at
/// `new_state`; `emit_message_start` falls back to it when `msg_model`
/// (from chunk caching) is also absent. The test builds the state through
/// the real `new_state` seam with the model set to verify the fallback.
#[test]
fn message_start_uses_req_model_when_chunk_carries_no_model() {
    use routectl_core::{ChunkChoice, ChunkDelta};
    // Arrange: state seeded with the request model, first chunk has no model.
    let mut s = new_state(&StreamRequestContext {
        model: "claude-opus-4-7".to_string(),
        input_tokens_estimate: 0,
    });
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: String::new(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("hi".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    };

    // Act
    let events = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start emitted");
    let payload: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(
        payload["message"]["model"], "claude-opus-4-7",
        "message_start.message.model must fall back to req_model when chunk carries no model"
    );
}

/// The synthesized early `message_start` must carry the request's local
/// input-token estimate on `usage.input_tokens` (Problem-C context-meter
/// fix for the pre-inversion fast path), while `output_tokens` stays 0 --
/// no output has streamed yet, and the terminal `message_delta` remains
/// the authoritative source for both. Builds state via the real
/// `new_state` seam with a known estimate, renders one chunk, and asserts
/// the rendered frame.
#[test]
fn message_start_carries_input_token_estimate_with_zero_output() {
    // Arrange: state seeded with a known estimate through the real seam.
    let mut s = new_state(&StreamRequestContext {
        model: "claude-opus-4-7".to_string(),
        input_tokens_estimate: 137,
    });

    // Act: render the first chunk, which triggers message_start.
    let events = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();

    // Assert: the message_start usage reflects the estimate; output is 0.
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start emitted");
    let payload: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(
        payload["message"]["usage"]["input_tokens"], 137,
        "message_start.usage.input_tokens must carry the state estimate"
    );
    assert_eq!(
        payload["message"]["usage"]["output_tokens"], 0,
        "message_start.usage.output_tokens must stay 0 before any output streams"
    );
}

/// Default state (no request context) keeps the pre-estimate behavior:
/// a zero `input_tokens` on `message_start`. Guards the `Default` /
/// library-consumer path that seeds no estimate.
#[test]
fn message_start_defaults_input_tokens_to_zero_without_estimate() {
    // Arrange: fresh default state carries no estimate.
    let mut s = fresh_state();

    // Act
    let events = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();

    // Assert
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start emitted");
    let payload: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(payload["message"]["usage"]["input_tokens"], 0);
    assert_eq!(payload["message"]["usage"]["output_tokens"], 0);
}
/// label (requested alias by default, or a `reported_model` override).
/// The Anthropic stream ingress caches the first chunk's model into
/// `msg_model` and surfaces it on `message_start`, so the rendered SSE
/// reflects the rewritten label rather than the upstream wire id.
#[test]
fn message_start_surfaces_rewritten_chunk_model_label() {
    use routectl_core::{ChunkChoice, ChunkDelta};
    // Arrange: a chunk carrying the router-rewritten label.
    let mut s = fresh_state();
    let chunk = ChatChunk {
        id: "msg_01".into(),
        model: "public-label".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("hi".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
        upstream_meta: None,
    };

    // Act
    let events = render_chunk_internal(chunk, &mut s).unwrap();

    // Assert
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_start"))
        .expect("message_start emitted");
    let payload: Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(payload["message"]["model"], "public-label");
}

/// Finding #5 (bug): `render_error_eos_internal` called after a normal
/// clean finish (where `state.finished` is already true) must return an
/// empty event list -- not push a second terminal `event: error` frame.
/// Before the fix, the function unconditionally appended the error event,
/// so a late transport error after `message_stop` would double-terminate
/// the stream. The fix adds an early `if state.finished { return Vec::new() }`
/// guard.
#[test]
fn render_error_eos_after_normal_finish_emits_nothing() {
    use routectl_core::{ChunkChoice, ChunkDelta, UsageDelta};
    let mut s = fresh_state();
    // Drive a normal clean finish: text + finish with inline usage.
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
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let _ = render_chunk_internal(closing, &mut s).unwrap();
    assert!(s.finished, "stream must be finished after normal close");

    // Act: late error after normal finish.
    let class =
        StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));
    let late_events = render_error_eos_internal(&mut s, &"late transport error", &class);

    // Assert: no events emitted -- double-termination must be suppressed.
    assert!(
        late_events.is_empty(),
        "render_error_eos after normal finish must emit no events, got: {late_events:?}"
    );
}

/// LOW-1 fix: closing `message_delta` must carry `output_tokens` even when
/// the upstream `UsageDelta` omits `completion_tokens`. Symmetric with the
/// existing `input_tokens` always-emit behavior -- Anthropic spec requires
/// both fields; when absent, default to 0.
#[test]
fn message_delta_missing_completion_tokens_still_emits_output_tokens_zero() {
    use routectl_core::UsageDelta;
    let mut s = fresh_state();
    let _ = render_chunk_internal(text_chunk("hi", None), &mut s).unwrap();
    // UsageDelta with prompt_tokens but NO completion_tokens.
    let closing = ChatChunk {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: None,
            total_tokens: None,
            ..Default::default()
        }),
        opaque_events: Vec::new(),
        upstream_meta: None,
    };
    let events = render_chunk_internal(closing, &mut s).unwrap();
    let delta_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("message_delta"))
        .expect("message_delta emitted");
    let payload: Value = serde_json::from_str(&delta_event.data).unwrap();
    assert!(
        !payload["usage"]["output_tokens"].is_null(),
        "output_tokens must appear on the closing delta even when completion_tokens is absent"
    );
    assert_eq!(
        payload["usage"]["output_tokens"], 0,
        "absent completion_tokens must default output_tokens to 0"
    );
    // input_tokens must still be present (existing behavior).
    assert_eq!(payload["usage"]["input_tokens"], 10);
}

/// Render one `Encrypted` reasoning detail through the streaming path
/// and return the `redacted_thinking.data` string it put on the wire.
fn streamed_redacted_thinking_data(format: Option<&str>, id: Option<&str>, blob: &str) -> String {
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};
    let chunk = reasoning_chunk(ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: id.map(Into::into),
        format: format.map(Into::into),
        index: Some(0),
        payload: json!({"encrypted_content": blob}),
    });
    let mut s = fresh_state();
    let events = render_chunk_internal(chunk, &mut s).unwrap();
    let start = events
        .iter()
        .find(|e| e.event.as_deref() == Some("content_block_start"))
        .expect("content_block_start emitted");
    let payload: Value = serde_json::from_str(&start.data).unwrap();
    payload["content_block"]["data"]
        .as_str()
        .expect("data is a string")
        .to_string()
}

/// Streaming twin of the non-streaming encode site: a Responses-family
/// blob loses its id and scheme on the Anthropic wire unless it goes out
/// self-describing, so it must be wrapped here too, blob bytes intact.
#[test]
fn streamed_encrypted_reasoning_wraps_a_foreign_scheme_blob_with_its_scheme_and_id() {
    // Arrange
    const BLOB: &str = "rsn_STREAMED-OPAQUE-PAYLOAD-2d90";

    // Act
    let data = streamed_redacted_thinking_data(
        Some(routectl_core::BEDROCK_MANTLE),
        Some("rs_stream1"),
        BLOB,
    );

    // Assert
    let (scheme, id, blob) =
        routectl_core::reasoning_envelope::unwrap(&data).expect("wire data is an envelope");
    assert_eq!(scheme, routectl_core::BEDROCK_MANTLE);
    assert_eq!(id, Some("rs_stream1"));
    assert_eq!(blob, BLOB, "inner blob bytes must be unchanged");
}

/// The carve-out holds on the streaming path as well: an
/// Anthropic-family blob reaches the wire byte-verbatim.
#[test]
fn streamed_encrypted_reasoning_emits_an_anthropic_family_blob_byte_verbatim() {
    // Arrange
    const BLOB: &str = "ErkBCkYIBRgCKkDd-ANTHROPIC-NATIVE-SIGNATURE";

    // Act
    let data = streamed_redacted_thinking_data(Some(ANTHROPIC_FORMAT), Some("rd_1"), BLOB);

    // Assert
    assert_eq!(data, BLOB, "Anthropic-sourced blob must not be wrapped");
}

/// An id-less foreign artifact still wraps on the streaming path, so its
/// scheme survives the trip through the Anthropic dialect.
#[test]
fn streamed_encrypted_reasoning_wraps_an_id_less_foreign_blob_so_its_scheme_survives() {
    // Arrange
    const BLOB: &str = "smry_STREAMED-OPAQUE-PAYLOAD-6b13";

    // Act
    let data = streamed_redacted_thinking_data(Some(routectl_core::OPENAI_APIKEY), None, BLOB);

    // Assert
    let (scheme, id, blob) =
        routectl_core::reasoning_envelope::unwrap(&data).expect("id-less envelope round-trips");
    assert_eq!(scheme, routectl_core::OPENAI_APIKEY);
    assert_eq!(id, None);
    assert_eq!(blob, BLOB);
}

/// Parity with the non-streaming path: an untagged blob has no scheme to
/// record, so it rides out exactly as it arrived. The two paths diverging
/// on this case is the failure mode the shared helper exists to prevent.
#[test]
fn streamed_encrypted_reasoning_emits_an_untagged_blob_byte_verbatim() {
    // Arrange
    const BLOB: &str = "UNTAGGED-STREAMED-PAYLOAD-4c21";

    // Act
    let data = streamed_redacted_thinking_data(None, Some("rd_9"), BLOB);

    // Assert
    assert_eq!(data, BLOB, "an untagged blob must not be wrapped");
}

/// An empty blob carries nothing to replay, so wrapping it would only
/// produce an envelope the decode side rejects.
#[test]
fn streamed_encrypted_reasoning_leaves_an_empty_blob_unwrapped() {
    // Act
    let data = streamed_redacted_thinking_data(
        Some(routectl_core::BEDROCK_MANTLE),
        Some("rs_stream2"),
        "",
    );

    // Assert
    assert_eq!(data, "", "an empty blob must not become an envelope");
}
