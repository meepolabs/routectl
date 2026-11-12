//! Re-emission of carried Anthropic upstream members and events, through the
//! real stream render, against chunks shaped as the Anthropic-API egress
//! builds them.

use routectl_core::wire_check::EditKind;
use routectl_core::wire_check::IngressDialect;
use routectl_core::{ChatChunk, ChunkChoice, ChunkDelta, OpaqueSseEvent, UpstreamMeta, UsageDelta};
use routectl_providers::anthropic_api::sse::SseState;
use serde_json::{Map, Value, json};
use serial_test::serial;

use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::{IngressAdapter, ResponseEdits, SseEvent};

use super::super::AnthropicStreamState;
use super::super::stream::render_chunk_declared;
use super::{MESSAGE_DELTA_DELTA_KEYS, MESSAGE_DELTA_KEYS, MESSAGE_DELTA_USAGE_KEYS};

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

/// A chunk carrying `opaque`, stamped as parsed from an Anthropic body when
/// it carries anything, the way the Anthropic-API egress builds it.
fn carrier(opaque: Vec<OpaqueSseEvent>) -> ChatChunk {
    let upstream_meta = (!opaque.is_empty())
        .then(|| UpstreamMeta::default().with_wire_dialect(IngressDialect::Anthropic));
    ChatChunk {
        id: "msg_1".into(),
        model: "m".into(),
        opaque_events: opaque,
        upstream_meta,
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
            start.upstream_meta =
                Some(UpstreamMeta::default().with_wire_dialect(IngressDialect::Anthropic));
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
            json!({"delta": {"stop_details": {"type": "x"}, "container": {"id": "cntr_1"}},
                   "context_management": {"applied_edits": [{"n": 1}]}}),
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
    assert_eq!(
        delta["context_management"],
        json!({"applied_edits": [{"n": 1}]}),
        "top-level members stay at the top level"
    );
    assert!(
        delta["delta"].get("context_management").is_none(),
        "{delta}"
    );
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
            json!({"delta": {"stop_reason": "refusal", "stop_details": {}}}),
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

/// Render `chunks` after the warm-hold early frame already sent the
/// stream's `message_start`.
fn render_warm_held(chunks: Vec<ChatChunk>) -> (Vec<SseEvent>, ResponseEdits) {
    let mut state = AnthropicStreamState::default();
    let early = AnthropicIngress.early_frame(&mut state);
    assert_eq!(early.len(), 1, "the early frame is the message_start");
    let (events, edits) = render_into(&mut state, chunks);
    assert!(
        named(&events, "message_start").is_empty(),
        "no second message_start"
    );
    (events, edits)
}

fn removed_paths(edits: &ResponseEdits) -> Vec<&str> {
    edits
        .as_slice()
        .iter()
        .filter(|e| e.kind() == EditKind::Remove)
        .map(|e| e.path())
        .collect()
}

fn closing_delta(events: &[SseEvent]) -> Value {
    let deltas = named(events, "message_delta");
    assert_eq!(deltas.len(), 1, "{events:?}");
    payload(deltas[0])
}

#[test]
#[serial(anthropic_unknown_member_withheld)]
fn late_message_start_members_reach_the_client_on_the_closing_message_delta() {
    // Arrange
    let before = withheld_count();
    let chunks = vec![
        role(vec![extras(
            "message_start",
            None,
            json!({"container": {"id": "cntr_1"}, "diagnostics": {"cache": "miss"}}),
        )]),
        text("ok"),
        closing(Vec::new()),
    ];

    // Act
    let (events, edits) = render_warm_held(chunks);

    // Assert
    let delta = closing_delta(&events);
    assert_eq!(delta["container"], json!({"id": "cntr_1"}), "{delta}");
    assert_eq!(delta["diagnostics"], json!({"cache": "miss"}), "{delta}");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert!(removed_paths(&edits).is_empty(), "{:?}", edits.as_slice());
    assert_eq!(withheld_count(), before);
}

/// Upstream events through the Anthropic-API parser a same-dialect client
/// gets, one chunk per event that produced one.
fn parsed(events: &[Value]) -> Vec<ChatChunk> {
    let mut state = SseState::default().with_unmodeled_carriage();
    events
        .iter()
        .filter_map(|event| {
            state
                .parse_event("test", &event.to_string())
                .expect("event parses")
        })
        .collect()
}

/// The warm-hold shape of a live stream: placeholder nulls on the
/// `message_start`, the real values on the `message_delta`.
#[test]
#[serial(anthropic_unknown_member_withheld)]
fn a_warm_held_stream_gives_the_client_the_message_delta_values_over_start_nulls() {
    // Arrange
    let upstream = [
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0},
            "container": null, "context_management": null}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null,
                         "container": {"id": "cntr_1"}},
               "context_management": {"applied_edits": [{"n": 1}]},
               "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ];

    // Act
    let (events, edits) = render_warm_held(parsed(&upstream));

    // Assert
    let delta = closing_delta(&events);
    assert_eq!(
        delta["delta"]["container"],
        json!({"id": "cntr_1"}),
        "{delta}"
    );
    assert!(
        delta.get("container").is_none(),
        "a start null restated in delta is superseded: {delta}"
    );
    assert_eq!(
        delta["context_management"],
        json!({"applied_edits": [{"n": 1}]}),
        "{delta}"
    );
    assert!(removed_paths(&edits).is_empty(), "{:?}", edits.as_slice());
}

