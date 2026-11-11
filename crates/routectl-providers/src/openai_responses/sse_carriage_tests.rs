//! Carriage of the stream events and event members the canonical chunk has
//! no slot for: a Responses client gets each on the chunk of the event that
//! carried it; any other client gets nothing.

use routectl_core::wire_check::IngressDialect;
use routectl_core::{ChatChunk, OpaqueSseEvent, RequestProvenance};
use serde_json::{Map, Value, json};

use super::ResponsesStreamState;
use super::carriage::{KNOWN_EVENT_TYPES, MANAGED_EVENT_KEYS};
use crate::openai_responses::AuthKind;

fn state(provenance: RequestProvenance) -> ResponsesStreamState {
    ResponsesStreamState::new(AuthKind::ApiKey, provenance, None, &[], None)
}

/// Each event through the real data-line parser, one chunk list per event.
fn drive(state: &mut ResponsesStreamState, events: &[Value]) -> Vec<Vec<ChatChunk>> {
    events
        .iter()
        .map(|event| {
            let parsed = super::parse_data_line("test", &event.to_string()).expect("event parses");
            state.parse_event("test", parsed).expect("event processing")
        })
        .collect()
}

fn carried(chunks: &[ChatChunk]) -> Vec<&OpaqueSseEvent> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.opaque_events)
        .filter(|event| {
            matches!(
                event,
                OpaqueSseEvent::UnknownEvent { .. } | OpaqueSseEvent::KnownEventExtras { .. }
            )
        })
        .collect()
}

fn members(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn created() -> Value {
    json!({"type": "response.created", "sequence_number": 0,
           "response": {"id": "resp_1", "model": "m", "created_at": 1}})
}

fn message_added() -> Value {
    json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
           "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})
}

fn unknown_event() -> Value {
    json!({"type": "response.audit.note", "sequence_number": 2, "output_index": 0,
           "item_id": "msg_1", "note": "caf\u{e9}"})
}

fn text_delta() -> Value {
    json!({"type": "response.output_text.delta", "sequence_number": 3, "output_index": 0,
           "content_index": 0, "delta": "ok", "logprobs": [], "obfuscation": "x1"})
}

#[test]
fn an_unknown_event_rides_whole_on_its_own_chunk_in_upstream_order() {
    // Arrange
    let mut state = state(RequestProvenance::ResponsesIngress);

    // Act
    let per_event = drive(
        &mut state,
        &[created(), message_added(), unknown_event(), text_delta()],
    );

    // Assert: the unknown event is the only payload of the one chunk its
    // event produced, between the item it names and the next text.
    let chunks = &per_event[2];
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert!(chunks[0].choices[0].delta.content.is_none());
    match carried(chunks).as_slice() {
        [OpaqueSseEvent::UnknownEvent { event_type, event }] => {
            assert_eq!(event_type, "response.audit.note");
            assert_eq!(event, &unknown_event());
        }
        other => panic!("expected the unknown event, got {other:?}"),
    }
    let meta = chunks[0].upstream_meta.as_ref().expect("stamped");
    assert_eq!(meta.wire_dialect, Some(IngressDialect::Responses));
    assert_eq!(
        per_event[3][0].choices[0].delta.content.as_deref(),
        Some("ok")
    );
}

#[test]
fn members_of_a_known_event_ride_on_that_events_own_chunk_without_managed_ones() {
    // Arrange
    let mut state = state(RequestProvenance::ResponsesIngress);

    // Act
    let per_event = drive(&mut state, &[created(), message_added(), text_delta()]);

    // Assert: the text chunk itself carries the delta's unmodeled members,
    // and none of the members the ingress writes itself.
    let chunks = &per_event[2];
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("ok"));
    match carried(chunks).as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                event_type,
                upstream_index,
                members: carried,
            },
        ] => {
            assert_eq!(event_type, "response.output_text.delta");
            assert_eq!(*upstream_index, Some(0));
            assert_eq!(
                carried,
                &members(json!({"logprobs": [], "obfuscation": "x1"}))
            );
        }
        other => panic!("expected the delta's members, got {other:?}"),
    }
}

#[test]
fn an_item_added_is_carried_with_its_identity_even_without_members() {
    // Arrange
    let mut state = state(RequestProvenance::ResponsesIngress);

    // Act
    let per_event = drive(&mut state, &[created(), message_added()]);

    // Assert
    match carried(&per_event[1]).as_slice() {
        [
            OpaqueSseEvent::KnownEventExtras {
                members: carried,
                upstream_index,
                ..
            },
        ] => {
            assert_eq!(*upstream_index, Some(0));
            assert_eq!(
                carried,
                &members(json!({"item": {"type": "message", "id": "msg_1"}}))
            );
        }
        other => panic!("expected the item identity, got {other:?}"),
    }
}

