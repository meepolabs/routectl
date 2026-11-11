//! SSE lifecycle renderer tests for the openai-responses ingress.
//!
//! Loaded via `#[path = "stream_tests.rs"] mod tests;` in `stream.rs`.
//! These pin the canonical `ChatChunk` -> Responses SSE event lifecycle
//! (the inverse of the egress SSE reader). Event `type` strings and
//! field shapes are asserted exactly against the egress wire fixtures
//! in `routectl-providers/.../sse_tests.rs`.

use serde_json::{Value, json};

use routectl_core::{
    BEDROCK_MANTLE, CODEX_OAUTH, ChatChunk, ChunkChoice, ChunkDelta, OPENAI_APIKEY,
    OPENAI_RESPONSES_V1, ReasoningDetail, ReasoningDetailKind, ResponsesEnvelope, UpstreamMeta,
};

use super::*;
use crate::ingress::StreamErrorClass;
use crate::ingress::openai_responses::ResponsesStreamState;

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn fresh() -> ResponsesStreamState {
    ResponsesStreamState::default()
}

fn text_chunk(text: &str) -> ChatChunk {
    ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
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

fn finish_chunk(reason: &str, usage: Option<UsageDelta>) -> ChatChunk {
    ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some(reason.into()),
            matched_stop_sequence: None,
        }],
        usage,
        opaque_events: Vec::new(),
        upstream_meta: None,
    }
}

fn reasoning_chunk(kind: ReasoningDetailKind, id: &str, payload: Value) -> ChatChunk {
    tagged_reasoning_chunk(kind, id, payload, OPENAI_RESPONSES_V1)
}

fn tagged_reasoning_chunk(
    kind: ReasoningDetailKind,
    id: &str,
    payload: Value,
    format: &str,
) -> ChatChunk {
    ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![ReasoningDetail {
                    kind,
                    id: Some(id.into()),
                    format: Some(format.into()),
                    index: Some(0),
                    payload,
                }],
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

fn foreign_reasoning_chunk(kind: ReasoningDetailKind, id: &str, payload: Value) -> ChatChunk {
    tagged_reasoning_chunk(kind, id, payload, "anthropic-claude-v1")
}

fn tool_chunk(index: u64, id: &str, name: &str, args: &str) -> ChatChunk {
    ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args},
                })]),
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

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn names(events: &[SseEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| e.event.clone())
        .collect::<Vec<_>>()
}

fn data_of(events: &[SseEvent], event_name: &str) -> Value {
    let ev = events
        .iter()
        .find(|e| e.event.as_deref() == Some(event_name))
        .unwrap_or_else(|| panic!("event {event_name} not emitted; got {:?}", names(events)));
    serde_json::from_str(&ev.data).expect("event data is JSON")
}

fn all_data(events: &[SseEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|e| serde_json::from_str::<Value>(&e.data).expect("json"))
        .collect()
}

fn render(state: &mut ResponsesStreamState, chunk: ChatChunk) -> Vec<SseEvent> {
    render_chunk_internal(chunk, state).expect("render_chunk")
}

// ---------------------------------------------------------------------------
// response.created
// ---------------------------------------------------------------------------

#[test]
fn first_chunk_emits_response_created_once_capturing_id_and_model() {
    // Arrange
    let mut state = fresh();

    // Act
    let events = render(&mut state, text_chunk("hi"));

    // Assert: created + in_progress emitted before any text events.
    let ns = names(&events);
    assert_eq!(ns[0], "response.created");
    assert_eq!(ns[1], "response.in_progress");
    let created = data_of(&events, "response.created");
    assert_eq!(created["type"], "response.created");
    assert_eq!(created["sequence_number"], 0);
    assert_eq!(created["response"]["id"], "resp_01");
    assert_eq!(created["response"]["model"], "gpt-5-codex");
    assert_eq!(created["response"]["object"], "response");
    assert_eq!(created["response"]["status"], "in_progress");
    assert_eq!(created["response"]["output"], json!([]));
}

#[test]
fn response_created_emitted_only_once_across_chunks() {
    // Arrange
    let mut state = fresh();

    // Act
    let first = render(&mut state, text_chunk("a"));
    let second = render(&mut state, text_chunk("b"));

    // Assert: only the first chunk carries response.created.
    assert!(names(&first).contains(&"response.created".to_string()));
    assert!(!names(&second).contains(&"response.created".to_string()));
}

// ---------------------------------------------------------------------------
// Pure-text lifecycle
// ---------------------------------------------------------------------------

