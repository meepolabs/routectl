//! SSE state machine tests for the OpenAI Responses provider.
//!
//! Loaded via `#[path = "sse_tests.rs"] mod tests;` in `sse.rs` to
//! keep that file under the 800-line cap.

use super::*;
use routectl_core::schema::ChunkDelta;
use serde_json::json;

fn parse(json_str: serde_json::Value) -> ResponsesStreamEvent {
    serde_json::from_value(json_str).expect("event parse")
}

fn drive(state: &mut ResponsesStreamState, ev: serde_json::Value) -> Vec<ChatChunk> {
    state
        .process_event("test", parse(ev))
        .expect("event processing")
}

#[test]
fn sse_response_created_emits_empty_role_chunk() {
    // Arrange
    let mut state = ResponsesStreamState::default();
    let ev = json!({
        "type": "response.created",
        "response": {"id": "resp_01", "model": "gpt-5-codex"}
    });

    // Act
    let chunks = drive(&mut state, ev);

    // Assert
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].id, "resp_01");
    assert_eq!(chunks[0].model, "gpt-5-codex");
    let delta = &chunks[0].choices[0].delta;
    assert!(matches!(delta.role, Some(Role::Assistant)));
    assert!(delta.content.is_none());
}

#[test]
fn sse_output_item_added_opens_text_block_state() {
    // Arrange
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({"type": "response.created", "response": {"id":"r","model":"m"}}),
    );
    let ev = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}
    });

    // Act
    let chunks = drive(&mut state, ev);

    // Assert: no chunks emitted, but state stores the block.
    assert!(chunks.is_empty());
    // Drive a text delta to verify it routes here.
    let delta_chunks = drive(
        &mut state,
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hi"
        }),
    );
    assert_eq!(delta_chunks.len(), 1);
    assert_eq!(delta_chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
}

#[test]
fn sse_output_item_added_opens_reasoning_block_state() {
    // Arrange
    let mut state = ResponsesStreamState::default();
    let ev = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {"type": "reasoning", "id": "rs_1", "summary": []}
    });

    // Act
    let chunks = drive(&mut state, ev);

    // Assert
    assert!(chunks.is_empty());
    // Drive a reasoning summary delta to verify routing.
    let delta_chunks = drive(
        &mut state,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "step"
        }),
    );
    assert_eq!(delta_chunks.len(), 1);
    assert!(matches!(
        delta_chunks[0].choices[0].delta.reasoning_details[0].kind,
        ReasoningDetailKind::Summary
    ));
}

#[test]
fn sse_output_item_added_opens_tool_use_block_state() {
    // Arrange
    let mut state = ResponsesStreamState::default();
    let ev = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_xy",
            "name": "calc",
            "arguments": ""
        }
    });

    // Act
    let chunks = drive(&mut state, ev);

    // Assert
    assert!(chunks.is_empty());
    let delta_chunks = drive(
        &mut state,
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"a\":1}"
        }),
    );
    assert_eq!(delta_chunks.len(), 1);
    let tcs = delta_chunks[0].choices[0].delta.tool_calls.as_ref().unwrap();
    assert_eq!(tcs[0]["id"], "call_xy");
    assert_eq!(tcs[0]["function"]["name"], "calc");
    assert_eq!(tcs[0]["function"]["arguments"], "{\"a\":1}");
}

#[test]
fn sse_output_text_delta_emits_text_chunk() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "m", "role": "assistant", "content": []}
        }),
    );
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        }),
    );
    assert_eq!(chunks.len(), 1);
    let delta: &ChunkDelta = &chunks[0].choices[0].delta;
    assert_eq!(delta.content.as_deref(), Some("hello"));
    assert!(delta.tool_calls.is_none());
    assert!(delta.reasoning_details.is_empty());
}

#[test]
fn sse_reasoning_summary_text_delta_emits_thinking_summary_chunk() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "r", "summary": []}
        }),
    );
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "thinking..."
        }),
    );
    assert_eq!(chunks.len(), 1);
    let delta = &chunks[0].choices[0].delta;
    assert_eq!(delta.reasoning.as_deref(), Some("thinking..."));
    assert_eq!(delta.reasoning_details.len(), 1);
    assert!(matches!(
        delta.reasoning_details[0].kind,
        ReasoningDetailKind::Summary
    ));
    assert_eq!(
        delta.reasoning_details[0].format.as_deref(),
        Some(OPENAI_RESPONSES_FORMAT)
    );
}

#[test]
fn sse_reasoning_text_delta_emits_thinking_content_chunk() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "r", "summary": []}
        }),
    );
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.reasoning_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "chain of thought"
        }),
    );
    assert_eq!(chunks.len(), 1);
    let delta = &chunks[0].choices[0].delta;
    assert_eq!(delta.reasoning.as_deref(), Some("chain of thought"));
    assert_eq!(delta.reasoning_details.len(), 1);
    assert!(matches!(
        delta.reasoning_details[0].kind,
        ReasoningDetailKind::Text
    ));
}

