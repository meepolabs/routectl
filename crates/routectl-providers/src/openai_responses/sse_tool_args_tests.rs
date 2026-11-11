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

// ---------------------------------------------------------------------------
// Codex lane: a call to a tool whose empty optional arguments the filter can
// remove is held back and emitted once, filtered. The unit-test registry is
// per thread and every stream here is driven on the test's own thread, so
// each census delta is exactly what that test caused.
// ---------------------------------------------------------------------------

const READER: &str = "read_file";
const EMPTY_PAGES: &str = r#"{"allow_large":false,"file_path":"/w/sentinel-path-9e2a","limit":2000,"offset":0,"pages":""}"#;
const WITHOUT_PAGES: &str =
    r#"{"allow_large":false,"file_path":"/w/sentinel-path-9e2a","limit":2000,"offset":0}"#;
const DROP_CLASS: &str = "codex_empty_optional_dropped";

fn reader_tool(schema: Value) -> routectl_core::ToolDef {
    routectl_core::ToolDef::Custom(routectl_core::CustomTool {
        name: READER.into(),
        description: None,
        input_schema: schema,
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
        extras: serde_json::Map::new(),
    })
}

fn reader_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string"},
            "pages": {"type": "string"},
            "offset": {"type": "integer"},
            "limit": {"type": "integer"},
            "allow_large": {"type": "boolean"}
        },
        "required": ["file_path"]
    })
}

fn lane_state(
    auth_kind: AuthKind,
    tools: &[routectl_core::ToolDef],
    observation: Option<RequestObservation>,
) -> ResponsesStreamState {
    ResponsesStreamState::new(
        auth_kind,
        RequestProvenance::Library,
        observation,
        tools,
        None,
    )
}

fn drive(state: &mut ResponsesStreamState, events: Vec<Value>) -> Vec<ChatChunk> {
    events
        .into_iter()
        .flat_map(|ev| {
            let event: ResponsesStreamEvent = serde_json::from_value(ev).expect("event parse");
            state.parse_event("test", event).expect("event processing")
        })
        .collect()
}

/// `args` split into three deltas, so an unbuffered call streams three chunks.
fn args_deltas(output_index: u32, args: &str) -> Vec<Value> {
    let (head, rest) = args.split_at(10);
    let (mid, tail) = rest.split_at(20);
    [head, mid, tail]
        .into_iter()
        .map(|piece| args_delta(output_index, piece))
        .collect()
}

fn drop_census() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::super::LANE && e.drop_class == DROP_CLASS)
        .map_or(0, |e| e.drop_count)
}

#[test]
fn a_codex_call_to_a_filterable_tool_emits_its_filtered_arguments_once() {
    // (row, events)
    let rows: [(&str, Vec<Value>); 3] = [
        ("deltas only", {
            let mut events = vec![function_call_added(0, "call_a", READER)];
            events.extend(args_deltas(0, EMPTY_PAGES));
            events.extend([
                args_done(0, EMPTY_PAGES),
                function_call_item_done(0, "call_a", READER, EMPTY_PAGES),
                completed(),
            ]);
            events
        }),
        (
            "done event only",
            vec![
                function_call_added(0, "call_a", READER),
                function_call_item_done(0, "call_a", READER, EMPTY_PAGES),
                completed(),
            ],
        ),
        ("completion with no done event", {
            let mut events = vec![function_call_added(0, "call_a", READER)];
            events.extend(args_deltas(0, EMPTY_PAGES));
            events.push(completed());
            events
        }),
    ];
    let tools = [reader_tool(reader_schema())];
    for (row, events) in rows {
        // Arrange
        let mut state = lane_state(AuthKind::ChatgptOauth, &tools, None);

        // Act
        let chunks = drive(&mut state, events);

        // Assert
        assert_eq!(
            tool_deltas(&chunks),
            vec![(0, "call_a".into(), READER.into(), WITHOUT_PAGES.into())],
            "{row}"
        );
        assert_eq!(
            chunks.last().expect("terminal").choices[0]
                .finish_reason
                .as_deref(),
            Some("tool_calls"),
            "{row}"
        );
    }
}