#[test]
fn pure_text_stream_emits_full_bracketed_lifecycle() {
    // Arrange
    let mut state = fresh();
    let mut events = render(&mut state, text_chunk("hello"));

    // Act
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert: the canonical bracket order.
    assert_eq!(
        names(&events),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
}

#[test]
fn output_text_delta_carries_text_and_indices() {
    // Arrange
    let mut state = fresh();

    // Act
    let events = render(&mut state, text_chunk("hello"));

    // Assert
    let delta = data_of(&events, "response.output_text.delta");
    assert_eq!(delta["type"], "response.output_text.delta");
    assert_eq!(delta["output_index"], 0);
    assert_eq!(delta["content_index"], 0);
    assert_eq!(delta["delta"], "hello");
    let added = data_of(&events, "response.output_item.added");
    assert_eq!(added["item"]["type"], "message");
    assert_eq!(added["item"]["role"], "assistant");
    let part = data_of(&events, "response.content_part.added");
    assert_eq!(part["part"]["type"], "output_text");
    assert_eq!(part["part"]["annotations"], json!([]));
}

#[test]
fn multiple_text_deltas_route_to_one_message_item() {
    // Arrange
    let mut state = fresh();

    // Act: three text deltas.
    let mut events = render(&mut state, text_chunk("hel"));
    events.extend(render(&mut state, text_chunk("lo ")));
    events.extend(render(&mut state, text_chunk("world")));

    // Assert: exactly one output_item.added, three text deltas, same index.
    let added: Vec<&SseEvent> = events
        .iter()
        .filter(|e| e.event.as_deref() == Some("response.output_item.added"))
        .collect();
    assert_eq!(added.len(), 1);
    let deltas: Vec<Value> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.output_text.delta")
        .collect();
    assert_eq!(deltas.len(), 3);
    let concat: String = deltas
        .iter()
        .map(|d| d["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(concat, "hello world");
    assert!(deltas.iter().all(|d| d["output_index"] == 0));
}

#[test]
fn output_text_done_carries_full_accumulated_text() {
    // Arrange
    let mut state = fresh();
    let mut events = render(&mut state, text_chunk("foo"));
    events.extend(render(&mut state, text_chunk("bar")));

    // Act
    events.extend(render_eos_internal(&mut state));

    // Assert
    let done = data_of(&events, "response.output_text.done");
    assert_eq!(done["text"], "foobar");
    let part_done = data_of(&events, "response.content_part.done");
    assert_eq!(part_done["part"]["text"], "foobar");
    let item_done = data_of(&events, "response.output_item.done");
    assert_eq!(item_done["item"]["content"][0]["text"], "foobar");
    assert_eq!(item_done["item"]["status"], "completed");
}

// ---------------------------------------------------------------------------
// Sequence numbers
// ---------------------------------------------------------------------------

#[test]
fn sequence_number_is_monotonic_across_whole_stream() {
    // Arrange
    let mut state = fresh();
    let mut events = render(&mut state, text_chunk("a"));
    events.extend(render(&mut state, text_chunk("b")));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Act
    let seqs: Vec<u64> = all_data(&events)
        .iter()
        .map(|d| d["sequence_number"].as_u64().unwrap())
        .collect();

    // Assert: 0,1,2,... with no gaps or repeats.
    let expected: Vec<u64> = (0..seqs.len() as u64).collect();
    assert_eq!(seqs, expected);
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

#[test]
fn tool_call_stream_emits_function_call_lifecycle() {
    // Arrange
    let mut state = fresh();
    let mut events = render(&mut state, tool_chunk(0, "call_42", "calc", "{\"x\":"));
    events.extend(render(&mut state, tool_chunk(0, "", "", "1}")));

    // Act
    events.extend(render(&mut state, finish_chunk("tool_calls", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert: the function-call bracket.
    let added = data_of(&events, "response.output_item.added");
    assert_eq!(added["item"]["type"], "function_call");
    assert_eq!(added["item"]["call_id"], "call_42");
    assert_eq!(added["item"]["name"], "calc");
    let arg_delta = data_of(&events, "response.function_call_arguments.delta");
    assert_eq!(arg_delta["delta"], "{\"x\":1}");
    let arg_done = data_of(&events, "response.function_call_arguments.done");
    assert_eq!(arg_done["arguments"], "{\"x\":1}");
    let item_done = data_of(&events, "response.output_item.done");
    assert_eq!(item_done["item"]["arguments"], "{\"x\":1}");
}

#[test]
fn multiple_tool_calls_get_distinct_output_index() {
    // Arrange
    let mut state = fresh();
    let mut events = render(&mut state, tool_chunk(0, "call_a", "first", "{}"));
    events.extend(render(&mut state, tool_chunk(1, "call_b", "second", "{}")));

    // Act
    events.extend(render(&mut state, finish_chunk("tool_calls", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert: two function_call items at distinct output_index.
    let added: Vec<Value> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.output_item.added")
        .collect();
    assert_eq!(added.len(), 2);
    let indices: Vec<u64> = added
        .iter()
        .map(|d| d["output_index"].as_u64().unwrap())
        .collect();
    assert_eq!(indices, vec![0, 1]);
    assert_eq!(added[0]["item"]["call_id"], "call_a");
    assert_eq!(added[1]["item"]["call_id"], "call_b");
}

#[test]
fn tool_call_index_above_cap_is_dropped() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("x")); // open created
    let huge = (MAX_TOOL_CALL_INDEX + 1) as u64;

    // Act
    render(&mut state, tool_chunk(huge, "call_z", "z", "{}"));
    let events = render_eos_internal(&mut state);

    // Assert: no function_call item emitted for the over-cap index.
    assert!(
        !all_data(&events)
            .iter()
            .any(|d| d["item"]["type"] == "function_call")
    );
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

#[test]
fn reasoning_summary_stream_emits_summary_delta_with_format_and_id() {
    // Arrange
    let mut state = fresh();

    // Act
    let events = render(
        &mut state,
        reasoning_chunk(
            ReasoningDetailKind::Summary,
            "rs_1",
            json!({"text": "step"}),
        ),
    );

    // Assert
    let added = data_of(&events, "response.output_item.added");
    assert_eq!(added["item"]["type"], "reasoning");
    assert_eq!(added["item"]["id"], "rs_1");
    let delta = data_of(&events, "response.reasoning_summary_text.delta");
    assert_eq!(delta["delta"], "step");
    assert_eq!(delta["summary_index"], 0);
}

#[test]
fn reasoning_text_stream_emits_reasoning_text_delta() {
    // Arrange
    let mut state = fresh();

    // Act
    let events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Text, "rs_1", json!({"text": "chain"})),
    );

    // Assert
    let delta = data_of(&events, "response.reasoning_text.delta");
    assert_eq!(delta["delta"], "chain");
    assert_eq!(delta["content_index"], 0);
}

#[test]
fn reasoning_encrypted_rides_to_item_done_not_a_delta() {
    // Arrange: a summary delta opens the reasoning item, then an
    // encrypted detail (signature) on the same id.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "s"})),
    );
    events.extend(render(
        &mut state,
        reasoning_chunk(
            ReasoningDetailKind::Encrypted,
            "rs_1",
            json!({"encrypted_content": "SIG"}),
        ),
    ));

    // Act: close the item.
    events.extend(render_eos_internal(&mut state));

    // Assert: no encrypted delta event; the signature is on item.done.
    assert!(
        !names(&events)
            .iter()
            .any(|n| n.contains("reasoning") && n.contains("delta") && n.contains("encrypted"))
    );
    let item_done = data_of(&events, "response.output_item.done");
    assert_eq!(item_done["item"]["encrypted_content"], "SIG");
    assert_eq!(item_done["item"]["summary"][0]["text"], "s");
}

#[test]
fn reasoning_unrecognized_kind_rides_to_item_done_contributing_nothing() {
    // An unrecognized kind gets the same treatment as Encrypted: it
    // opens/keeps the reasoning item alive (so the output_index does
    // not gap) but emits no delta, and unlike Encrypted it contributes
    // nothing to the completed item body either -- no slot exists for
    // it there. Paired with the Encrypted test above (same code path,
    // recognized kind that DOES land on item.done) as the positive
    // control.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "s"})),
    );
    events.extend(render(
        &mut state,
        reasoning_chunk(
            ReasoningDetailKind::Other("future.kind".to_string()),
            "rs_1",
            json!({"text": "a future shape"}),
        ),
    ));

    // Act: close the item.
    events.extend(render_eos_internal(&mut state));

    // Assert: no delta event for the unrecognized detail.
    assert!(
        !names(&events)
            .iter()
            .any(|n| n.contains("reasoning") && n.contains("delta") && n.contains("future")),
        "an unrecognized kind must never emit a delta event"
    );
    let item_done = data_of(&events, "response.output_item.done");
    assert_eq!(
        item_done["item"]["summary"][0]["text"], "s",
        "the recognized detail must still land on the completed body"
    );
    let summary = item_done["item"]["summary"].as_array().unwrap();
    assert_eq!(
        summary.len(),
        1,
        "the unrecognized detail must not contribute a summary entry: {summary:?}"
    );
    assert!(
        item_done["item"].get("content").is_none()
            || item_done["item"]["content"].as_array().unwrap().is_empty(),
        "the unrecognized detail must not contribute a content entry: {:?}",
        item_done["item"]
    );
}

#[test]
fn reasoning_then_text_supersedes_and_closes_reasoning_item_first() {
    // Arrange
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(
            ReasoningDetailKind::Summary,
            "rs_1",
            json!({"text": "think"}),
        ),
    );

    // Act: a text delta supersedes the open reasoning item.
    events.extend(render(&mut state, text_chunk("answer")));

    // Assert: reasoning item.done lands BEFORE the message item.added.
    let ns = names(&events);
    let reasoning_done = ns
        .iter()
        .position(|n| n == "response.output_item.done")
        .unwrap();
    let message_added = ns
        .iter()
        .rposition(|n| n == "response.output_item.added")
        .unwrap();
    assert!(
        reasoning_done < message_added,
        "reasoning item must close before message opens: {ns:?}"
    );
}

// ---------------------------------------------------------------------------
// finish_reason -> completed status
// ---------------------------------------------------------------------------

#[test]
fn finish_reason_stop_maps_to_completed_status() {
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render(&mut state, finish_chunk("stop", None));
    let events = render_eos_internal(&mut state);
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
}

#[test]
fn finish_reason_tool_calls_maps_to_completed_status() {
    let mut state = fresh();
    let _ = render(&mut state, tool_chunk(0, "c", "n", "{}"));
    let _ = render(&mut state, finish_chunk("tool_calls", None));
    let events = render_eos_internal(&mut state);
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
}

#[test]
fn finish_reason_length_maps_to_incomplete_status() {
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render(&mut state, finish_chunk("length", None));
    let events = render_eos_internal(&mut state);
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "incomplete");
    assert_eq!(
        completed["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
}

#[test]
fn finish_reason_content_filter_maps_to_incomplete_with_reason() {
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render(&mut state, finish_chunk("content_filter", None));
    let events = render_eos_internal(&mut state);
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "incomplete");
    assert_eq!(
        completed["response"]["incomplete_details"]["reason"],
        "content_filter"
    );
}

#[test]
fn second_finish_reason_is_dropped_first_wins() {
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render(&mut state, finish_chunk("stop", None));
    let _ = render(&mut state, finish_chunk("length", None));
    let events = render_eos_internal(&mut state);
    let completed = data_of(&events, "response.completed");
    // First-wins: "stop" -> completed (not "length" -> incomplete).
    assert_eq!(completed["response"]["status"], "completed");
}

// ---------------------------------------------------------------------------
// usage
// ---------------------------------------------------------------------------

#[test]
fn usage_on_completed_renders_cached_and_reasoning_subdetails() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let usage = UsageDelta {
        prompt_tokens: Some(12),
        completion_tokens: Some(7),
        total_tokens: Some(19),
        reasoning_tokens: Some(3),
        cache_read_input_tokens: Some(4),
        ..Default::default()
    };

    // Act: finish with inline usage.
    let _ = render(&mut state, finish_chunk("stop", Some(usage)));
    let events = render_eos_internal(&mut state);

    // Assert: usage object matches the non-stream render_usage shape.
    let completed = data_of(&events, "response.completed");
    let u = &completed["response"]["usage"];
    assert_eq!(u["input_tokens"], 12);
    assert_eq!(u["output_tokens"], 7);
    assert_eq!(u["total_tokens"], 19);
    assert_eq!(u["input_tokens_details"]["cached_tokens"], 4);
    assert_eq!(u["output_tokens_details"]["reasoning_tokens"], 3);
}

#[test]
fn trailing_usage_only_chunk_is_captured_for_completed_body() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render(&mut state, finish_chunk("stop", None));
    // Separate trailing usage chunk (no choices).
    let usage_chunk = ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![],
        usage: Some(UsageDelta {
            prompt_tokens: Some(5),
            completion_tokens: Some(2),
            total_tokens: Some(7),
            ..Default::default()
        }),
        opaque_events: Vec::new(),
        upstream_meta: None,
    };

    // Act
    let _ = render(&mut state, usage_chunk);
    let events = render_eos_internal(&mut state);

    // Assert
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 5);
}