#[test]
#[serial(anthropic_unknown_member_withheld)]
fn the_message_delta_top_level_wins_over_a_late_start_member_and_the_render_over_both() {
    // Arrange
    let chunks = vec![
        role(vec![extras(
            "message_start",
            None,
            json!({"context_management": {"from": "start"}, "only_start": 1,
                   "stop_reason": "start"}),
        )]),
        text("ok"),
        closing(vec![extras(
            "message_delta",
            None,
            json!({"context_management": {"from": "delta"}, "usage": {"output_tokens": 99},
                   "delta": {"stop_reason": "refusal"}}),
        )]),
    ];

    // Act
    let (events, _) = render_warm_held(chunks);

    // Assert
    let delta = closing_delta(&events);
    assert_eq!(
        delta["context_management"],
        json!({"from": "delta"}),
        "{delta}"
    );
    assert_eq!(delta["only_start"], 1, "{delta}");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use", "{delta}");
    assert!(delta.get("stop_reason").is_none(), "{delta}");
    assert_eq!(delta["usage"]["output_tokens"], 1, "{delta}");
}

/// A carried top-level `type`, `delta` or `usage` never displaces the
/// render's own, including a `usage` the render's `message_delta` omits.
#[test]
fn the_render_message_delta_keys_win_over_carried_top_level_members() {
    // Arrange
    let carried = |late: Value| {
        vec![
            role(vec![extras("message_start", None, late)]),
            text("ok"),
            with_delta(
                ChunkDelta::default(),
                vec![extras(
                    "message_delta",
                    None,
                    json!({"type": "forged", "delta": "flat", "usage": {"output_tokens": 99},
                           "kept": true}),
                )],
            ),
            {
                let mut finish = with_delta(ChunkDelta::default(), Vec::new());
                finish.choices[0].finish_reason = Some("stop".into());
                finish
            },
        ]
    };
    let rows = [
        ("no late start members", json!({})),
        (
            "late start members of the same names",
            json!({"type": "forged", "delta": "flat", "usage": {"output_tokens": 99}}),
        ),
    ];
    for (row, late) in rows {
        // Act
        let (events, _) = render_warm_held(carried(late));

        // Assert
        let delta = closing_delta(&events);
        assert_eq!(delta["type"], "message_delta", "{row}: {delta}");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn", "{row}: {delta}");
        assert!(delta.get("usage").is_none(), "{row}: {delta}");
        assert_eq!(delta["kept"], true, "{row}: {delta}");
    }
}