#[test]
fn a_buffered_call_with_nothing_to_drop_keeps_its_arguments_byte_for_byte() {
    // Arrange: spacing a re-serialization would normalize away.
    let args = r#"{"file_path": "/w/a.txt",   "pages": "1-3", "limit": 10}"#;
    let mut state = lane_state(
        AuthKind::ChatgptOauth,
        &[reader_tool(reader_schema())],
        None,
    );
    let mut events = vec![function_call_added(0, "call_a", READER)];
    events.extend(args_deltas(0, args));
    events.push(function_call_item_done(0, "call_a", READER, args));

    // Act
    let chunks = drive(&mut state, events);

    // Assert
    assert_eq!(
        tool_deltas(&chunks),
        vec![(0, "call_a".into(), READER.into(), args.into())]
    );
}

#[test]
fn deltas_stream_unchanged_where_the_filter_does_not_apply() {
    // (row, lane, the called tool's schema)
    let rows = [
        ("api-key lane", AuthKind::ApiKey, reader_schema()),
        (
            "tool without properties",
            AuthKind::ChatgptOauth,
            json!({"type": "object"}),
        ),
        (
            "all properties required",
            AuthKind::ChatgptOauth,
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "pages": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                    "allow_large": {"type": "boolean"}
                },
                "required": ["file_path", "pages", "offset", "limit", "allow_large"]
            }),
        ),
    ];
    for (row, auth_kind, schema) in rows {
        // Arrange
        let mut state = lane_state(auth_kind, &[reader_tool(schema)], None);
        let deltas = args_deltas(0, EMPTY_PAGES);
        let pieces: Vec<String> = deltas
            .iter()
            .map(|d| d["delta"].as_str().expect("delta").to_string())
            .collect();
        drive(&mut state, vec![function_call_added(0, "call_a", READER)]);

        // Act: each delta is driven alone, so a chunk per delta is incremental.
        let per_delta: Vec<Vec<ChatChunk>> = deltas
            .into_iter()
            .map(|d| drive(&mut state, vec![d]))
            .collect();
        let at_done = drive(
            &mut state,
            vec![
                function_call_item_done(0, "call_a", READER, EMPTY_PAGES),
                completed(),
            ],
        );

        // Assert
        let streamed: Vec<String> = per_delta
            .iter()
            .map(|chunks| {
                let deltas = tool_deltas(chunks);
                assert_eq!(deltas.len(), 1, "{row}: one chunk per delta");
                deltas[0].3.clone()
            })
            .collect();
        assert_eq!(streamed, pieces, "{row}");
        assert!(tool_deltas(&at_done).is_empty(), "{row}: nothing re-sent");
    }
}

#[test]
fn only_a_call_to_a_filterable_tool_is_held_back_on_the_codex_lane() {
    // Arrange: a filterable reader and a schemaless tool in one response.
    let tools = [
        reader_tool(reader_schema()),
        routectl_core::ToolDef::Custom(routectl_core::CustomTool {
            name: "now".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
            extras: serde_json::Map::new(),
        }),
    ];
    let mut state = lane_state(AuthKind::ChatgptOauth, &tools, None);
    drive(
        &mut state,
        vec![
            function_call_added(0, "call_a", READER),
            function_call_added(1, "call_b", "now"),
        ],
    );

    // Act
    let reader_delta = drive(&mut state, vec![args_delta(0, "{\"file_path\":")]);
    let now_delta = drive(&mut state, vec![args_delta(1, "{\"tz\":")]);

    // Assert
    assert!(tool_deltas(&reader_delta).is_empty());
    assert_eq!(tool_deltas(&now_delta).len(), 1);
}

/// A terminal event whose body carries `output`, under `status`.
fn terminal_with(kind: &str, status: &str, output: Vec<Value>) -> Value {
    json!({
        "type": kind,
        "response": {"id": "r", "status": status, "model": "m", "output": output}
    })
}

fn body_call(call_id: &str, args: &str) -> Value {
    json!({"type": "function_call", "id": "fc_1", "call_id": call_id,
           "name": READER, "arguments": args, "status": "completed"})
}