// ---------------------------------------------------------------------------
// Completed body parity with non-stream render
// ---------------------------------------------------------------------------

#[test]
fn completed_body_output_matches_non_stream_render_for_text() {
    use crate::ingress::openai_responses::render::render_responses_response;
    use routectl_core::{ChatResponse, Message, MessageContent, Role, schema::Choice};

    // Arrange: stream "hello" then finish.
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hello"));
    let _ = render(&mut state, finish_chunk("stop", None));
    let events = render_eos_internal(&mut state);
    let streamed_output = data_of(&events, "response.completed")["response"]["output"].clone();

    // Act: render the equivalent non-stream response.
    let resp = ChatResponse {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extras: serde_json::Map::new(),
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let non_stream_output = render_responses_response(resp).unwrap()["output"].clone();

    // Assert: byte-for-byte identical output[].
    assert_eq!(streamed_output, non_stream_output);
}

// ---------------------------------------------------------------------------
// Empty stream
// ---------------------------------------------------------------------------

#[test]
fn empty_stream_still_emits_created_and_completed() {
    // Arrange: no chunks at all.
    let mut state = fresh();

    // Act
    let events = render_eos_internal(&mut state);

    // Assert: a protocol-valid minimal envelope.
    let ns = names(&events);
    assert!(ns.contains(&"response.created".to_string()));
    assert!(ns.contains(&"response.completed".to_string()));
    let completed = data_of(&events, "response.completed");
    assert_eq!(completed["response"]["output"], json!([]));
}

#[test]
fn render_eos_is_idempotent() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let first = render_eos_internal(&mut state);

    // Act: second call must produce nothing.
    let second = render_eos_internal(&mut state);

    // Assert
    assert!(!first.is_empty());
    assert!(second.is_empty());
}

