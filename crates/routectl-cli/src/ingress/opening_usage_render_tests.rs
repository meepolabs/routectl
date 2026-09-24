//! OpenAI Chat and Responses ingresses keep their first-event sequence when
//! the opening chunk carries the transport-internal opening usage, and report
//! usage only at the end. The chunks are produced by the real Anthropic SSE
//! parser (the translated lane, where the opening carrier actually exists),
//! so the positive control is that the rendered opener DID carry it.

use super::openai::OpenAiIngress;
use super::openai_responses::ResponsesIngress;
use super::{IngressAdapter, SseEvent, StreamRequestContext};
use routectl_core::ChatChunk;
use routectl_providers::anthropic_api::sse::SseState;
use serde_json::Value;

const INPUT: u64 = 1_234;
const CACHE_WRITE: u64 = 5_678;
const CACHE_READ: u64 = 91_011;
const OUTPUT: u64 = 42;
const PROMPT_TOTAL: u64 = INPUT + CACHE_WRITE + CACHE_READ;

fn anthropic_events() -> Vec<String> {
    vec![
        format!(
            r#"{{"type":"message_start","message":{{"id":"msg_meter","type":"message","role":"assistant","content":[],"model":"claude-opus-4-7","usage":{{"input_tokens":{INPUT},"output_tokens":1,"cache_creation_input_tokens":{CACHE_WRITE},"cache_read_input_tokens":{CACHE_READ}}}}}}}"#
        ),
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            .to_string(),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#
            .to_string(),
        r#"{"type":"content_block_stop","index":0}"#.to_string(),
        format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"usage":{{"output_tokens":{OUTPUT}}}}}"#
        ),
        r#"{"type":"message_stop"}"#.to_string(),
    ]
}

fn parsed_chunks() -> Vec<ChatChunk> {
    let mut state = SseState::default();
    anthropic_events()
        .iter()
        .filter_map(|event| state.parse_event("test", event).expect("event parses"))
        .collect()
}

fn without_carrier(chunks: &[ChatChunk]) -> Vec<ChatChunk> {
    chunks
        .iter()
        .cloned()
        .map(|mut chunk| {
            chunk.upstream_meta = None;
            chunk
        })
        .collect()
}

fn render_all(adapter: &dyn IngressAdapter, chunks: Vec<ChatChunk>) -> Vec<Vec<SseEvent>> {
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    let mut frames: Vec<Vec<SseEvent>> = chunks
        .into_iter()
        .map(|chunk| {
            adapter
                .render_chunk(chunk, state.as_mut())
                .expect("renders")
        })
        .collect();
    frames.push(adapter.render_eos(state.as_mut()));
    frames
}

/// Event names plus JSON bodies, minus the OpenAI wall-clock `created`
/// stamp, so two renders of the same stream compare deterministically.
fn comparable(frames: &[Vec<SseEvent>]) -> Vec<Vec<(Option<String>, Value)>> {
    frames
        .iter()
        .map(|events| {
            events
                .iter()
                .map(|e| {
                    let mut data = serde_json::from_str::<Value>(&e.data)
                        .unwrap_or_else(|_| Value::String(e.data.clone()));
                    if let Some(obj) = data.as_object_mut() {
                        obj.remove("created");
                    }
                    (e.event.clone(), data)
                })
                .collect()
        })
        .collect()
}

fn json_of(event: &SseEvent) -> Value {
    serde_json::from_str(&event.data).expect("event data is JSON")
}

fn assert_parsed_opener_carries_opening_usage(chunks: &[ChatChunk]) {
    let opening = chunks[0]
        .upstream_meta
        .as_ref()
        .and_then(|m| m.opening_usage.as_ref())
        .expect("positive control: the parsed opener carries the opening usage");
    assert_eq!(u64::from(opening.input_tokens), INPUT);
    assert!(chunks[0].usage.is_none());
}

#[test]
fn openai_first_frame_is_unchanged_by_the_opening_carrier() {
    // Arrange
    let chunks = parsed_chunks();
    assert_parsed_opener_carries_opening_usage(&chunks);

    // Act
    let with = render_all(&OpenAiIngress, chunks.clone());
    let without = render_all(&OpenAiIngress, without_carrier(&chunks));

    // Assert
    assert_eq!(comparable(&with), comparable(&without));
    let first = json_of(&with[0][0]);
    assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
    assert!(first.get("usage").is_none(), "no early usage: {first}");
    assert!(!with[0][0].data.contains(&INPUT.to_string()));
}

#[test]
fn openai_reports_exact_usage_only_on_the_terminal_chunk() {
    // Arrange
    let chunks = parsed_chunks();

    // Act
    let frames = render_all(&OpenAiIngress, chunks);

    // Assert
    let with_usage: Vec<Value> = frames
        .iter()
        .flatten()
        .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
        .filter(|v| v.get("usage").is_some())
        .collect();
    assert_eq!(with_usage.len(), 1, "usage rides exactly one frame");
    let terminal = &with_usage[0];
    assert_eq!(terminal["choices"][0]["finish_reason"], "stop");
    let usage = &terminal["usage"];
    assert_eq!(usage["prompt_tokens"], PROMPT_TOTAL);
    assert_eq!(usage["cache_creation_input_tokens"], CACHE_WRITE);
    assert_eq!(usage["cache_read_input_tokens"], CACHE_READ);
    assert_eq!(usage["completion_tokens"], OUTPUT);
    assert_eq!(usage["total_tokens"], PROMPT_TOTAL + OUTPUT);
}

#[test]
fn responses_first_events_are_unchanged_by_the_opening_carrier() {
    // Arrange
    let chunks = parsed_chunks();
    assert_parsed_opener_carries_opening_usage(&chunks);

    // Act
    let with = render_all(&ResponsesIngress, chunks.clone());
    let without = render_all(&ResponsesIngress, without_carrier(&chunks));

    // Assert
    assert_eq!(comparable(&with), comparable(&without));
    let names: Vec<Option<&str>> = with[0].iter().map(|e| e.event.as_deref()).collect();
    assert_eq!(
        names,
        vec![Some("response.created"), Some("response.in_progress")]
    );
    for event in &with[0] {
        let body = json_of(event);
        assert!(
            body["response"].get("usage").is_none(),
            "no early usage: {body}"
        );
    }
}

#[test]
fn responses_reports_exact_usage_only_on_completion() {
    // Arrange
    let chunks = parsed_chunks();

    // Act
    let frames = render_all(&ResponsesIngress, chunks);

    // Assert
    let all: Vec<&SseEvent> = frames.iter().flatten().collect();
    let with_usage: Vec<&SseEvent> = all
        .iter()
        .copied()
        .filter(|e| json_of(e)["response"].get("usage").is_some())
        .collect();
    assert_eq!(with_usage.len(), 1, "usage rides exactly one event");
    assert_eq!(with_usage[0].event.as_deref(), Some("response.completed"));
    let usage = &json_of(with_usage[0])["response"]["usage"];
    assert_eq!(usage["input_tokens"], PROMPT_TOTAL);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], CACHE_READ);
    assert_eq!(usage["output_tokens"], OUTPUT);
    assert_eq!(usage["total_tokens"], PROMPT_TOTAL + OUTPUT);
}