#[test]
fn a_call_settled_only_by_the_terminal_body_reaches_the_client_once() {
    let completed_with =
        |output: Vec<Value>| terminal_with("response.completed", "completed", output);
    let call = |args: &str| (0, "call_a".into(), READER.into(), args.to_string());
    // (row, lane, events, expected tool-call deltas)
    type Row = (
        &'static str,
        AuthKind,
        Vec<Value>,
        Vec<(u64, String, String, String)>,
    );
    let rows: Vec<Row> = vec![
        (
            "buffered, arguments only in the completed body",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![body_call("call_a", EMPTY_PAGES)]),
            ],
            vec![call(WITHOUT_PAGES)],
        ),
        (
            "unbuffered, arguments only in the completed body",
            AuthKind::ApiKey,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![body_call("call_a", EMPTY_PAGES)]),
            ],
            vec![call(EMPTY_PAGES)],
        ),
        (
            "buffered, arguments only in the incomplete body",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                terminal_with(
                    "response.incomplete",
                    "incomplete",
                    vec![body_call("call_a", EMPTY_PAGES)],
                ),
            ],
            vec![call(WITHOUT_PAGES)],
        ),
        (
            "buffered, held deltas win over a differing completed body",
            AuthKind::ChatgptOauth,
            {
                let mut events = vec![function_call_added(0, "call_a", READER)];
                events.extend(args_deltas(0, EMPTY_PAGES));
                events.push(completed_with(vec![body_call(
                    "call_a",
                    r#"{"file_path":"/w/other"}"#,
                )]));
                events
            },
            vec![call(WITHOUT_PAGES)],
        ),
        (
            "deferred empty arguments take the completed body's arguments",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                args_done(0, ""),
                completed_with(vec![body_call("call_a", EMPTY_PAGES)]),
            ],
            vec![call(WITHOUT_PAGES)],
        ),
        (
            "empty body arguments settle as an empty object",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![body_call("call_a", "")]),
            ],
            vec![call("{}")],
        ),
        (
            "empty object body arguments pass through",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![body_call("call_a", "{}")]),
            ],
            vec![call("{}")],
        ),
        (
            "body arguments that are not an object pass through",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![body_call("call_a", r#"["",1]"#)]),
            ],
            vec![call(r#"["",1]"#)],
        ),
        (
            "body arguments that are not a string settle as an empty object",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                completed_with(vec![json!({"type": "function_call", "id": "fc_1",
                    "call_id": "call_a", "name": READER, "arguments": {"file_path": "/w"}})]),
            ],
            vec![call("{}")],
        ),
        (
            "a call settled by item.done is not re-sent from the body",
            AuthKind::ChatgptOauth,
            vec![
                function_call_added(0, "call_a", READER),
                function_call_item_done(0, "call_a", READER, EMPTY_PAGES),
                completed_with(vec![body_call("call_a", EMPTY_PAGES)]),
            ],
            vec![call(WITHOUT_PAGES)],
        ),
        (
            "calls settle in output order from their own body items",
            AuthKind::ApiKey,
            vec![
                function_call_added(1, "call_b", READER),
                function_call_added(0, "call_a", READER),
                completed_with(vec![
                    body_call("call_a", r#"{"file_path":"/w/a"}"#),
                    body_call("call_b", r#"{"file_path":"/w/b"}"#),
                ]),
            ],
            vec![
                (
                    1,
                    "call_a".into(),
                    READER.into(),
                    r#"{"file_path":"/w/a"}"#.into(),
                ),
                (
                    0,
                    "call_b".into(),
                    READER.into(),
                    r#"{"file_path":"/w/b"}"#.into(),
                ),
            ],
        ),
    ];
    let tools = [reader_tool(reader_schema())];
    for (row, auth_kind, events, expected) in rows {
        // Arrange
        let mut state = lane_state(auth_kind, &tools, None);

        // Act
        let chunks = drive(&mut state, events);

        // Assert
        assert_eq!(tool_deltas(&chunks), expected, "{row}");
        let terminal = chunks.last().expect("terminal chunk");
        assert!(
            terminal.choices[0].delta.tool_calls.is_none(),
            "{row}: the tool call precedes the terminal chunk",
        );
    }
}

#[test]
fn a_body_item_that_is_not_the_held_call_settles_nothing() {
    // (row, body item at the call's output_index)
    let rows = [
        (
            "a message item",
            json!({"type": "message", "id": "msg_1", "role": "assistant", "content": []}),
        ),
        ("a different call", body_call("call_other", EMPTY_PAGES)),
    ];
    let tools = [reader_tool(reader_schema())];
    for (row, item) in rows {
        // Arrange
        let mut state = lane_state(AuthKind::ChatgptOauth, &tools, None);
        let events = vec![
            function_call_added(0, "call_a", READER),
            terminal_with("response.completed", "completed", vec![item]),
        ];

        // Act
        let chunks = drive(&mut state, events);

        // Assert
        assert!(tool_deltas(&chunks).is_empty(), "{row}");
    }
}

#[test]
fn drops_across_one_stream_count_once_in_the_census_and_the_observation() {
    // Arrange: two calls, each with an empty optional.
    let slot = RequestObservation::new();
    let before = drop_census();
    let mut state = lane_state(
        AuthKind::ChatgptOauth,
        &[reader_tool(reader_schema())],
        Some(slot.clone()),
    );
    let events = vec![
        function_call_added(0, "call_a", READER),
        function_call_added(1, "call_b", READER),
        function_call_item_done(0, "call_a", READER, EMPTY_PAGES),
        function_call_item_done(1, "call_b", READER, EMPTY_PAGES),
        completed(),
    ];

    // Act
    let chunks = drive(&mut state, events);
    state.report_removed_empty_optionals("test");

    // Assert
    assert_eq!(tool_deltas(&chunks).len(), 2);
    assert_eq!(drop_census() - before, 1, "census");
    assert_eq!(slot.snapshot().translation_drops, 1, "observation");
}

// ---------------------------------------------------------------------------
// Through the provider's stream(), against a mock upstream.
// ---------------------------------------------------------------------------

mod through_the_provider {
    use std::sync::Arc;

    use futures::StreamExt;
    use routectl_core::wire_check::{EditKind, EditSite, IngressDialect, Leg, WireTap};
    use routectl_core::{
        ChatRequest, Message, MessageContent, Provider, RequestObservation, Role, StaticToken,
        TokenSource,
    };
    use serde_json::{Value, json};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        EMPTY_PAGES, READER, WITHOUT_PAGES, args_deltas, completed, drop_census,
        function_call_added, function_call_item_done, reader_schema, reader_tool, tool_deltas,
    };
    use crate::openai_responses::response::EMPTY_OPTIONAL_ARGUMENTS_PATH;
    use crate::openai_responses::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};

    fn provider(base_url: &str) -> OpenAiResponsesProvider {
        OpenAiResponsesProvider::new(OpenAiResponsesConfig {
            id: "responses-stream-empty-optional".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct".into()),
            base_url: base_url.to_string(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: Vec::new(),
            user_agent: None,
            session_id: None,
            installation_id: None,
            #[cfg(feature = "bedrock")]
            mantle: None,
        })
    }

    fn request(observation: &RequestObservation) -> ChatRequest {
        let mut req = ChatRequest {
            model: "gpt-test".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("read it".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extras: serde_json::Map::new(),
            }]
            .into(),
            tools: Some(vec![reader_tool(reader_schema())]),
            ..Default::default()
        };
        req.routectl_internal.observation = Some(observation.clone());
        req
    }

    fn sse(events: &[Value]) -> ResponseTemplate {
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }

    /// One call to the reader carrying an empty optional, streamed as deltas.
    fn streamed_call(terminal: bool) -> Vec<Value> {
        let mut events = vec![function_call_added(0, "call_a", READER)];
        events.extend(args_deltas(0, EMPTY_PAGES));
        events.push(function_call_item_done(0, "call_a", READER, EMPTY_PAGES));
        if terminal {
            events.push(completed());
        }
        events
    }

    async fn server_answering(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    /// Every item the stream yielded: chunks, then any error, which ends it.
    async fn drain(req: ChatRequest, events: &[Value]) -> (Vec<routectl_core::ChatChunk>, bool) {
        let server = server_answering(sse(events)).await;
        let mut stream = provider(&server.uri())
            .stream(req)
            .await
            .expect("stream opens");
        let mut chunks = Vec::new();
        let mut errored = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => chunks.push(chunk),
                Err(_) => errored = true,
            }
        }
        (chunks, errored)
    }

    #[tokio::test]
    async fn a_streamed_drop_reaches_the_client_filtered_and_counts_once() {
        // Arrange
        let slot = RequestObservation::new();
        let before = drop_census();

        // Act
        let (chunks, errored) = drain(request(&slot), &streamed_call(true)).await;

        // Assert
        assert!(!errored);
        assert_eq!(
            tool_deltas(&chunks),
            vec![(0, "call_a".into(), READER.into(), WITHOUT_PAGES.into())]
        );
        assert_eq!(drop_census() - before, 1, "census");
        assert_eq!(slot.snapshot().translation_drops, 1, "observation");
    }

    #[tokio::test]
    async fn a_stream_cut_after_a_drop_still_counts_it() {
        // Arrange
        let slot = RequestObservation::new();
        let before = drop_census();

        // Act
        let (chunks, errored) = drain(request(&slot), &streamed_call(false)).await;

        // Assert
        assert!(errored, "a stream with no terminal event ends in an error");
        assert_eq!(tool_deltas(&chunks).len(), 1);
        assert_eq!(drop_census() - before, 1, "census");
        assert_eq!(slot.snapshot().translation_drops, 1, "observation");
    }

    #[tokio::test]
    async fn stream_and_complete_drops_for_one_request_count_once() {
        // Arrange: one client request's slot, answered once streamed and
        // once complete (as a retry across both paths would be).
        let slot = RequestObservation::new();
        let before = drop_census();
        let body = json!({"id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
                          "status": "completed", "model": "gpt-test",
                          "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_a",
                                      "name": READER, "arguments": EMPTY_PAGES,
                                      "status": "completed"}],
                          "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}});
        let complete_server = server_answering(sse(&[
            json!({"type": "response.completed", "response": body}),
        ]))
        .await;

        // Act
        let (chunks, _) = drain(request(&slot), &streamed_call(true)).await;
        let resp = provider(&complete_server.uri())
            .complete(request(&slot))
            .await
            .expect("complete");

        // Assert: both paths dropped it, and it counted once.
        assert_eq!(tool_deltas(&chunks)[0].3, WITHOUT_PAGES);
        let complete_args =
            resp.choices[0].message.tool_calls.as_ref().expect("calls")[0]["function"]["arguments"]
                .as_str()
                .expect("arguments")
                .to_string();
        assert_eq!(complete_args, WITHOUT_PAGES);
        assert_eq!(drop_census() - before, 1, "census");
        assert_eq!(slot.snapshot().translation_drops, 1, "observation");
    }

    #[tokio::test]
    async fn a_streamed_drop_is_declared_only_on_a_compared_response_leg() {
        // (row, tap, drop declared)
        let rows = [
            (
                "fidelity tap",
                WireTap::checking_response(IngressDialect::Responses),
                true,
            ),
            ("translation tap", WireTap::admitted(), false),
        ];
        for (row, tap, declared) in rows {
            // Arrange
            let slot = RequestObservation::new();
            let mut req = request(&slot);
            req.routectl_internal.wire_tap = Some(tap.clone());

            // Act
            drain(req, &streamed_call(true)).await;

            // Assert
            let edits: Vec<_> = tap
                .take_response()
                .edits(Leg::Response)
                .iter()
                .filter(|e| e.site() == EditSite::DialectFieldStrip)
                .map(|e| (e.path().to_string(), e.kind()))
                .collect();
            let want = if declared {
                vec![(EMPTY_OPTIONAL_ARGUMENTS_PATH.to_string(), EditKind::Rewrite)]
            } else {
                Vec::new()
            };
            assert_eq!(edits, want, "{row}");
        }
    }
}