// ---------------------------------------------------------------------------
// render_error_eos
// ---------------------------------------------------------------------------

fn err_class() -> StreamErrorClass {
    StreamErrorClass::from_error(&routectl_core::Error::upstream("p", 503, "x"))
}

#[test]
fn render_error_eos_emits_terminal_response_failed() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("partial"));

    // Act
    let events = render_error_eos_internal(&mut state, &"upstream exploded", &err_class());

    // Assert: terminal event is response.failed (NOT response.completed).
    let ns = names(&events);
    assert!(ns.contains(&"response.failed".to_string()));
    assert!(!ns.contains(&"response.completed".to_string()));
    let failed = data_of(&events, "response.failed");
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["status"], "failed");
    assert_eq!(failed["response"]["error"]["message"], "upstream exploded");
}

#[test]
fn render_error_eos_sanitizes_control_chars_in_message() {
    // Arrange
    let mut state = fresh();

    // Act: a message with CRLF that would break SSE framing.
    let events = render_error_eos_internal(&mut state, &"line1\r\nline2", &err_class());

    // Assert: sanitized (no raw CRLF in the emitted message).
    let failed = data_of(&events, "response.failed");
    let msg = failed["response"]["error"]["message"].as_str().unwrap();
    assert!(!msg.contains('\r'));
    assert!(!msg.contains('\n'));
}

#[test]
fn render_error_eos_after_completion_emits_nothing() {
    // Arrange: stream completes cleanly first.
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render_eos_internal(&mut state);

    // Act: a late error must not push a second terminal event.
    let events = render_error_eos_internal(&mut state, &"late error", &err_class());

    // Assert
    assert!(events.is_empty());
}

#[test]
fn render_error_eos_opens_envelope_when_no_chunk_seen() {
    // Arrange: error before any chunk.
    let mut state = fresh();

    // Act
    let events = render_error_eos_internal(&mut state, &"early failure", &err_class());

    // Assert: created opens the envelope, then failed terminates it.
    let ns = names(&events);
    assert_eq!(ns.first().map(String::as_str), Some("response.created"));
    assert!(ns.contains(&"response.failed".to_string()));
}

// ---------------------------------------------------------------------------
// Post-completion straggler guard
// ---------------------------------------------------------------------------

#[test]
fn chunk_after_completion_is_dropped() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let _ = render_eos_internal(&mut state);

    // Act: a straggler chunk after the terminal event.
    let events = render(&mut state, text_chunk("late"));

    // Assert: dropped.
    assert!(events.is_empty());
}

// ---------------------------------------------------------------------------
// Review-hardening: correctness fixes
// ---------------------------------------------------------------------------

#[test]
fn tool_call_completed_body_includes_function_call_items() {
    // Guard for the HIGH ordering bug: flush_tool_calls previously
    // drained tool_buffers BEFORE completed_output read them, producing
    // an empty output[] in response.completed for every tool-call turn.
    let mut state = fresh();
    let mut events = render(&mut state, tool_chunk(0, "call_1", "run", "{\"n\":1}"));
    events.extend(render(&mut state, finish_chunk("tool_calls", None)));
    events.extend(render_eos_internal(&mut state));

    let completed = data_of(&events, "response.completed");
    let output = &completed["response"]["output"];
    // output[] must contain the function_call item (not be empty).
    assert!(
        output.as_array().is_some_and(|a| !a.is_empty()),
        "response.completed output[] must not be empty for a tool-call turn; got: {output}"
    );
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[0]["call_id"], "call_1");
    assert_eq!(output[0]["name"], "run");
}

#[test]
fn render_error_eos_closes_open_item_before_emitting_failed() {
    // Guard for MEDIUM bracket-balance bug: render_error_eos_internal
    // previously omitted close_open_item, leaving a dangling open text
    // item (missing output_text.done + content_part.done + output_item.done).
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("partial"));

    let events = render_error_eos_internal(&mut state, &"boom", &err_class());

    // The text item must be closed (output_item.done) before response.failed.
    let ns = names(&events);
    let item_done = ns
        .iter()
        .position(|n| n == "response.output_item.done")
        .expect("output_item.done must be emitted on error path; got: {ns:?}");
    let failed = ns
        .iter()
        .position(|n| n == "response.failed")
        .expect("response.failed must be emitted");
    assert!(
        item_done < failed,
        "output_item.done must precede response.failed; order: {ns:?}"
    );
}

#[test]
fn two_reasoning_groups_with_different_ids_get_distinct_output_index() {
    // Guard for MEDIUM reasoning id-match bug: ensure_reasoning_item
    // previously reused the open Reasoning slot regardless of detail_id,
    // routing the second group onto the first group's output_index.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_A", json!({"text": "a"})),
    );
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_B", json!({"text": "b"})),
    ));
    events.extend(render_eos_internal(&mut state));

    let added: Vec<Value> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.output_item.added")
        .collect();
    // Both groups must produce their own output_item.added event.
    assert_eq!(
        added.len(),
        2,
        "each reasoning group must open its own item; got: {added:?}"
    );
    let indices: Vec<u64> = added
        .iter()
        .map(|d| d["output_index"].as_u64().unwrap())
        .collect();
    assert!(
        indices[0] != indices[1],
        "two reasoning groups must get distinct output_index; got: {indices:?}"
    );
    assert_eq!(added[0]["item"]["id"], "rs_A");
    assert_eq!(added[1]["item"]["id"], "rs_B");
}

