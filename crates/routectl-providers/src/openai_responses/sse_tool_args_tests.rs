//! Tool-call argument emission across the three wire shapes the
//! Responses API uses for a `function_call` item: argument deltas, the
//! final arguments carried only by `function_call_arguments.done`, and
//! the final arguments carried only by the item on `output_item.done`.

use super::*;
use serde_json::json;
use tracing_test::traced_test;

const ARGS_DONE_ONLY_FIXTURE: &str =
    include_str!("../../tests/fixtures/openai_responses/args_done_only_parallel_calls.jsonl");

fn drive_all(events: impl IntoIterator<Item = Value>) -> Vec<ChatChunk> {
    let mut state = ResponsesStreamState::default();
    events
        .into_iter()
        .flat_map(|ev| {
            let event: ResponsesStreamEvent = serde_json::from_value(ev).expect("event parse");
            state.parse_event("test", event).expect("event processing")
        })
        .collect()
}

/// Every emitted tool-call delta as `(index, id, name, arguments)`.
fn tool_deltas(chunks: &[ChatChunk]) -> Vec<(u64, String, String, String)> {
    chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.tool_calls.as_ref())
        .flatten()
        .map(|tc| {
            let text = |v: &Value| v.as_str().expect("string field").to_string();
            (
                tc["index"].as_u64().expect("index"),
                text(&tc["id"]),
                text(&tc["function"]["name"]),
                text(&tc["function"]["arguments"]),
            )
        })
        .collect()
}

fn function_call_added(output_index: u32, call_id: &str, name: &str) -> Value {
    json!({
        "type": "response.output_item.added",
        "output_index": output_index,
        "item": {"type": "function_call", "id": "fc_1", "call_id": call_id,
                 "name": name, "arguments": ""}
    })
}

fn function_call_item_done(output_index: u32, call_id: &str, name: &str, args: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "output_index": output_index,
        "item": {"type": "function_call", "id": "fc_1", "call_id": call_id,
                 "name": name, "arguments": args, "status": "completed"}
    })
}

fn args_done(output_index: u32, args: &str) -> Value {
    json!({
        "type": "response.function_call_arguments.done",
        "output_index": output_index,
        "arguments": args
    })
}

fn args_delta(output_index: u32, delta: &str) -> Value {
    json!({
        "type": "response.function_call_arguments.delta",
        "output_index": output_index,
        "delta": delta
    })
}

fn completed() -> Value {
    json!({
        "type": "response.completed",
        "response": {"id": "r", "status": "completed", "model": "m", "output": []}
    })
}

#[test]
fn captured_args_done_only_stream_emits_every_tool_call_once_with_full_arguments() {
    // Arrange
    let events: Vec<Value> = ARGS_DONE_ONLY_FIXTURE
        .lines()
        .map(|line| serde_json::from_str(line).expect("fixture line"))
        .collect();
    let expected: Vec<(u64, String, String, String)> = events
        .iter()
        .filter(|ev| {
            ev["type"] == "response.output_item.done" && ev["item"]["type"] == "function_call"
        })
        .zip(0_u64..)
        .map(|(ev, index)| {
            let item = &ev["item"];
            let text = |k: &str| item[k].as_str().expect("item field").to_string();
            (index, text("call_id"), text("name"), text("arguments"))
        })
        .collect();

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(expected.len(), 5, "fixture carries five function calls");
    assert!(expected.iter().all(|(_, _, _, args)| args.starts_with('{')));
    assert_eq!(tool_deltas(&chunks), expected);
    assert_eq!(
        chunks.last().expect("terminal").choices[0]
            .finish_reason
            .as_deref(),
        Some("tool_calls")
    );
}

#[test]
fn args_done_alone_emits_tool_call_without_waiting_for_item_done() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "lookup"),
        args_done(0, "{\"q\":\"x\"}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "lookup".into(), "{\"q\":\"x\"}".into())]
    );
}

#[test]
fn item_done_alone_emits_tool_call_when_no_arguments_event_arrives() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "lookup"),
        function_call_item_done(0, "call_a", "lookup", "{\"q\":\"x\"}"),
        completed(),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "lookup".into(), "{\"q\":\"x\"}".into())]
    );
}

#[test]
fn deltas_followed_by_matching_done_events_emit_arguments_once() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "calc"),
        args_delta(0, "{\"x\":"),
        args_delta(0, "1}"),
        args_done(0, "{\"x\":1}"),
        function_call_item_done(0, "call_a", "calc", "{\"x\":1}"),
        completed(),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    let args: Vec<String> = tool_deltas(&chunks)
        .into_iter()
        .map(|(_, _, _, a)| a)
        .collect();
    assert_eq!(args, vec!["{\"x\":".to_string(), "1}".to_string()]);
}

#[test]
fn streamed_deltas_win_over_a_differing_done_payload() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "calc"),
        args_delta(0, "{\"x\":1}"),
        args_done(0, "{\"x\":2}"),
        function_call_item_done(0, "call_a", "calc", "{\"x\":2}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    let args: Vec<String> = tool_deltas(&chunks)
        .into_iter()
        .map(|(_, _, _, a)| a)
        .collect();
    assert_eq!(args, vec!["{\"x\":1}".to_string()]);
}

#[test]
fn args_done_then_item_done_emits_arguments_once() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "calc"),
        args_done(0, "{\"x\":1}"),
        function_call_item_done(0, "call_a", "calc", "{\"x\":1}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "calc".into(), "{\"x\":1}".into())]
    );
}

#[test]
fn empty_final_arguments_emit_a_zero_argument_tool_call() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "now"),
        args_done(0, ""),
        function_call_item_done(0, "call_a", "now", ""),
        completed(),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "now".into(), "{}".into())]
    );
}