#[test]
fn sse_function_call_arguments_delta_emits_tool_call_partial() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc",
                "call_id": "c1",
                "name": "n",
                "arguments": ""
            }
        }),
    );
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"a\":"
        }),
    );
    assert_eq!(chunks.len(), 1);
    let tcs = chunks[0].choices[0].delta.tool_calls.as_ref().unwrap();
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0]["index"], 0);
    assert_eq!(tcs[0]["id"], "c1");
    assert_eq!(tcs[0]["function"]["arguments"], "{\"a\":");
}

#[test]
fn sse_output_item_done_flushes_encrypted_content_signature() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "r", "summary": []}
        }),
    );
    // Drive a summary delta so the detail_index is allocated.
    let _ = drive(
        &mut state,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "step"
        }),
    );
    // item.done carries the server-assigned encrypted_content.
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "r",
                "summary": [],
                "encrypted_content": "SIG_PAYLOAD"
            }
        }),
    );
    assert_eq!(chunks.len(), 1);
    let detail = &chunks[0].choices[0].delta.reasoning_details[0];
    assert!(matches!(detail.kind, ReasoningDetailKind::Encrypted));
    assert_eq!(detail.payload["encrypted_content"], "SIG_PAYLOAD");
}

#[test]
fn sse_response_completed_emits_finish_and_usage() {
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({"type": "response.created", "response": {"id": "resp_1", "model": "m"}}),
    );
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "model": "m",
                "output": [{
                    "type": "message",
                    "id": "msg",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "x"}]
                }],
                "usage": {"input_tokens": 12, "output_tokens": 7, "total_tokens": 19}
            }
        }),
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].finish_reason.as_deref(), Some("stop"));
    let u = chunks[0].usage.as_ref().unwrap();
    assert_eq!(u.prompt_tokens, Some(12));
    assert_eq!(u.completion_tokens, Some(7));
    assert_eq!(u.total_tokens, Some(19));
}

#[test]
fn sse_response_failed_emits_error_chunk() {
    let mut state = ResponsesStreamState::default();
    let result = state.process_event(
        "test",
        parse(json!({
            "type": "response.failed",
            "response": {
                "id": "r",
                "status": "failed",
                "error": {"message": "boom"}
            }
        })),
    );
    let err = result.expect_err("failed event must surface as Err");
    match err {
        Error::Upstream { body, .. } => {
            assert!(body.contains("boom"), "body: {body}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[test]
fn sse_unknown_event_logs_debug_and_continues() {
    let mut state = ResponsesStreamState::default();
    let chunks = drive(
        &mut state,
        json!({"type": "response.future_kind", "output_index": 0}),
    );
    // No chunk, no error: forward-compat path.
    assert!(chunks.is_empty());
}

#[test]
fn sse_interleaved_output_index_routes_to_correct_block_state() {
    let mut state = ResponsesStreamState::default();
    // Open a reasoning block at index 0.
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "r", "summary": []}
        }),
    );
    // Open a message block at index 1.
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "message", "id": "m", "role": "assistant", "content": []}
        }),
    );
    // Interleave: text delta on index 1, summary delta on index 0.
    let chunks_text = drive(
        &mut state,
        json!({
            "type": "response.output_text.delta",
            "output_index": 1,
            "delta": "ans"
        }),
    );
    assert_eq!(chunks_text[0].choices[0].delta.content.as_deref(), Some("ans"));

    let chunks_reason = drive(
        &mut state,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "think"
        }),
    );
    assert_eq!(
        chunks_reason[0].choices[0].delta.reasoning.as_deref(),
        Some("think")
    );
}

#[test]
fn sse_full_session_text_only_round_trip() {
    let mut state = ResponsesStreamState::default();
    let mut all_chunks: Vec<ChatChunk> = Vec::new();
    let events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({"type": "response.in_progress"}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg", "role": "assistant", "content": []}
        }),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hel"}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo"}),
        json!({"type": "response.output_text.done", "output_index": 0, "text": "hello"}),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "id": "msg", "role": "assistant",
                     "content": [{"type": "output_text", "text": "hello"}]}
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "r", "status": "completed", "model": "m",
                "output": [{"type": "message", "id": "msg", "role": "assistant",
                            "content": [{"type": "output_text", "text": "hello"}]}],
                "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
            }
        }),
    ];
    for ev in events {
        all_chunks.extend(drive(&mut state, ev));
    }
    // Expect: created (1) + 2 text deltas + final = 4 chunks.
    assert_eq!(all_chunks.len(), 4);
    let texts: Vec<String> = all_chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.clone())
        .collect();
    assert_eq!(texts.join(""), "hello");
    let final_c = all_chunks.last().unwrap();
    assert_eq!(final_c.choices[0].finish_reason.as_deref(), Some("stop"));
}