#[test]
fn tool_call_index_at_cap_boundary_is_dropped() {
    // Guard for LOW off-by-one: cap was `> MAX_TOOL_CALL_INDEX` (allows 4096);
    // should be `>= MAX_TOOL_CALL_INDEX` (drops 4096 and above).
    let mut state = fresh();
    let at_cap = MAX_TOOL_CALL_INDEX as u64;
    render(&mut state, tool_chunk(at_cap, "call_z", "z", "{}"));
    let events = render_eos_internal(&mut state);

    assert!(
        !all_data(&events)
            .iter()
            .any(|d| d["item"]["type"] == "function_call"),
        "index == MAX_TOOL_CALL_INDEX must be dropped; events: {:?}",
        names(&events)
    );
}

#[test]
fn missing_upstream_id_is_minted_and_stable_across_events() {
    // Arrange: a chunk with empty id/model.
    let mut state = fresh();
    let chunk = ChatChunk {
        id: String::new(),
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
    let mut events = render(&mut state, chunk);
    events.extend(render_eos_internal(&mut state));

    // Assert: created and completed echo the same minted id.
    let created_id = data_of(&events, "response.created")["response"]["id"].clone();
    let completed_id = data_of(&events, "response.completed")["response"]["id"].clone();
    assert!(created_id.as_str().unwrap().starts_with("resp_"));
    assert_eq!(created_id, completed_id);
}

// ---------------------------------------------------------------------------
// Terminal failed-event (finish_reason == "error")
// ---------------------------------------------------------------------------

#[test]
fn finish_reason_error_emits_response_failed_not_completed() {
    // Arrange: stream partial text, then a terminal error with usage.
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("partial answer"));
    let usage = UsageDelta {
        prompt_tokens: Some(10),
        completion_tokens: Some(4),
        total_tokens: Some(14),
        ..Default::default()
    };

    // Act
    let _ = render(&mut state, finish_chunk("error", Some(usage)));
    let events = render_eos_internal(&mut state);

    // Assert: the terminal event NAME is response.failed, never completed.
    let ns = names(&events);
    assert!(
        ns.contains(&"response.failed".to_string()),
        "error finish must emit response.failed; got {ns:?}"
    );
    assert!(
        !ns.contains(&"response.completed".to_string()),
        "error finish must NOT emit response.completed; got {ns:?}"
    );
    let failed = data_of(&events, "response.failed");
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["status"], "failed");
}

#[test]
fn failed_body_preserves_accumulated_usage_and_partial_output() {
    // Arrange
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("half a reply"));
    let usage = UsageDelta {
        prompt_tokens: Some(21),
        completion_tokens: Some(6),
        total_tokens: Some(27),
        ..Default::default()
    };

    // Act
    let _ = render(&mut state, finish_chunk("error", Some(usage)));
    let events = render_eos_internal(&mut state);

    // Assert: usage survives into the failed body.
    let failed = data_of(&events, "response.failed");
    assert_eq!(failed["response"]["usage"]["input_tokens"], 21);
    assert_eq!(failed["response"]["usage"]["output_tokens"], 6);
    assert_eq!(failed["response"]["usage"]["total_tokens"], 27);
    // Partial streamed output survives into the failed body.
    let output = &failed["response"]["output"];
    assert!(
        output.as_array().is_some_and(|a| !a.is_empty()),
        "partial output must be preserved in the failed body; got {output}"
    );
    assert_eq!(output[0]["content"][0]["text"], "half a reply");
}

// ---------------------------------------------------------------------------
// Foreign-format reasoning excluded from the streamed lifecycle
// ---------------------------------------------------------------------------

#[test]
fn foreign_format_reasoning_not_streamed_and_leaves_no_output_index_gap() {
    // Arrange: a foreign-format reasoning detail arrives first, then a
    // Responses-format message.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        foreign_reasoning_chunk(
            ReasoningDetailKind::Text,
            "rs_foreign",
            json!({"text": "hidden chain"}),
        ),
    );

    // Act
    events.extend(render(&mut state, text_chunk("answer")));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert: no reasoning lifecycle event emitted for the foreign detail.
    let ns = names(&events);
    assert!(
        !ns.iter().any(|n| n.contains("reasoning")),
        "foreign-format reasoning must not stream any reasoning event; got {ns:?}"
    );
    // No leaked foreign reasoning delta anywhere.
    assert!(
        !all_data(&events)
            .iter()
            .any(|d| d["delta"] == "hidden chain"),
        "foreign reasoning delta must not leak into the stream"
    );
    // The message item takes output_index 0 (no gap from a phantom
    // reasoning item that never landed in the completed body).
    let added_indices: Vec<u64> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.output_item.added")
        .map(|d| d["output_index"].as_u64().unwrap())
        .collect();
    assert_eq!(
        added_indices,
        vec![0],
        "output_index must be dense from 0 with no gap; got {added_indices:?}"
    );
    // Completed body carries no foreign reasoning item (parity with stream).
    let completed = data_of(&events, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    assert!(
        output.iter().all(|o| o["type"] != "reasoning"),
        "foreign reasoning must be absent from the completed body; got {output:?}"
    );
}

// ---------------------------------------------------------------------------
// Every Responses-family tag streams and reaches the completed body
// ---------------------------------------------------------------------------