#[test]
#[serial(anthropic_unknown_member_withheld)]
fn late_start_members_of_a_stream_with_no_message_delta_are_withheld_declared_and_counted() {
    // Arrange
    let before = withheld_count();
    let mut state = AnthropicStreamState::default();
    let _ = AnthropicIngress.early_frame(&mut state);
    let mut edits = ResponseEdits::recording();
    let chunk = role(vec![extras(
        "message_start",
        None,
        json!({"container": {"id": "cntr_1"}, "context_management": {}}),
    )]);

    // Act
    let mut events = render_chunk_declared(chunk, &mut state, &mut edits).expect("renders");
    events.extend(AnthropicIngress.render_eos_declared(&mut state, &mut edits));

    // Assert
    assert!(named(&events, "message_delta").is_empty(), "{events:?}");
    assert!(
        !events.iter().any(|e| e.data.contains("cntr_1")),
        "{events:?}"
    );
    assert_eq!(removed_paths(&edits), ["/container", "/context_management"]);
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
    assert!(removed_paths(&edits).is_empty(), "{:?}", edits.as_slice());
    assert_eq!(withheld_count(), before);
}

/// The reserved key lists are what `emit_message_delta` writes when every
/// optional member is present.
#[test]
fn the_message_delta_reserved_keys_are_what_the_render_writes() {
    // Arrange
    let usage = UsageDelta {
        prompt_tokens: Some(3),
        completion_tokens: Some(1),
        cache_creation_input_tokens: Some(1),
        cache_read_input_tokens: Some(1),
        cache_creation: Some(routectl_core::CacheCreation::from_lifetimes([(300, 1)])),
        server_tool_use: Some(json!({"web_search_requests": 1})),
        ..Default::default()
    };
    let mut events = Vec::new();

    // Act
    super::super::stream::emit_message_delta(
        Some("stop"),
        Some("END"),
        Some(&usage),
        None,
        &mut events,
        &mut ResponseEdits::default(),
    );

    // Assert
    let delta = payload(&events[0]);
    let keys = |value: &Value| -> std::collections::BTreeSet<String> {
        value.as_object().expect("object").keys().cloned().collect()
    };
    let reserved = |list: &[&str]| -> std::collections::BTreeSet<String> {
        list.iter().map(|k| (*k).to_owned()).collect()
    };
    assert_eq!(keys(&delta), reserved(MESSAGE_DELTA_KEYS));
    assert_eq!(keys(&delta["delta"]), reserved(MESSAGE_DELTA_DELTA_KEYS));
    assert_eq!(keys(&delta["usage"]), reserved(MESSAGE_DELTA_USAGE_KEYS));
}

/// A closing chunk whose usage carries `members`, stamped as parsed from
/// `dialect`, the way the Anthropic-API egress builds it.
fn closing_with_usage_members(carried: Value, dialect: Option<IngressDialect>) -> ChatChunk {
    let mut chunk = closing(Vec::new());
    let usage = chunk.usage.as_mut().expect("closing usage");
    usage.cache_creation_input_tokens = Some(2);
    usage.cache_creation = Some(routectl_core::CacheCreation::from_lifetimes([(300, 2)]));
    usage.extras = members(carried);
    chunk.upstream_meta = dialect.map(|d| UpstreamMeta::default().with_wire_dialect(d));
    chunk
}

