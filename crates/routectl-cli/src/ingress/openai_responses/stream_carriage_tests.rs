//! Re-emission of carried Responses upstream events and members, through
//! the real stream render, against chunks shaped as the openai-responses
//! egress builds them.

use routectl_core::wire_check::{EditKind, EditSite, IngressDialect};
use routectl_core::{
    ChatChunk, ChunkChoice, ChunkDelta, OpaqueSseEvent, ReasoningDetail, ReasoningDetailKind,
    ResponsesEnvelope, UpstreamMeta,
};
use serde_json::{Map, Value, json};
use serial_test::serial;

use crate::ingress::{ResponseEdits, SseEvent};

use super::super::ResponsesStreamState;
use super::super::stream::{render_chunk_declared, render_eos_declared};

fn members(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn stamped() -> Option<UpstreamMeta> {
    Some(UpstreamMeta::default().with_wire_dialect(IngressDialect::Responses))
}

fn chunk(delta: ChunkDelta, opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    ChatChunk {
        id: "resp_1".into(),
        model: "m".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: opaque,
        upstream_meta: stamped(),
    }
}

fn carrier(opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    chunk(ChunkDelta::default(), opaque)
}

fn text(text: &str, opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    chunk(
        ChunkDelta {
            content: Some(text.into()),
            ..Default::default()
        },
        opaque,
    )
}

fn message_item_id(mut chunk: ChatChunk, id: &str) -> ChatChunk {
    chunk.upstream_meta = Some(
        UpstreamMeta::from_responses_envelope(ResponsesEnvelope::of_message_item_id(id.into()))
            .with_wire_dialect(IngressDialect::Responses),
    );
    chunk
}

fn finish() -> ChatChunk {
    let mut chunk = carrier(Vec::new());
    chunk.choices[0].finish_reason = Some("stop".into());
    chunk
}

fn added(index: u32, item: Value) -> OpaqueSseEvent {
    OpaqueSseEvent::KnownEventExtras {
        event_type: "response.output_item.added".into(),
        upstream_index: Some(index),
        members: members(json!({"item": item})),
    }
}

fn extras(event_type: &str, index: Option<u32>, value: Value) -> OpaqueSseEvent {
    OpaqueSseEvent::KnownEventExtras {
        event_type: event_type.into(),
        upstream_index: index,
        members: members(value),
    }
}

fn unknown(event: Value) -> OpaqueSseEvent {
    OpaqueSseEvent::UnknownEvent {
        event_type: event["type"].as_str().expect("type").into(),
        event,
    }
}

fn render(chunks: Vec<ChatChunk>) -> (Vec<SseEvent>, ResponseEdits) {
    let mut state = ResponsesStreamState::default();
    let mut edits = ResponseEdits::recording();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(render_chunk_declared(chunk, &mut state, &mut edits).expect("renders"));
    }
    events.extend(render_eos_declared(&mut state, &mut edits));
    (events, edits)
}

fn payloads(events: &[SseEvent], name: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some(name))
        .map(|e| serde_json::from_str(&e.data).expect("json"))
        .collect()
}

fn names(events: &[SseEvent]) -> Vec<&str> {
    events.iter().filter_map(|e| e.event.as_deref()).collect()
}

fn sequence(events: &[SseEvent]) -> Vec<u64> {
    events
        .iter()
        .map(|e| {
            serde_json::from_str::<Value>(&e.data).expect("json")["sequence_number"]
                .as_u64()
                .expect("sequence_number")
        })
        .collect()
}

/// The paths the carriage declared withheld.
fn removed_paths(edits: &ResponseEdits) -> Vec<String> {
    edits
        .as_slice()
        .iter()
        .filter(|e| e.kind() == EditKind::Remove && e.site() == EditSite::StreamEnvelopeStamp)
        .map(|e| e.path().to_owned())
        .collect()
}