fn assert_lane_tag_streams(tag: &str) {
    // Arrange
    let mut state = fresh();

    // Act
    let mut events = render(
        &mut state,
        tagged_reasoning_chunk(
            ReasoningDetailKind::Summary,
            "rs_1",
            json!({"text": "step"}),
            tag,
        ),
    );
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let delta = data_of(&events, "response.reasoning_summary_text.delta");
    assert_eq!(delta["delta"], "step");
    let completed = data_of(&events, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    assert!(
        output.iter().any(|o| o["type"] == "reasoning"),
        "tag {tag} must reach the completed body; got {output:?}"
    );
}

#[test]
fn codex_oauth_tagged_reasoning_streams() {
    assert_lane_tag_streams(CODEX_OAUTH);
}

#[test]
fn openai_apikey_tagged_reasoning_streams() {
    assert_lane_tag_streams(OPENAI_APIKEY);
}

#[test]
fn bedrock_mantle_tagged_reasoning_streams() {
    assert_lane_tag_streams(BEDROCK_MANTLE);
}

#[test]
fn compatibility_tagged_reasoning_still_streams() {
    assert_lane_tag_streams(OPENAI_RESPONSES_V1);
}

// ---------------------------------------------------------------------------
// Per-item reasoning summary/content indices
// ---------------------------------------------------------------------------

#[test]
fn multiple_reasoning_details_get_incrementing_per_item_indices() {
    // Arrange: one reasoning item carrying two summaries and two texts.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "s0"})),
    );
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "s1"})),
    ));
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Text, "rs_1", json!({"text": "t0"})),
    ));
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Text, "rs_1", json!({"text": "t1"})),
    ));

    // Act
    events.extend(render_eos_internal(&mut state));

    // Assert: summary_index increments 0,1; content_index increments 0,1.
    let summary_indices: Vec<u64> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.reasoning_summary_text.delta")
        .map(|d| d["summary_index"].as_u64().unwrap())
        .collect();
    assert_eq!(summary_indices, vec![0, 1]);
    let content_indices: Vec<u64> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.reasoning_text.delta")
        .map(|d| d["content_index"].as_u64().unwrap())
        .collect();
    assert_eq!(content_indices, vec![0, 1]);

    // Parity: the completed body carries the same count of parts in order.
    let completed = data_of(&events, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    let reasoning = output
        .iter()
        .find(|o| o["type"] == "reasoning")
        .expect("reasoning item present in completed body");
    assert_eq!(reasoning["summary"].as_array().unwrap().len(), 2);
    assert_eq!(reasoning["content"].as_array().unwrap().len(), 2);
}

#[test]
fn reasoning_indices_reset_per_new_item() {
    // Arrange: two details on rs_A, then a new item rs_B.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_A", json!({"text": "a0"})),
    );
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_A", json!({"text": "a1"})),
    ));
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_B", json!({"text": "b0"})),
    ));

    // Act
    events.extend(render_eos_internal(&mut state));

    // Assert: rs_B's counter starts fresh at 0.
    let summary_indices: Vec<u64> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.reasoning_summary_text.delta")
        .map(|d| d["summary_index"].as_u64().unwrap())
        .collect();
    assert_eq!(summary_indices, vec![0, 1, 0]);
}

#[test]
fn reasoning_indices_count_streamed_native_details_only() {
    // Arrange: native summary, foreign summary (must not advance), native
    // summary. The foreign detail neither streams nor bumps the counter.
    let mut state = fresh();
    let mut events = render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "n0"})),
    );
    events.extend(render(
        &mut state,
        foreign_reasoning_chunk(
            ReasoningDetailKind::Summary,
            "rs_1",
            json!({"text": "foreign"}),
        ),
    ));
    events.extend(render(
        &mut state,
        reasoning_chunk(ReasoningDetailKind::Summary, "rs_1", json!({"text": "n1"})),
    ));

    // Act
    events.extend(render_eos_internal(&mut state));

    // Assert: only the two native details streamed, with indices 0 and 1.
    let summary_indices: Vec<u64> = all_data(&events)
        .into_iter()
        .filter(|d| d["type"] == "response.reasoning_summary_text.delta")
        .map(|d| d["summary_index"].as_u64().unwrap())
        .collect();
    assert_eq!(summary_indices, vec![0, 1]);
}

// ---------------------------------------------------------------------------
// Upstream envelope: created_at and message item ids
// ---------------------------------------------------------------------------

fn with_envelope(mut chunk: ChatChunk, envelope: ResponsesEnvelope) -> ChatChunk {
    chunk.upstream_meta = Some(UpstreamMeta::from_responses_envelope(envelope));
    chunk
}

fn item_text_chunk(id: Option<&str>, text: &str) -> ChatChunk {
    match id {
        Some(id) => with_envelope(
            text_chunk(text),
            ResponsesEnvelope::of_message_item_id(id.into()),
        ),
        None => text_chunk(text),
    }
}

fn opening_chunk(created_at: i64) -> ChatChunk {
    let mut chunk = text_chunk("");
    chunk.choices[0].delta.content = None;
    chunk.choices[0].delta.role = Some(routectl_core::Role::Assistant);
    with_envelope(chunk, ResponsesEnvelope::of_created_at(created_at))
}

fn events_named(events: &[SseEvent], name: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some(name))
        .map(|e| serde_json::from_str::<Value>(&e.data).expect("json"))
        .collect()
}

#[test]
fn stream_echoes_upstream_created_at() {
    // Arrange
    let mut state = fresh();

    // Act
    let mut events = render(&mut state, opening_chunk(1_700_000_123));
    events.extend(render(&mut state, text_chunk("hi")));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    for name in [
        "response.created",
        "response.in_progress",
        "response.completed",
    ] {
        assert_eq!(
            data_of(&events, name)["response"]["created_at"],
            1_700_000_123,
            "{name}"
        );
    }
}

#[test]
fn stream_interleaved_message_items_keep_their_own_ids() {
    // Arrange
    let mut state = fresh();

    // Act: item A opens, item B interrupts, A continues without an id
    // (the egress stamps only an item switch), then B again.
    let mut events = render(&mut state, item_text_chunk(Some("msg_a"), "a1"));
    events.extend(render(&mut state, item_text_chunk(None, "a2")));
    events.extend(render(&mut state, item_text_chunk(Some("msg_b"), "b1")));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let added: Vec<Value> = events_named(&events, "response.output_item.added")
        .into_iter()
        .map(|e| e["item"]["id"].clone())
        .collect();
    assert_eq!(added, [json!("msg_a"), json!("msg_b")]);
    let done: Vec<(Value, Value)> = events_named(&events, "response.output_item.done")
        .into_iter()
        .map(|e| {
            (
                e["item"]["id"].clone(),
                e["item"]["content"][0]["text"].clone(),
            )
        })
        .collect();
    assert_eq!(
        done,
        [
            (json!("msg_a"), json!("a1a2")),
            (json!("msg_b"), json!("b1"))
        ]
    );
    let completed = data_of(&events, "response.completed")["response"]["output"].clone();
    let completed: Vec<(Value, Value)> = completed
        .as_array()
        .expect("output array")
        .iter()
        .map(|item| (item["id"].clone(), item["content"][0]["text"].clone()))
        .collect();
    assert_eq!(
        completed,
        [
            (json!("msg_a"), json!("a1a2")),
            (json!("msg_b"), json!("b1"))
        ]
    );
}