#[test]
fn sse_full_session_reasoning_then_text_round_trip() {
    let mut state = ResponsesStreamState::default();
    let mut all_chunks: Vec<ChatChunk> = Vec::new();
    let events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs", "summary": []}
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "thinking"
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs", "summary": [],
                     "encrypted_content": "SIG"}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "message", "id": "m1", "role": "assistant", "content": []}
        }),
        json!({"type": "response.output_text.delta", "output_index": 1, "delta": "answer"}),
        json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {"type": "message", "id": "m1", "role": "assistant",
                     "content": [{"type": "output_text", "text": "answer"}]}
        }),
        json!({
            "type": "response.completed",
            "response": {"id": "r", "status": "completed", "model": "m", "output": []}
        }),
    ];
    for ev in events {
        all_chunks.extend(drive(&mut state, ev));
    }
    // created + reasoning summary + reasoning signature + text + final = 5
    assert_eq!(all_chunks.len(), 5);
    // Signature surfaces as an Encrypted detail.
    let sig_chunk = all_chunks
        .iter()
        .find(|c| {
            c.choices[0]
                .delta
                .reasoning_details
                .iter()
                .any(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        })
        .expect("signature chunk emitted");
    let sig_detail = &sig_chunk.choices[0]
        .delta
        .reasoning_details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .unwrap();
    assert_eq!(sig_detail.payload["encrypted_content"], "SIG");
}

#[test]
fn sse_full_session_reasoning_then_tool_call_round_trip() {
    let mut state = ResponsesStreamState::default();
    let mut all_chunks: Vec<ChatChunk> = Vec::new();
    let events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs", "summary": []}
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "consider"
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs", "summary": []}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc",
                "call_id": "call_42",
                "name": "calc",
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"x\":"
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "1}"
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "output_index": 1
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc",
                "call_id": "call_42",
                "name": "calc",
                "arguments": "{\"x\":1}"
            }
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "r", "status": "completed", "model": "m",
                "output": [{
                    "type": "function_call",
                    "id": "fc",
                    "call_id": "call_42",
                    "name": "calc",
                    "arguments": "{\"x\":1}"
                }]
            }
        }),
    ];
    for ev in events {
        all_chunks.extend(drive(&mut state, ev));
    }
    // Expect tool_calls partials AND finish_reason = tool_calls.
    let final_c = all_chunks.last().unwrap();
    assert_eq!(
        final_c.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    let tool_chunks: Vec<&ChatChunk> = all_chunks
        .iter()
        .filter(|c| c.choices[0].delta.tool_calls.is_some())
        .collect();
    assert_eq!(tool_chunks.len(), 2);
    let concat: String = tool_chunks
        .iter()
        .map(|c| {
            c.choices[0].delta.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(concat, "{\"x\":1}");
}

#[test]
fn sse_text_delta_for_unknown_block_is_dropped() {
    // Arrange: drive a created event, then send a text delta for an
    // output_index that was never opened via output_item.added. The
    // delta must drop silently (DEBUG-logged) rather than emit a
    // text chunk that the canonical stream would attribute to a
    // phantom block.
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
    );

    // Act: text delta at output_index=7 (not in self.blocks).
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.output_text.delta",
            "output_index": 7,
            "delta": "stray"
        }),
    );

    // Assert: dropped.
    assert!(chunks.is_empty());
}

#[test]
fn sse_text_delta_for_reasoning_block_is_dropped() {
    // Arrange: open a Reasoning block at index 0, then send a text
    // delta naming index 0. The text delta must drop because the
    // block at that index is Reasoning, not Text. This guards
    // against an upstream that mis-routes a text delta and would
    // otherwise corrupt the assistant output.
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": []}
        }),
    );

    // Act
    let chunks = drive(
        &mut state,
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "stray"
        }),
    );

    // Assert: dropped.
    assert!(chunks.is_empty());
}

#[test]
fn sse_interleaved_two_text_blocks_route_to_correct_block() {
    // Arrange: open TWO Text blocks at output_index 0 and 1, then
    // interleave deltas on both. Each delta must produce a chunk
    // because both blocks are Text; the test guards against a future
    // regression in the gating logic that would lose track of
    // multi-Text-block sessions (some models emit `message` items
    // followed by reasoning followed by another `message`).
    let mut state = ResponsesStreamState::default();
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "m0", "role": "assistant", "content": []}
        }),
    );
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "reasoning", "id": "rs", "summary": []}
        }),
    );
    let _ = drive(
        &mut state,
        json!({
            "type": "response.output_item.added",
            "output_index": 2,
            "item": {"type": "message", "id": "m1", "role": "assistant", "content": []}
        }),
    );

    // Act
    let chunks_0 = drive(
        &mut state,
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "first"}),
    );
    let chunks_2 = drive(
        &mut state,
        json!({"type": "response.output_text.delta", "output_index": 2, "delta": "second"}),
    );

    // Assert: each Text delta routed to its own block.
    assert_eq!(chunks_0.len(), 1);
    assert_eq!(chunks_0[0].choices[0].delta.content.as_deref(), Some("first"));
    assert_eq!(chunks_2.len(), 1);
    assert_eq!(chunks_2[0].choices[0].delta.content.as_deref(), Some("second"));
}
