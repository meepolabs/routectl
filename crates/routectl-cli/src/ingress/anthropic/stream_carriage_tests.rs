//! Re-emission of carried Anthropic upstream members and events, through the
//! real stream render, against chunks shaped as the Anthropic-API egress
//! builds them.

use routectl_core::wire_check::EditKind;
use routectl_core::{ChatChunk, ChunkChoice, ChunkDelta, OpaqueSseEvent, UsageDelta};
use serde_json::{Map, Value, json};
use serial_test::serial;

use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::{IngressAdapter, ResponseEdits, SseEvent};

use super::super::AnthropicStreamState;
use super::super::stream::render_chunk_declared;

fn members(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn extras(event_type: &str, index: Option<u32>, value: Value) -> OpaqueSseEvent {
    OpaqueSseEvent::KnownEventExtras {
        event_type: event_type.into(),
        upstream_index: index,
        members: members(value),
    }
}

fn carrier(opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    ChatChunk {
        id: "msg_1".into(),
        model: "m".into(),
        opaque_events: opaque,
        ..Default::default()
    }
}

fn with_delta(delta: ChunkDelta, opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..carrier(opaque)
    }
}

fn role(opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    with_delta(
        ChunkDelta {
            role: Some(routectl_core::Role::Assistant),
            ..Default::default()
        },
        opaque,
    )
}

fn text(t: &str) -> ChatChunk {
    with_delta(
        ChunkDelta {
            content: Some(t.into()),
            ..Default::default()
        },
        Vec::new(),
    )
}

fn tool(args: &str) -> ChatChunk {
    with_delta(
        ChunkDelta {
            tool_calls: Some(vec![
                json!({"index": 0, "id": "toolu_1", "type": "function",
                                         "function": {"name": "lookup", "arguments": args}}),
            ]),
            ..Default::default()
        },
        Vec::new(),
    )
}

fn closing(opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(UsageDelta {
            prompt_tokens: Some(3),
            completion_tokens: Some(1),
            ..Default::default()
        }),
        ..carrier(opaque)
    }
}

/// The egress's chunks for a tool-use stream carrying every kind of member
/// and event, one chunk per upstream event, in upstream order.
fn carried_tool_stream() -> Vec<ChatChunk> {
    vec![
        role(vec![extras(
            "message_start",
            None,
            json!({"container": {"id": "cntr_1"}, "context_management": {"applied_edits": []}}),
        )]),
        {
            let mut start = tool("");
            start.opaque_events = vec![extras(
                "content_block_start",
                Some(0),
                json!({"type": "tool_use", "caller": {"type": "direct"}}),
            )];
            start
        },
        tool("{}"),
        carrier(vec![OpaqueSseEvent::UnknownBlockDelta {
            upstream_index: 0,
            delta: json!({"type": "future_delta", "x": "\u{e9}"}),
        }]),
        carrier(vec![extras("content_block_stop", Some(0), json!({}))]),
        carrier(vec![OpaqueSseEvent::UnknownEvent {
            event_type: "future_event".into(),
            event: json!({"type": "future_event", "payload": {"n": 1}}),
        }]),
        closing(vec![extras(
            "message_delta",
            None,
            json!({"stop_details": {"type": "x"}, "container": {"id": "cntr_1"}}),
        )]),
    ]
}

fn render(chunks: Vec<ChatChunk>) -> (Vec<SseEvent>, ResponseEdits) {
    let mut state = AnthropicStreamState::default();
    render_into(&mut state, chunks)
}

fn render_into(
    state: &mut AnthropicStreamState,
    chunks: Vec<ChatChunk>,
) -> (Vec<SseEvent>, ResponseEdits) {
    let mut edits = ResponseEdits::recording();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(render_chunk_declared(chunk, state, &mut edits).expect("renders"));
    }
    events.extend(AnthropicIngress.render_eos(state));
    (events, edits)
}

fn payload(event: &SseEvent) -> Value {
    serde_json::from_str(&event.data).expect("event data is JSON")
}

fn named<'a>(events: &'a [SseEvent], name: &str) -> Vec<&'a SseEvent> {
    events
        .iter()
        .filter(|e| e.event.as_deref() == Some(name))
        .collect()
}

#[test]
fn message_start_and_message_delta_take_their_carried_members() {
    // Arrange + Act
    let (events, _) = render(carried_tool_stream());

    // Assert
    let start = payload(named(&events, "message_start")[0]);
    assert_eq!(start["message"]["container"]["id"], "cntr_1");
    assert_eq!(
        start["message"]["context_management"]["applied_edits"],
        json!([])
    );
    let delta = payload(named(&events, "message_delta")[0]);
    assert_eq!(delta["delta"]["stop_details"]["type"], "x");
    assert_eq!(delta["delta"]["container"]["id"], "cntr_1");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}

#[test]
fn a_tool_use_start_takes_its_members_and_its_unknown_delta_follows_the_input() {
    // Arrange + Act
    let (events, _) = render(carried_tool_stream());

    // Assert
    let start = payload(named(&events, "content_block_start")[0]);
    assert_eq!(start["content_block"]["caller"]["type"], "direct");
    assert_eq!(start["content_block"]["id"], "toolu_1");
    let deltas: Vec<Value> = named(&events, "content_block_delta")
        .into_iter()
        .map(payload)
        .collect();
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    assert_eq!(deltas[0]["delta"]["type"], "input_json_delta");
    assert_eq!(
        deltas[1]["delta"],
        json!({"type": "future_delta", "x": "\u{e9}"})
    );
    assert_eq!(deltas[1]["index"], start["index"], "the render's own index");
}