fn withheld_count() -> u64 {
    routectl_providers::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-responses" && e.drop_class == "unknown_member_withheld")
        .map_or(0, |e| e.drop_count)
}

const NOTE: &str = "response.audit.note";

/// A message item the upstream streamed at `output_index` 3, whose text the
/// render emits as its own item 0.
fn message_at_three(opaque_after_open: Vec<OpaqueSseEvent>) -> Vec<ChatChunk> {
    vec![
        carrier(vec![added(3, json!({"type": "message", "id": "msg_1"}))]),
        message_item_id(text("ok", Vec::new()), "msg_1"),
        carrier(opaque_after_open),
        finish(),
    ]
}

#[test]
fn an_unknown_event_for_an_open_item_is_restamped_to_the_renders_own_item() {
    // Arrange
    let event = json!({"type": NOTE, "sequence_number": 41, "output_index": 3,
                       "item_id": "msg_1", "note": "caf\u{e9}"});

    // Act
    let (events, _) = render(message_at_three(vec![unknown(event)]));

    // Assert
    let notes = payloads(&events, NOTE);
    assert_eq!(notes.len(), 1, "{:?}", names(&events));
    assert_eq!(notes[0]["output_index"], 0);
    assert_eq!(notes[0]["item_id"], "msg_1");
    assert_eq!(notes[0]["note"], "caf\u{e9}");
    let position = names(&events)
        .iter()
        .position(|n| *n == NOTE)
        .expect("note");
    let delta = names(&events)
        .iter()
        .position(|n| *n == "response.output_text.delta")
        .expect("delta");
    assert!(delta < position, "{:?}", names(&events));
}

#[test]
fn sequence_numbers_stay_dense_across_carried_events() {
    // Arrange
    let event = json!({"type": NOTE, "sequence_number": 41, "output_index": 3,
                       "item_id": "msg_1"});
    let stream_event = json!({"type": "response.rate_note", "sequence_number": 7});

    // Act
    let (events, _) = render(message_at_three(vec![
        unknown(event),
        unknown(stream_event),
    ]));

    // Assert
    let expected: Vec<u64> = (0..events.len() as u64).collect();
    assert_eq!(sequence(&events), expected);
}

#[test]
fn an_unknown_event_naming_no_item_is_emitted_in_place() {
    // Arrange
    let event = json!({"type": "response.rate_note", "sequence_number": 7, "remaining": 3});

    // Act
    let (events, edits) = render(vec![carrier(vec![unknown(event)]), text("ok", Vec::new())]);

    // Assert
    let notes = payloads(&events, "response.rate_note");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["remaining"], 3);
    assert!(removed_paths(&edits).is_empty(), "{:?}", edits.as_slice());
}

#[test]
fn an_unknown_event_for_an_item_not_opened_yet_follows_the_items_opening() {
    // Arrange: the upstream sends the event between the item's added and
    // its first text; the render opens its item only at that text.
    let event = json!({"type": NOTE, "output_index": 3, "item_id": "msg_1"});
    let chunks = vec![
        carrier(vec![added(3, json!({"type": "message", "id": "msg_1"}))]),
        carrier(vec![unknown(event)]),
        message_item_id(text("ok", Vec::new()), "msg_1"),
    ];

    // Act
    let (events, _) = render(chunks);

    // Assert
    let names = names(&events);
    let note = names.iter().position(|n| *n == NOTE).expect("note");
    let part = names
        .iter()
        .position(|n| *n == "response.content_part.added")
        .expect("part");
    let delta = names
        .iter()
        .position(|n| *n == "response.output_text.delta")
        .expect("delta");
    assert!(part < note && note < delta, "{names:?}");
    assert_eq!(payloads(&events, NOTE)[0]["output_index"], 0);
}