/// The closing usage of an Anthropic-parsed stream takes its carried
/// members after the typed ones, on the inline and the deferred close
/// alike: a carried `cache_creation`, cache-lifetime member or token
/// count never replaces or adds to what the render writes.
#[test]
fn message_delta_usage_takes_carried_members_under_the_typed_ones() {
    // Arrange
    let carried = json!({
        "output_tokens_details": {"thinking_tokens": 0},
        "speed": "standard",
        "cache_creation": {"ephemeral_1h_input_tokens": 99},
        "ephemeral_5m_input_tokens": 99,
        "output_tokens": 99,
    });
    let deferred = || {
        let mut finish = closing(Vec::new());
        finish.usage = None;
        let mut usage_only =
            closing_with_usage_members(carried.clone(), Some(IngressDialect::Anthropic));
        usage_only.choices.clear();
        vec![role(Vec::new()), finish, usage_only]
    };
    let untyped_cache = || {
        let mut chunk =
            closing_with_usage_members(carried.clone(), Some(IngressDialect::Anthropic));
        let usage = chunk.usage.as_mut().expect("closing usage");
        usage.cache_creation_input_tokens = None;
        usage.cache_creation = None;
        vec![role(Vec::new()), chunk]
    };
    let typed_split = json!({"ephemeral_5m_input_tokens": 2});
    let rows: [(&str, Vec<ChatChunk>, Option<&Value>); 3] = [
        (
            "inline",
            vec![
                role(Vec::new()),
                closing_with_usage_members(carried.clone(), Some(IngressDialect::Anthropic)),
            ],
            Some(&typed_split),
        ),
        ("deferred", deferred(), Some(&typed_split)),
        ("typed cache absent", untyped_cache(), None),
    ];

    for (name, chunks, expected_split) in rows {
        // Act
        let (events, _) = render(chunks);

        // Assert
        let deltas = named(&events, "message_delta");
        assert_eq!(deltas.len(), 1, "{name}");
        let usage = &payload(deltas[0])["usage"];
        assert_eq!(
            usage["output_tokens_details"],
            json!({"thinking_tokens": 0}),
            "{name}: {usage}"
        );
        assert_eq!(usage["speed"], "standard", "{name}");
        assert_eq!(
            usage.get("cache_creation"),
            expected_split,
            "{name}: the typed cache_creation, or none, never the carried one"
        );
        assert!(
            usage.get("ephemeral_5m_input_tokens").is_none(),
            "{name}: a lifetime member stays inside cache_creation: {usage}"
        );
        assert_eq!(usage["output_tokens"], 1, "{name}: typed count wins");
    }
}

/// Usage members carried on a chunk not parsed from an Anthropic body never
/// reach the closing `message_delta`. Positive control: the stamped chunk's
/// do.
#[test]
fn message_delta_usage_carries_members_only_from_an_anthropic_body() {
    let rows = [
        ("unstamped", None, false),
        ("chat body", Some(IngressDialect::Chat), false),
        ("responses body", Some(IngressDialect::Responses), false),
        ("anthropic body", Some(IngressDialect::Anthropic), true),
    ];
    for (name, dialect, expected) in rows {
        // Arrange
        let chunk = closing_with_usage_members(json!({"sentinel_usage": 1}), dialect);

        // Act
        let (events, _) = render(vec![role(Vec::new()), chunk]);

        // Assert
        let usage = &payload(named(&events, "message_delta")[0])["usage"];
        assert_eq!(
            usage.get("sentinel_usage").is_some(),
            expected,
            "{name}: {usage}"
        );
    }
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

/// Carried events on a chunk not parsed from an Anthropic body never reach
/// an Anthropic client: no unknown event, no unknown block delta, no
/// message_start member. Positive control: the stamped chunk's do.
#[test]
fn carried_events_render_only_from_an_anthropic_body() {
    let rows = [
        ("unstamped", None, false),
        ("responses body", Some(IngressDialect::Responses), false),
        ("chat body", Some(IngressDialect::Chat), false),
        ("anthropic body", Some(IngressDialect::Anthropic), true),
    ];
    for (name, dialect, expected) in rows {
        // Arrange
        let stamp = |mut chunk: ChatChunk| {
            chunk.upstream_meta = dialect.map(|d| UpstreamMeta::default().with_wire_dialect(d));
            chunk
        };
        let chunks = vec![
            stamp(role(vec![extras(
                "message_start",
                None,
                json!({"zz_start_member": 1}),
            )])),
            text("ok"),
            stamp(carrier(vec![
                OpaqueSseEvent::UnknownEvent {
                    event_type: "zz_future_event".into(),
                    event: json!({"type": "zz_future_event"}),
                },
                OpaqueSseEvent::UnknownBlockDelta {
                    upstream_index: 0,
                    delta: json!({"type": "zz_future_delta"}),
                },
            ])),
        ];

        // Act
        let (events, _) = render(chunks);

        // Assert
        let wire: String = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(wire.contains("zz_start_member"), expected, "{name}: {wire}");
        assert_eq!(wire.contains("zz_future_event"), expected, "{name}: {wire}");
        if !expected {
            assert!(!wire.contains("zz_future_delta"), "{name}: {wire}");
        }
    }
}