#[test]
fn stream_without_an_envelope_keeps_zero_created_at_and_no_item_id() {
    // Arrange
    let mut state = fresh();

    // Act
    let mut events = render(&mut state, text_chunk("hi"));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let completed = data_of(&events, "response.completed")["response"].clone();
    assert_eq!(completed["created_at"], 0);
    assert!(completed["output"][0].get("id").is_none());
    let added = data_of(&events, "response.output_item.added");
    assert!(added["item"].get("id").is_none());
}

// ---------------------------------------------------------------------------
// namespace on returned calls
// ---------------------------------------------------------------------------

/// Stream one call to a namespaced tool and one to a flat tool through a
/// state built from a request whose namespace record maps the first.
fn stream_namespaced_and_flat_calls() -> Vec<SseEvent> {
    use crate::ingress::openai_responses::ResponsesIngress;
    use crate::ingress::{IngressAdapter, RenderContext, StreamRequestContext};

    let ctx = StreamRequestContext {
        render: RenderContext {
            tool_namespaces: [("alpha_wait".to_string(), "ns_alpha".to_string())].into(),
        },
        ..StreamRequestContext::default()
    };
    let mut boxed = ResponsesIngress.new_stream_state(&ctx);
    let state = state_mut(boxed.as_mut());
    let mut events = render(state, tool_chunk(0, "call_ns", "alpha_wait", "{}"));
    events.extend(render(state, tool_chunk(1, "call_flat", "flat_one", "{}")));
    events.extend(render(state, finish_chunk("tool_calls", None)));
    events.extend(render_eos_internal(state));
    events
}

/// Every `function_call` item the stream emitted on `event_name`, keyed by
/// name, in emission order.
fn streamed_call_items(events: &[SseEvent], event_name: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some(event_name))
        .map(|e| serde_json::from_str::<Value>(&e.data).expect("json")["item"].clone())
        .filter(|item| item["type"] == "function_call")
        .collect()
}

#[test]
fn a_streamed_call_to_a_namespaced_tool_carries_its_namespace() {
    // Act
    let events = stream_namespaced_and_flat_calls();

    // Assert
    for event_name in ["response.output_item.added", "response.output_item.done"] {
        let items = streamed_call_items(&events, event_name);
        assert_eq!(items.len(), 2, "{event_name}: {items:?}");
        assert_eq!(items[0]["name"], "alpha_wait");
        assert_eq!(items[0]["namespace"], "ns_alpha", "{event_name}: {items:?}");
    }
    let completed = data_of(&events, "response.completed");
    let output = &completed["response"]["output"];
    assert_eq!(output[0]["name"], "alpha_wait");
    assert_eq!(
        output[0]["namespace"], "ns_alpha",
        "completed body: {output}"
    );
}

#[test]
fn a_streamed_call_to_a_flat_tool_carries_no_namespace() {
    // Act
    let events = stream_namespaced_and_flat_calls();

    // Assert
    for event_name in ["response.output_item.added", "response.output_item.done"] {
        let items = streamed_call_items(&events, event_name);
        assert_eq!(items[1]["name"], "flat_one");
        assert!(
            items[1].get("namespace").is_none(),
            "{event_name}: {items:?}"
        );
    }
    let completed = data_of(&events, "response.completed");
    let output = &completed["response"]["output"];
    assert_eq!(output[1]["name"], "flat_one");
    assert!(
        output[1].get("namespace").is_none(),
        "completed body: {output}"
    );
}

// ---------------------------------------------------------------------------
// carried output items
// ---------------------------------------------------------------------------

fn custom_call(call_id: &str, input: &str) -> Value {
    json!({
        "type": "custom_tool_call",
        "id": format!("ctc_{call_id}"),
        "call_id": call_id,
        "name": "tool_freeform",
        "input": input,
        "status": "completed",
    })
}

fn carried_chunk(item: Value) -> ChatChunk {
    ChatChunk {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: vec![routectl_core::OpaqueSseEvent::OutputItem { item }],
        upstream_meta: None,
    }
}