#[test]
#[serial(responses_unknown_member_withheld)]
fn an_event_whose_index_cannot_be_mapped_is_withheld_declared_and_counted_once() {
    // Arrange: two events naming an upstream index no item was added at.
    let before = withheld_count();
    let first = json!({"type": NOTE, "output_index": 9, "note": "a"});
    let second = json!({"type": NOTE, "output_index": 9, "other": "b"});

    // Act
    let (events, edits) = render(vec![
        carrier(vec![unknown(first)]),
        carrier(vec![unknown(second)]),
        text("ok", Vec::new()),
    ]);

    // Assert
    assert!(payloads(&events, NOTE).is_empty(), "{:?}", names(&events));
    let removed = removed_paths(&edits);
    assert!(
        removed.contains(&"/output/9/note".to_owned()),
        "{removed:?}"
    );
    assert!(
        removed.contains(&"/output/9/other".to_owned()),
        "{removed:?}"
    );
    assert_eq!(withheld_count(), before + 1);
}

#[test]
#[serial(responses_unknown_member_withheld)]
fn an_event_held_for_an_item_the_render_never_opens_is_withheld_at_the_end() {
    // Arrange: a reasoning item the render never opens (no detail follows).
    let before = withheld_count();
    let chunks = vec![
        carrier(vec![added(0, json!({"type": "reasoning", "id": "rs_1"}))]),
        carrier(vec![unknown(
            json!({"type": NOTE, "output_index": 0, "note": "a"}),
        )]),
        text("ok", Vec::new()),
    ];

    // Act
    let (events, edits) = render(chunks);

    // Assert
    assert!(payloads(&events, NOTE).is_empty());
    assert_eq!(removed_paths(&edits), ["/output/0/note"]);
    assert_eq!(withheld_count(), before + 1);
}

#[test]
#[serial(responses_unknown_member_withheld)]
fn a_placed_event_counts_nothing() {
    // Arrange
    let before = withheld_count();
    let event = json!({"type": NOTE, "output_index": 3, "item_id": "msg_1"});

    // Act
    let (_, edits) = render(message_at_three(vec![unknown(event)]));

    // Assert
    assert!(removed_paths(&edits).is_empty(), "{:?}", edits.as_slice());
    assert_eq!(withheld_count(), before);
}

#[test]
fn members_of_a_known_event_merge_into_the_renders_event_with_render_members_winning() {
    // Arrange: the delta's own chunk carries its members, one of which
    // collides with a member the render writes.
    let chunks = vec![
        carrier(vec![added(3, json!({"type": "message", "id": "msg_1"}))]),
        message_item_id(
            text(
                "ok",
                vec![extras(
                    "response.output_text.delta",
                    Some(3),
                    json!({"logprobs": [], "content_index": 7}),
                )],
            ),
            "msg_1",
        ),
    ];

    // Act
    let (events, _) = render(chunks);

    // Assert
    let deltas = payloads(&events, "response.output_text.delta");
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["logprobs"], json!([]));
    assert_eq!(deltas[0]["content_index"], 0);
    assert_eq!(deltas[0]["output_index"], 0);
}

#[test]
fn carried_events_on_a_chunk_from_another_dialect_are_not_rendered() {
    // Arrange
    let mut foreign = carrier(vec![unknown(json!({"type": "response.rate_note"}))]);
    foreign.upstream_meta = None;

    // Act
    let (events, edits) = render(vec![foreign, text("ok", Vec::new())]);

    // Assert
    assert!(payloads(&events, "response.rate_note").is_empty());
    assert!(removed_paths(&edits).is_empty());
}

#[test]
fn the_completed_body_carries_the_upstream_top_level_members_render_keys_winning() {
    // Arrange
    let mut closing = finish();
    let mut envelope = ResponsesEnvelope::default();
    envelope.extras = members(json!({"service_tier": "default", "status": "bogus"}));
    closing.upstream_meta = Some(
        UpstreamMeta::from_responses_envelope(envelope)
            .with_wire_dialect(IngressDialect::Responses),
    );

    // Act
    let (events, _) = render(vec![text("ok", Vec::new()), closing]);

    // Assert
    let completed = payloads(&events, "response.completed");
    assert_eq!(completed[0]["response"]["service_tier"], "default");
    assert_eq!(completed[0]["response"]["status"], "completed");
    assert!(
        payloads(&events, "response.created")[0]["response"]
            .get("service_tier")
            .is_none()
    );
}