#[test]
fn only_a_responses_client_gets_carried_events_and_members() {
    // (row, provenance, carried events expected)
    let rows = [
        ("responses", RequestProvenance::ResponsesIngress, 3),
        ("anthropic", RequestProvenance::AnthropicIngress, 0),
        ("chat", RequestProvenance::OpenaiIngress, 0),
        ("library", RequestProvenance::Library, 0),
    ];
    for (row, provenance, expected) in rows {
        // Arrange
        let mut state = state(provenance);

        // Act
        let chunks: Vec<ChatChunk> = drive(
            &mut state,
            &[created(), message_added(), unknown_event(), text_delta()],
        )
        .into_iter()
        .flatten()
        .collect();

        // Assert: the item identity, the unknown event and the delta's
        // members; `response.created` has no member to carry.
        assert_eq!(carried(&chunks).len(), expected, "{row}");
        let text = chunks
            .iter()
            .filter(|c| c.choices[0].delta.content.is_some())
            .count();
        assert_eq!(text, 1, "{row}: the text still streams");
        assert_eq!(chunks.len(), if expected > 0 { 4 } else { 2 }, "{row}");
    }
}

#[test]
fn an_error_event_is_never_carried() {
    // Arrange
    let mut state = state(RequestProvenance::ResponsesIngress);

    // Act
    let per_event = drive(
        &mut state,
        &[
            created(),
            json!({"type": "error", "code": "x", "message": "m"}),
        ],
    );

    // Assert
    assert!(per_event[1].is_empty(), "{:?}", per_event[1]);
}

#[test]
fn the_completed_body_top_level_members_ride_on_the_terminal_chunk() {
    // (row, provenance, members expected)
    let rows = [
        ("responses", RequestProvenance::ResponsesIngress, true),
        ("anthropic", RequestProvenance::AnthropicIngress, false),
    ];
    for (row, provenance, expected) in rows {
        // Arrange
        let mut state = state(provenance);
        let completed = json!({"type": "response.completed", "response": {
            "id": "resp_1", "model": "m", "status": "completed", "output": [],
            "service_tier": "default", "store": false}});

        // Act
        let per_event = drive(&mut state, &[created(), completed]);

        // Assert
        let terminal = per_event[1].last().expect("terminal chunk");
        let extras = terminal
            .upstream_meta
            .as_ref()
            .and_then(|meta| meta.openai_responses.as_ref())
            .map(|envelope| envelope.extras.clone())
            .unwrap_or_default();
        if expected {
            assert_eq!(
                extras,
                members(json!({"service_tier": "default", "store": false})),
                "{row}"
            );
            assert_eq!(
                terminal.upstream_meta.as_ref().and_then(|m| m.wire_dialect),
                Some(IngressDialect::Responses)
            );
        } else {
            assert!(extras.is_empty(), "{row}: {extras:?}");
        }
    }
}

#[test]
fn reasoning_item_members_ride_on_their_item_events() {
    // Arrange
    let mut state = state(RequestProvenance::ResponsesIngress);
    let item = |status: &str| {
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "status": status,
               "metadata": {"turn": "t1"},
               "internal_chat_message_metadata_passthrough": {"k": "caf\u{e9}"}})
    };

    // Act
    let per_event = drive(
        &mut state,
        &[
            created(),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": item("in_progress")}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": item("completed")}),
        ],
    );

    // Assert
    let item_members = |chunks: &[ChatChunk], done: bool| {
        chunks
            .iter()
            .filter_map(|c| c.upstream_meta.as_ref()?.openai_responses.as_ref())
            .map(|envelope| {
                if done {
                    envelope.item_members.reasoning.clone()
                } else {
                    envelope.added_item_members.reasoning.clone()
                }
            })
            .find(|members| !members.is_empty())
            .expect("reasoning members carried")
    };
    assert_eq!(
        item_members(&per_event[1], false)["rs_1"],
        members(json!({"status": "in_progress", "metadata": {"turn": "t1"},
                       "internal_chat_message_metadata_passthrough": {"k": "caf\u{e9}"}}))
    );
    assert_eq!(
        item_members(&per_event[2], true)["rs_1"]["status"],
        "completed"
    );
}

/// Every event type the Responses ingress renders is one the carriage
/// treats as known, so it never also arrives whole as an unknown event.
#[test]
fn every_event_type_the_ingress_emits_is_known_to_the_carriage() {
    for emitted in [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.output_item.done",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_text.delta",
        "response.output_text.done",
        "response.reasoning_summary_text.delta",
        "response.reasoning_text.delta",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
        "response.completed",
        "response.failed",
    ] {
        assert!(KNOWN_EVENT_TYPES.contains(&emitted), "{emitted}");
    }
    assert!(MANAGED_EVENT_KEYS.contains(&"sequence_number"));
}