#[test]
fn item_done_without_arguments_field_emits_a_zero_argument_tool_call() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "now"),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_a",
                     "name": "now", "status": "completed"}
        }),
        completed(),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "now".into(), "{}".into())]
    );
}

#[test]
fn delta_arriving_after_args_done_is_dropped() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "calc"),
        args_done(0, "{\"x\":1}"),
        args_delta(0, "{\"x\":1}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "calc".into(), "{\"x\":1}".into())]
    );
}

#[test]
fn done_events_route_only_to_the_tool_block_at_their_output_index() {
    // Arrange
    let events = [
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}
        }),
        function_call_added(1, "call_b", "calc"),
        args_done(0, "{\"text\":0}"),
        args_done(1, "{\"x\":1}"),
        args_done(7, "{\"orphan\":7}"),
        function_call_item_done(7, "call_z", "calc", "{\"orphan\":7}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_b".into(), "calc".into(), "{\"x\":1}".into())]
    );
}

fn function_call_item_done_raw(output_index: u32, call_id: &str, item: Value) -> Value {
    let mut item = item;
    let obj = item.as_object_mut().expect("item is an object");
    obj.insert("type".into(), json!("function_call"));
    obj.insert("id".into(), json!("fc_1"));
    obj.insert("call_id".into(), json!(call_id));
    obj.insert("status".into(), json!("completed"));
    json!({
        "type": "response.output_item.done",
        "output_index": output_index,
        "item": item
    })
}

#[test]
fn late_delta_after_args_done_keeps_the_streamed_arguments() {
    // Arrange: bytes already streamed, so args.done takes the keep-streamed
    // branch; a delta arriving after it must not extend the emitted JSON.
    let events = [
        function_call_added(0, "call_a", "calc"),
        args_delta(0, "{\"x\":1}"),
        args_done(0, "{\"x\":1}"),
        args_delta(0, ",\"y\":2}"),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    let args: Vec<String> = tool_deltas(&chunks)
        .into_iter()
        .map(|(_, _, _, a)| a)
        .collect();
    assert_eq!(args, vec!["{\"x\":1}".to_string()]);
}

#[test]
fn empty_args_done_defers_to_item_done_arguments() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "clock"),
        args_done(0, ""),
        function_call_item_done(0, "call_a", "clock", "{\"tz\":\"utc\"}"),
        completed(),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(
            0,
            "call_a".into(),
            "clock".into(),
            "{\"tz\":\"utc\"}".into()
        )]
    );
}

/// An empty `function_call_arguments.done` defers to `output_item.done`;
/// when the stream completes without that event the call still reaches the
/// client, as `{}`, once, before the terminal chunk.
#[test]
fn a_deferred_zero_argument_call_is_settled_once_by_completion() {
    type Row = (&'static str, Vec<Value>, Vec<(u64, String, String, String)>);
    let zero_arg = |index: u64, call_id: &str| (index, call_id.into(), "now".into(), "{}".into());
    let rows: [Row; 3] = [
        (
            "completed without item.done",
            vec![
                function_call_added(0, "call_a", "now"),
                args_done(0, ""),
                completed(),
            ],
            vec![zero_arg(0, "call_a")],
        ),
        (
            "item.done arrives before completed",
            vec![
                function_call_added(0, "call_a", "now"),
                args_done(0, ""),
                function_call_item_done(0, "call_a", "now", ""),
                completed(),
            ],
            vec![zero_arg(0, "call_a")],
        ),
        (
            "two deferred calls settle in output order",
            vec![
                function_call_added(3, "call_b", "now"),
                function_call_added(1, "call_a", "now"),
                args_done(3, ""),
                args_done(1, ""),
                completed(),
            ],
            vec![zero_arg(1, "call_a"), zero_arg(0, "call_b")],
        ),
    ];

    for (name, events, expected) in rows {
        // Act
        let chunks = drive_all(events);

        // Assert
        assert_eq!(tool_deltas(&chunks), expected, "{name}");
        let terminal = chunks.last().expect("terminal chunk");
        assert!(
            terminal.choices[0].delta.tool_calls.is_none(),
            "{name}: the tool call precedes the terminal chunk",
        );
        assert_eq!(
            terminal.choices[0].finish_reason.as_deref(),
            Some("tool_calls"),
            "{name}",
        );
    }
}

#[traced_test]
#[test]
fn item_done_without_arguments_field_logs_before_emitting_empty_object() {
    // Arrange
    let events = [
        function_call_added(0, "call_a", "now"),
        function_call_item_done_raw(0, "call_a", json!({"name": "now"})),
    ];

    // Act
    let chunks = drive_all(events);

    // Assert
    assert!(logs_contain("function_call item.done without arguments"));
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), "now".into(), "{}".into())]
    );
}

#[traced_test]
#[test]
fn item_done_with_non_string_arguments_logs_before_emitting_empty_object() {
    let rows = [
        ("null", json!(null)),
        ("number", json!(7)),
        ("object", json!({"tz": "utc"})),
    ];
    for (kind, arguments) in rows {
        // Arrange
        let events = [
            function_call_added(0, "call_a", "now"),
            function_call_item_done_raw(
                0,
                "call_a",
                json!({"name": "now", "arguments": arguments}),
            ),
        ];

        // Act
        let chunks = drive_all(events);

        // Assert
        assert!(
            logs_contain(&format!("arguments_type=\"{kind}\"")),
            "row {kind}: the non-string fallback must log its argument type"
        );
        assert_eq!(
            tool_deltas(&chunks),
            vec![(0, "call_a".into(), "now".into(), "{}".into())],
            "row {kind}"
        );
    }
    assert!(logs_contain(
        "function_call item.done arguments are not a string"
    ));
}