#[test]
fn reasoning_item_members_reach_the_items_own_events_and_the_completed_body() {
    // Arrange
    let mut added_members = routectl_core::ResponsesItemMembers::default();
    added_members
        .reasoning
        .insert("rs_1".into(), members(json!({"status": "in_progress"})));
    let mut done_members = routectl_core::ResponsesItemMembers::default();
    done_members.reasoning.insert(
        "rs_1".into(),
        members(json!({"status": "completed", "metadata": {"t": "caf\u{e9}"}})),
    );
    let with = |added: bool| {
        let mut envelope = ResponsesEnvelope::default();
        if added {
            envelope.added_item_members = added_members.clone();
        } else {
            envelope.item_members = done_members.clone();
        }
        let mut chunk = carrier(Vec::new());
        chunk.upstream_meta = Some(
            UpstreamMeta::from_responses_envelope(envelope)
                .with_wire_dialect(IngressDialect::Responses),
        );
        chunk
    };
    let detail = chunk(
        ChunkDelta {
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Summary,
                id: Some("rs_1".into()),
                format: Some(routectl_core::OPENAI_RESPONSES_V1.into()),
                index: Some(0),
                payload: json!({"text": "think"}),
            }],
            ..Default::default()
        },
        Vec::new(),
    );

    // Act
    let (events, _) = render(vec![
        with(true),
        detail,
        with(false),
        text("ok", Vec::new()),
    ]);

    // Assert
    let added = payloads(&events, "response.output_item.added");
    assert_eq!(added[0]["item"]["status"], "in_progress");
    let done = payloads(&events, "response.output_item.done");
    assert_eq!(done[0]["item"]["metadata"]["t"], "caf\u{e9}");
    let completed = &payloads(&events, "response.completed")[0]["response"]["output"][0];
    assert_eq!(completed["type"], "reasoning");
    assert_eq!(completed["status"], "completed");
}

#[test]
fn an_empty_reasoning_content_array_reaches_every_event_that_carries_the_item() {
    // Arrange
    let mut item_members = routectl_core::ResponsesItemMembers::default();
    item_members
        .reasoning
        .insert("rs_1".into(), members(json!({"content": []})));
    let with = |added: bool| {
        let mut envelope = ResponsesEnvelope::default();
        if added {
            envelope.added_item_members = item_members.clone();
        } else {
            envelope.item_members = item_members.clone();
        }
        let mut chunk = carrier(Vec::new());
        chunk.upstream_meta = Some(
            UpstreamMeta::from_responses_envelope(envelope)
                .with_wire_dialect(IngressDialect::Responses),
        );
        chunk
    };
    let detail = chunk(
        ChunkDelta {
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Summary,
                id: Some("rs_1".into()),
                format: Some(routectl_core::OPENAI_RESPONSES_V1.into()),
                index: Some(0),
                payload: json!({"text": "think"}),
            }],
            ..Default::default()
        },
        Vec::new(),
    );

    // Act
    let (events, _) = render(vec![
        with(true),
        detail,
        with(false),
        text("ok", Vec::new()),
    ]);

    // Assert
    let added = payloads(&events, "response.output_item.added");
    assert_eq!(added[0]["item"]["content"], json!([]), "{:?}", added[0]);
    let done = payloads(&events, "response.output_item.done");
    assert_eq!(done[0]["item"]["content"], json!([]), "{:?}", done[0]);
    let completed = &payloads(&events, "response.completed")[0]["response"]["output"][0];
    assert_eq!(completed["content"], json!([]), "{completed}");
}