/// `(event name, output_index, item type)` for every item-level bracket.
fn item_brackets(events: &[SseEvent]) -> Vec<(String, u64, String)> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.event.as_deref(),
                Some("response.output_item.added" | "response.output_item.done")
            )
        })
        .map(|e| {
            let data: Value = serde_json::from_str(&e.data).expect("json");
            (
                e.event.clone().unwrap_or_default(),
                data["output_index"].as_u64().expect("output_index"),
                data["item"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn bracket(name: &str, index: u64, kind: &str) -> (String, u64, String) {
    (name.to_string(), index, kind.to_string())
}

#[test]
fn a_carried_item_after_text_closes_the_message_and_takes_the_next_index() {
    // Arrange
    let mut state = fresh();
    let item = custom_call("call_a", "ls -la");

    // Act
    let mut events = render(&mut state, text_chunk("running it"));
    events.extend(render(&mut state, carried_chunk(item.clone())));
    events.extend(render(&mut state, finish_chunk("stop", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    assert_eq!(
        item_brackets(&events),
        [
            bracket("response.output_item.added", 0, "message"),
            bracket("response.output_item.done", 0, "message"),
            bracket("response.output_item.added", 1, "custom_tool_call"),
            bracket("response.output_item.done", 1, "custom_tool_call"),
        ]
    );
    let done = events_named(&events, "response.output_item.done");
    assert_eq!(
        done[1]["item"], item,
        "the done item is the upstream's, whole"
    );
    let output = data_of(&events, "response.completed")["response"]["output"].clone();
    assert_eq!(output.as_array().map(Vec::len), Some(2), "{output}");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[1], item, "{output}");
}

#[test]
fn two_carried_items_take_dense_indices_in_arrival_order() {
    // Arrange
    let mut state = fresh();
    let first = custom_call("call_a", "pwd");
    let second = custom_call("call_b", "date");

    // Act
    let mut events = render(&mut state, text_chunk("two steps"));
    events.extend(render(&mut state, carried_chunk(first.clone())));
    events.extend(render(&mut state, carried_chunk(second.clone())));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let added: Vec<(u64, Value)> = events_named(&events, "response.output_item.added")
        .into_iter()
        .map(|d| {
            (
                d["output_index"].as_u64().expect("index"),
                d["item"].clone(),
            )
        })
        .filter(|(_, item)| item["type"] == "custom_tool_call")
        .collect();
    assert_eq!(added, [(1, first.clone()), (2, second.clone())]);
    let output = data_of(&events, "response.completed")["response"]["output"].clone();
    assert_eq!(output.as_array().map(Vec::len), Some(3), "{output}");
    assert_eq!(output[1], first, "{output}");
    assert_eq!(output[2], second, "{output}");
}

#[test]
fn a_carried_item_keeps_its_index_ahead_of_flushed_function_calls() {
    // Arrange
    let mut state = fresh();
    let item = custom_call("call_a", "ls");

    // Act
    let mut events = render(&mut state, text_chunk("both kinds"));
    events.extend(render(&mut state, carried_chunk(item.clone())));
    events.extend(render(&mut state, tool_chunk(0, "call_f", "lookup", "{}")));
    events.extend(render(&mut state, finish_chunk("tool_calls", None)));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let added: Vec<(u64, String)> = item_brackets(&events)
        .into_iter()
        .filter(|(name, _, _)| name == "response.output_item.added")
        .map(|(_, index, kind)| (index, kind))
        .collect();
    assert_eq!(
        added,
        [
            (0, "message".to_string()),
            (1, "custom_tool_call".to_string()),
            (2, "function_call".to_string()),
        ]
    );
    let output = data_of(&events, "response.completed")["response"]["output"].clone();
    let kinds: Vec<&str> = output
        .as_array()
        .expect("output")
        .iter()
        .filter_map(|i| i["type"].as_str())
        .collect();
    assert_eq!(kinds, ["message", "custom_tool_call", "function_call"]);
}

#[test]
fn a_carried_item_before_any_text_sits_ahead_of_the_message() {
    // Arrange
    let mut state = fresh();
    let item = custom_call("call_a", "ls");

    // Act
    let mut events = render(&mut state, carried_chunk(item.clone()));
    events.extend(render(&mut state, text_chunk("after")));
    events.extend(render_eos_internal(&mut state));

    // Assert
    let output = data_of(&events, "response.completed")["response"]["output"].clone();
    assert_eq!(output[0], item, "{output}");
    assert_eq!(output[1]["type"], "message", "{output}");
    let message_added = events_named(&events, "response.output_item.added")
        .into_iter()
        .find(|d| d["item"]["type"] == "message")
        .expect("message added");
    assert_eq!(message_added["output_index"], 1);
}

// ---------------------------------------------------------------------------
// Upstream usage members the canonical delta does not model
// ---------------------------------------------------------------------------

/// The completed body's usage after a finish chunk whose usage carries
/// `members`, its carrier stamped with `dialect`; `usage_only` sends the
/// usage on a trailing chunk with no choice.
fn completed_usage_with_members(
    dialect: Option<routectl_core::wire_check::IngressDialect>,
    members: Value,
    usage_only: bool,
) -> Value {
    let mut state = fresh();
    let _ = render(&mut state, text_chunk("hi"));
    let Value::Object(extras) = members else {
        panic!("members must be an object");
    };
    let usage = UsageDelta {
        prompt_tokens: Some(12),
        completion_tokens: Some(7),
        total_tokens: Some(19),
        reasoning_tokens: Some(3),
        cache_read_input_tokens: Some(4),
        extras,
        ..Default::default()
    };
    let mut chunk = if usage_only {
        let _ = render(&mut state, finish_chunk("stop", None));
        ChatChunk {
            choices: Vec::new(),
            ..finish_chunk("stop", Some(usage))
        }
    } else {
        finish_chunk("stop", Some(usage))
    };
    chunk.upstream_meta = dialect.map(|d| UpstreamMeta::default().with_wire_dialect(d));
    let _ = render(&mut state, chunk);
    let events = render_eos_internal(&mut state);
    data_of(&events, "response.completed")["response"]["usage"].clone()
}

#[test]
fn a_responses_stamped_usage_renders_its_members_with_typed_counts_winning() {
    for usage_only in [false, true] {
        // Act
        let u = completed_usage_with_members(
            Some(routectl_core::wire_check::IngressDialect::Responses),
            json!({"input_tokens_details": {"cached_tokens": 999, "audio_tokens": 1},
                   "output_tokens_details": {"reasoning_tokens": 999, "x": 2},
                   "input_tokens": 999, "cost": 5}),
            usage_only,
        );

        // Assert
        assert_eq!(
            u,
            json!({"input_tokens": 12, "output_tokens": 7, "total_tokens": 19,
                   "input_tokens_details": {"cached_tokens": 4, "audio_tokens": 1},
                   "output_tokens_details": {"reasoning_tokens": 3, "x": 2},
                   "cost": 5}),
            "usage_only={usage_only}"
        );
    }
}

#[test]
fn usage_members_on_an_unstamped_or_foreign_chunk_are_not_rendered() {
    let rows = [
        ("unstamped", None),
        (
            "anthropic",
            Some(routectl_core::wire_check::IngressDialect::Anthropic),
        ),
        (
            "chat",
            Some(routectl_core::wire_check::IngressDialect::Chat),
        ),
    ];
    for (row, dialect) in rows {
        for usage_only in [false, true] {
            // Act
            let u = completed_usage_with_members(
                dialect,
                json!({"output_tokens_details": {"x": 2}, "cost": 5}),
                usage_only,
            );

            // Assert
            assert_eq!(
                u,
                json!({"input_tokens": 12, "output_tokens": 7, "total_tokens": 19,
                       "input_tokens_details": {"cached_tokens": 4},
                       "output_tokens_details": {"reasoning_tokens": 3}}),
                "{row} usage_only={usage_only}"
            );
        }
    }
}
