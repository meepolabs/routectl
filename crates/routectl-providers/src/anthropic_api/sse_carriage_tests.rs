//! Event-by-event carriage of unmodeled members and unknown events.

use serde_json::{Value, json};

use super::*;
use crate::anthropic_api::sse_carriage::oversize_event_total;

const PROVIDER: &str = "test-anthropic";

fn carrying() -> SseState {
    SseState::default().with_unmodeled_carriage()
}

fn feed(state: &mut SseState, event: &Value) -> Option<ChatChunk> {
    state
        .parse_event(PROVIDER, &event.to_string())
        .expect("event parses")
}

fn message_start() -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 0},
        "container": {"id": "cntr_1"}, "context_management": {"applied_edits": []}}})
}

/// A whole stream carrying every kind of unmodeled member and event, one
/// event per line in upstream order.
fn carrying_stream() -> Vec<Value> {
    vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "tool_use", "id": "toolu_1", "name": "lookup",
                                 "input": {}, "caller": {"type": "direct"}}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "future_delta", "x": "é"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "future_event", "payload": {"n": 1}}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "tool_use", "stop_sequence": null,
                         "stop_details": {"type": "x"}, "container": {"id": "cntr_1"}},
               "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]
}

/// Each chunk's carried events, with the chunk's position in the stream.
fn carried_by_chunk(state: &mut SseState, events: &[Value]) -> Vec<(usize, Vec<OpaqueSseEvent>)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| feed(state, e).map(|chunk| (i, chunk.opaque_events)))
        .collect()
}

#[test]
fn each_unmodeled_member_and_event_rides_the_chunk_of_its_own_event() {
    // Arrange
    let mut state = carrying();

    // Act
    let chunks = carried_by_chunk(&mut state, &carrying_stream());

    // Assert
    let at = |event: usize| -> &Vec<OpaqueSseEvent> {
        &chunks
            .iter()
            .find(|(i, _)| *i == event)
            .unwrap_or_else(|| panic!("event {event} produced no chunk: {chunks:?}"))
            .1
    };
    match at(0).as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                event_type,
                upstream_index: None,
                members,
            },
        ] => {
            assert_eq!(event_type, "message_start");
            assert_eq!(
                members.keys().collect::<Vec<_>>(),
                ["container", "context_management"]
            );
        }
        other => panic!("message_start carriage: {other:?}"),
    }
    match at(1).as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                event_type,
                upstream_index: Some(0),
                members,
            },
        ] => {
            assert_eq!(event_type, "content_block_start");
            assert_eq!(members.keys().collect::<Vec<_>>(), ["caller", "type"]);
            assert_eq!(members["type"], "tool_use");
        }
        other => panic!("tool_use start carriage: {other:?}"),
    }
    assert!(at(2).is_empty(), "a typed delta carries nothing");
    match at(3).as_slice() {
        [
            OpaqueSseEvent::UnknownBlockDelta {
                upstream_index: 0,
                delta,
            },
        ] => {
            assert_eq!(delta["x"], "é");
        }
        other => panic!("unknown delta carriage: {other:?}"),
    }
    match at(5).as_slice() {
        [OpaqueSseEvent::UnknownEvent { event_type, event }] => {
            assert_eq!(event_type, "future_event");
            assert_eq!(event["payload"]["n"], 1);
        }
        other => panic!("unknown event carriage: {other:?}"),
    }
    match at(6).as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                event_type,
                members,
                ..
            },
        ] => {
            assert_eq!(event_type, "message_delta");
            assert_eq!(
                members.keys().collect::<Vec<_>>(),
                ["container", "stop_details"]
            );
        }
        other => panic!("message_delta carriage: {other:?}"),
    }
}

#[test]
fn a_client_of_another_dialect_gets_no_carriage_and_no_extra_chunks() {
    // Arrange
    let mut carrying_state = carrying();
    let mut plain_state = SseState::default();

    // Act
    let carried = carried_by_chunk(&mut carrying_state, &carrying_stream());
    let plain = carried_by_chunk(&mut plain_state, &carrying_stream());

    // Assert
    assert!(
        plain.iter().all(|(_, opaque)| opaque.is_empty()),
        "nothing carried for a foreign client: {plain:?}"
    );
    let positions = |chunks: &[(usize, Vec<OpaqueSseEvent>)]| -> Vec<usize> {
        chunks.iter().map(|(i, _)| *i).collect()
    };
    assert_eq!(
        positions(&plain),
        [0, 1, 2, 6],
        "foreign client chunk shape unchanged"
    );
    assert_eq!(
        positions(&carried),
        [0, 1, 2, 3, 4, 5, 6],
        "carrier chunks at their own events"
    );
}

#[test]
fn a_known_block_start_without_members_still_marks_its_index() {
    // Arrange
    let mut state = carrying();
    feed(&mut state, &message_start());

    // Act
    let chunk = feed(
        &mut state,
        &json!({"type": "content_block_start", "index": 2,
                "content_block": {"type": "text", "text": ""}}),
    )
    .expect("the block start rides its own chunk");

    // Assert
    match chunk.opaque_events.as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                event_type,
                upstream_index: Some(2),
                members,
            },
        ] => {
            assert_eq!(event_type, "content_block_start");
            assert_eq!(Value::Object(members.clone()), json!({"type": "text"}));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        chunk.choices.is_empty(),
        "no canonical content on the marker"
    );
}

#[test]
fn a_malformed_event_of_a_modeled_type_is_not_carried_as_a_new_event() {
    // Arrange
    let mut state = carrying();
    feed(&mut state, &message_start());

    // Act
    let chunk = feed(
        &mut state,
        &json!({"type": "content_block_delta", "index": "zero", "delta": {}}),
    );

    // Assert
    assert!(
        chunk.is_none(),
        "malformed modeled event sink-drains: {chunk:?}"
    );
}

#[test]
fn an_error_event_stays_an_error_with_carriage_on() {
    // Arrange
    let mut state = carrying();
    feed(&mut state, &message_start());

    // Act
    let result = state.parse_event(
        PROVIDER,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#,
    );

    // Assert
    assert!(
        matches!(result, Err(Error::Upstream { status: 529, .. })),
        "{result:?}"
    );
}

#[test]
fn a_carried_event_over_the_cap_ends_the_stream_and_counts_once() {
    // Arrange
    let mut state = carrying();
    feed(&mut state, &message_start());
    let before = oversize_event_total();
    let blob = "x".repeat(super::super::sse_opaque::MAX_OPAQUE_BYTES_PER_BLOCK + 1);
    let event = json!({"type": "future_event", "blob": blob});

    // Act
    let result = state.parse_event(PROVIDER, &event.to_string());

    // Assert
    assert!(matches!(result, Err(Error::Streaming(_))), "{result:?}");
    assert_eq!(oversize_event_total(), before + 1);
}

#[test]
fn a_foreign_client_never_trips_the_carriage_cap() {
    // Arrange
    let mut state = SseState::default();
    feed(&mut state, &message_start());
    let before = oversize_event_total();
    let blob = "x".repeat(super::super::sse_opaque::MAX_OPAQUE_BYTES_PER_BLOCK + 1);

    // Act
    let result = state.parse_event(
        PROVIDER,
        &json!({"type": "future_event", "blob": blob}).to_string(),
    );

    // Assert
    assert!(matches!(result, Ok(None)), "{result:?}");
    assert_eq!(oversize_event_total(), before);
}