#[test]
fn an_unknown_event_is_emitted_under_its_own_name_in_order() {
    // Arrange + Act
    let (events, _) = render(carried_tool_stream());

    // Assert
    let order: Vec<&str> = events.iter().filter_map(|e| e.event.as_deref()).collect();
    assert_eq!(
        order,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "future_event",
            "message_delta",
            "message_stop",
        ]
    );
    assert_eq!(
        payload(named(&events, "future_event")[0])["payload"]["n"],
        1
    );
}

#[test]
fn a_text_block_start_and_its_unknown_delta_ride_in_upstream_order() {
    // Arrange
    let chunks = vec![
        role(Vec::new()),
        carrier(vec![extras(
            "content_block_start",
            Some(0),
            json!({"type": "text", "citations": []}),
        )]),
        carrier(vec![OpaqueSseEvent::UnknownBlockDelta {
            upstream_index: 0,
            delta: json!({"type": "citations_delta", "citation": {"n": 1}}),
        }]),
        text("ok"),
        carrier(vec![extras("content_block_stop", Some(0), json!({}))]),
        carrier(vec![OpaqueSseEvent::UnknownEvent {
            event_type: "future_event".into(),
            event: json!({"type": "future_event"}),
        }]),
        closing(Vec::new()),
    ];

    // Act
    let (events, _) = render(chunks);

    // Assert
    let order: Vec<String> = events
        .iter()
        .map(|e| {
            let data = payload(e);
            let delta_type = data["delta"]["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            format!("{}:{delta_type}", e.event.as_deref().unwrap_or_default())
        })
        .collect();
    assert_eq!(
        order,
        [
            "message_start:",
            "content_block_start:",
            "content_block_delta:citations_delta",
            "content_block_delta:text_delta",
            "content_block_stop:",
            "future_event:",
            "message_delta:",
            "message_stop:",
        ]
    );
    let start = payload(named(&events, "content_block_start")[0]);
    assert_eq!(start["content_block"]["citations"], json!([]));
}

#[test]
fn a_render_member_wins_over_a_carried_member_of_the_same_name() {
    // Arrange
    let chunks = vec![
        role(vec![extras(
            "message_start",
            None,
            json!({"role": "user", "container": {"id": "c"}}),
        )]),
        text("ok"),
        closing(vec![extras(
            "message_delta",
            None,
            json!({"stop_reason": "refusal", "stop_details": {}}),
        )]),
    ];

    // Act
    let (events, _) = render(chunks);

    // Assert
    let start = payload(named(&events, "message_start")[0]);
    assert_eq!(start["message"]["role"], "assistant");
    let delta = payload(named(&events, "message_delta")[0]);
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(delta["delta"]["stop_details"], json!({}));
}

#[test]
#[serial(anthropic_unknown_member_withheld)]
fn message_start_members_after_the_early_frame_are_withheld_declared_and_counted() {
    // Arrange
    let before = withheld_count();
    let mut state = AnthropicStreamState::default();
    let early = AnthropicIngress.early_frame(&mut state);
    let chunks = vec![
        role(vec![extras(
            "message_start",
            None,
            json!({"container": {"id": "cntr_1"}, "context_management": {}}),
        )]),
        text("ok"),
    ];

    // Act
    let (events, edits) = render_into(&mut state, chunks);

    // Assert
    assert_eq!(early.len(), 1, "the early frame is the message_start");
    assert!(
        named(&events, "message_start").is_empty(),
        "no second message_start"
    );
    assert!(!early[0].data.contains("container"), "{}", early[0].data);
    let removed: Vec<&str> = edits
        .as_slice()
        .iter()
        .filter(|e| e.kind() == EditKind::Remove)
        .map(|e| e.path())
        .collect();
    assert_eq!(removed, ["/container", "/context_management"]);
    assert_eq!(withheld_count(), before + 1);
}

#[test]
#[serial(anthropic_unknown_member_withheld)]
fn a_stream_with_no_start_members_after_the_early_frame_counts_nothing() {
    // Arrange
    let before = withheld_count();
    let mut state = AnthropicStreamState::default();
    let _ = AnthropicIngress.early_frame(&mut state);

    // Act
    let (_, edits) = render_into(&mut state, vec![role(Vec::new()), text("ok")]);

    // Assert
    assert!(
        edits
            .as_slice()
            .iter()
            .all(|e| e.kind() != EditKind::Remove),
        "{:?}",
        edits.as_slice()
    );
    assert_eq!(withheld_count(), before);
}

#[test]
fn a_carried_chunk_after_message_stop_is_dropped_like_any_straggler() {
    // Arrange
    let mut chunks = carried_tool_stream();
    chunks.push(carrier(vec![OpaqueSseEvent::UnknownEvent {
        event_type: "late_event".into(),
        event: json!({"type": "late_event"}),
    }]));

    // Act
    let (events, _) = render(chunks);

    // Assert
    assert!(named(&events, "late_event").is_empty());
    assert_eq!(
        events.last().and_then(|e| e.event.as_deref()),
        Some("message_stop")
    );
}

#[test]
fn an_unknown_event_name_that_is_not_a_token_is_not_emitted() {
    // Arrange
    let chunks = vec![
        role(Vec::new()),
        carrier(vec![OpaqueSseEvent::UnknownEvent {
            event_type: "bad\nname".into(),
            event: json!({"type": "bad\nname"}),
        }]),
        text("ok"),
    ];

    // Act
    let (events, _) = render(chunks);

    // Assert
    assert!(events.iter().all(|e| !e.data.contains("bad")), "{events:?}");
}

fn withheld_count() -> u64 {
    routectl_providers::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "anthropic" && e.drop_class == "unknown_member_withheld")
        .map_or(0, |e| e.drop_count)
}
