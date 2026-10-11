use serde_json::{Value, json};

use super::{Assembled, IncompleteReason, SseFrame, StreamAssembler, StreamExit};
use crate::wire_check::lane::IngressDialect;

const ANTHROPIC_STREAM: &str =
    include_str!("../../../routectl-providers/tests/fixtures/anthropic_api/sse_stream.txt");
const CHAT_STREAM: &str = include_str!(
    "../../../routectl-providers/tests/fixtures/openai_compat/deepseek_sse_stream.txt"
);
const RESPONSES_STREAM: &str = include_str!(
    "../../../routectl-providers/tests/fixtures/openai_responses/args_done_only_parallel_calls.jsonl"
);

/// One `(event, data)` pair per SSE block of a captured stream body.
fn sse_frames(raw: &str) -> Vec<(Option<String>, String)> {
    raw.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(name) = line.strip_prefix("event: ") {
                    event = Some(name.to_owned());
                } else if let Some(payload) = line.strip_prefix("data: ") {
                    data.push(payload);
                }
            }
            (event, data.join("\n"))
        })
        .collect()
}

/// One data-only frame per line of a captured event log.
fn line_frames(raw: &str) -> Vec<(Option<String>, String)> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| (None, line.to_owned()))
        .collect()
}

fn json_frames(frames: &[Value]) -> Vec<(Option<String>, String)> {
    frames
        .iter()
        .map(|frame| {
            let event = frame.get("type").and_then(Value::as_str).map(str::to_owned);
            (event, frame.to_string())
        })
        .collect()
}

fn feed(assembler: &mut StreamAssembler, frames: &[(Option<String>, String)]) {
    for (event, data) in frames {
        assembler.push(SseFrame::new(event.as_deref(), data));
    }
}

fn assemble(
    dialect: IngressDialect,
    frames: &[(Option<String>, String)],
    exit: StreamExit,
) -> Assembled {
    let mut assembler = StreamAssembler::new(dialect);
    feed(&mut assembler, frames);
    assembler.finish(exit)
}

fn complete(dialect: IngressDialect, frames: &[(Option<String>, String)]) -> Value {
    match assemble(dialect, frames, StreamExit::Ended) {
        Assembled::Complete(document) => document,
        Assembled::Incomplete(reason) => panic!("expected a complete document, got {reason:?}"),
    }
}

fn anthropic_tool_frames() -> Vec<(Option<String>, String)> {
    json_frames(&[
        json!({"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","model":"claude-3-opus","usage":{"input_tokens":20,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Looking "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"it up."}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"search","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ])
}

fn chat_tool_frames() -> Vec<(Option<String>, String)> {
    let chunk = |delta: Value, finish: Value| {
        json!({"id":"chatcmpl-1","object":"chat.completion.chunk","created":1_700_000_000,"model":"gpt-4o",
               "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    };
    let mut frames = json_frames(&[
        chunk(json!({"role":"assistant","content":""}), Value::Null),
        chunk(json!({"content":"Let me "}), Value::Null),
        chunk(json!({"content":"check."}), Value::Null),
        chunk(
            json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"calc","arguments":""}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        json!({"id":"chatcmpl-1","object":"chat.completion.chunk","created":1_700_000_000,"model":"gpt-4o",
               "choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}}),
    ]);
    frames.push((None, "[DONE]".to_owned()));
    frames
}

fn responses_text_frames() -> Vec<(Option<String>, String)> {
    json_frames(&[
        json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","status":"in_progress","model":"gpt-5","output":[]}}),
        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","sequence_number":2,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"delta":"Hello "}),
        json!({"type":"response.output_text.delta","sequence_number":4,"output_index":0,"content_index":0,"delta":"world"}),
        json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"calc","arguments":"","status":"in_progress"}}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":6,"output_index":1,"delta":"{\"x\":"}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":7,"output_index":1,"delta":"1}"}),
        json!({"type":"response.completed","sequence_number":8,"response":{"id":"resp_1","object":"response","status":"completed","model":"gpt-5","output":[],
               "usage":{"input_tokens":5,"output_tokens":4,"total_tokens":9}}}),
    ])
}

fn captured_frames(dialect: IngressDialect) -> Vec<(Option<String>, String)> {
    match dialect {
        IngressDialect::Anthropic => sse_frames(ANTHROPIC_STREAM),
        IngressDialect::Chat => sse_frames(CHAT_STREAM),
        IngressDialect::Responses => line_frames(RESPONSES_STREAM),
    }
}

/// The captured stream with every frame from the terminal one onward cut.
fn without_terminal(dialect: IngressDialect) -> Vec<(Option<String>, String)> {
    let terminal = match dialect {
        IngressDialect::Anthropic => "\"message_stop\"",
        IngressDialect::Chat => "\"finish_reason\":\"stop\"",
        IngressDialect::Responses => "\"response.completed\"",
    };
    captured_frames(dialect)
        .into_iter()
        .take_while(|(_, data)| !data.contains(terminal))
        .collect()
}

fn error_frame(dialect: IngressDialect) -> (Option<String>, String) {
    match dialect {
        IngressDialect::Anthropic => (
            Some("error".to_owned()),
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}).to_string(),
        ),
        IngressDialect::Chat => (
            None,
            json!({"error":{"type":"server_error","code":"server_error","message":"boom"}}).to_string(),
        ),
        IngressDialect::Responses => (
            Some("response.failed".to_owned()),
            json!({"type":"response.failed","response":{"id":"resp_fixture","status":"failed","error":{"code":"server_error","message":"boom"}}}).to_string(),
        ),
    }
}

#[test]
fn captured_anthropic_stream_assembles_to_the_message_body() {
    let document = complete(
        IngressDialect::Anthropic,
        &captured_frames(IngressDialect::Anthropic),
    );

    assert_eq!(
        document,
        json!({
            "id": "msg_sse01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-5-20251101",
            "content": [
                {"type": "thinking", "thinking": "Let me think...", "signature": "sig_xyz789"},
                {"type": "text", "text": "Hello world!"}
            ],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 100, "output_tokens": 20}
        })
    );
}

#[test]
fn anthropic_tool_use_input_is_joined_and_parsed() {
    let document = complete(IngressDialect::Anthropic, &anthropic_tool_frames());

    assert_eq!(
        document,
        json!({
            "id": "msg_tool",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [
                {"type": "text", "text": "Looking it up."},
                {"type": "tool_use", "id": "toolu_01", "name": "search", "input": {"q": "rust"}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 20, "output_tokens": 10}
        })
    );
}

#[test]
fn anthropic_unparseable_tool_input_is_kept_as_text() {
    let mut frames = anthropic_tool_frames();
    frames.retain(|(_, data)| !data.contains("\\\"rust\\\"}"));

    let document = complete(IngressDialect::Anthropic, &frames);

    assert_eq!(document["content"][1]["input"], json!("{\"q\":"));
}

#[test]
fn captured_chat_stream_assembles_to_the_completion_body() {
    let document = complete(IngressDialect::Chat, &captured_frames(IngressDialect::Chat));

    assert_eq!(
        document,
        json!({
            "id": "chatcmpl-ds-sse-001",
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "reasoning_content": "Step 1 and step 2",
                    "content": "The answer."
                },
                "finish_reason": "stop"
            }]
        })
    );
}

#[test]
fn chat_tool_call_arguments_are_joined_and_usage_kept() {
    let document = complete(IngressDialect::Chat, &chat_tool_frames());

    assert_eq!(
        document,
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Let me check.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{\"x\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
        })
    );
}

#[test]
fn chat_terminal_waits_for_every_choice_to_finish() {
    let frames = json_frames(&[
        json!({"id":"c","choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null},{"index":1,"delta":{"content":"b"},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]);

    let result = assemble(IngressDialect::Chat, &frames, StreamExit::Ended);

    assert_eq!(result, Assembled::Incomplete(IncompleteReason::Disconnect));
}

#[test]
fn captured_responses_stream_assembles_to_the_response_body() {
    let frames = captured_frames(IngressDialect::Responses);
    let terminal: Value = frames
        .last()
        .and_then(|(_, data)| serde_json::from_str(data).ok())
        .unwrap_or_else(|| panic!("fixture has a terminal frame"));
    let mut expected = terminal["response"].clone();
    let mut output = vec![json!({
        "type": "reasoning",
        "id": "rs_fixture_1",
        "summary": [{"type": "summary_text", "text": "**Planning operation inspection and optimization**"}],
        "content": [],
        "encrypted_content": "enc_fixture"
    })];
    output.extend(expected["output"].as_array().cloned().unwrap_or_default());
    expected["output"] = Value::Array(output);

    let document = complete(IngressDialect::Responses, &frames);

    assert_eq!(document, expected);
}

#[test]
fn responses_text_and_argument_deltas_are_joined() {
    let document = complete(IngressDialect::Responses, &responses_text_frames());

    assert_eq!(
        document,
        json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "model": "gpt-5",
            "output": [
                {"type": "message", "role": "assistant", "status": "in_progress",
                 "content": [{"type": "output_text", "text": "Hello world", "annotations": []}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "calc",
                 "arguments": "{\"x\":1}", "status": "in_progress"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 4, "total_tokens": 9}
        })
    );
}

#[test]
fn each_dialect_and_exit_reason_is_incomplete() {
    let exits = [
        (
            "disconnect",
            StreamExit::Disconnect,
            IncompleteReason::Disconnect,
        ),
        (
            "cancelled",
            StreamExit::Cancelled,
            IncompleteReason::Cancelled,
        ),
        (
            "render failure",
            StreamExit::RenderFailure,
            IncompleteReason::RenderFailure,
        ),
        (
            "upstream error",
            StreamExit::UpstreamError,
            IncompleteReason::UpstreamError,
        ),
    ];

    for dialect in IngressDialect::ALL {
        for (name, exit, reason) in exits {
            let partial = assemble(dialect, &without_terminal(dialect), exit);
            let whole = assemble(dialect, &captured_frames(dialect), exit);

            assert_eq!(
                partial,
                Assembled::Incomplete(reason),
                "row {} mid-stream {name}",
                dialect.label()
            );
            assert_eq!(
                whole,
                Assembled::Incomplete(reason),
                "row {} after terminal {name}",
                dialect.label()
            );
        }
    }
}

#[test]
fn each_dialect_without_its_terminal_frame_is_a_disconnect() {
    let rows = [
        (
            "anthropic ends after message_delta",
            IngressDialect::Anthropic,
            "\"message_delta\"",
        ),
        (
            "chat ends before the finish chunk",
            IngressDialect::Chat,
            "\"content\"",
        ),
        (
            "responses ends before response.completed",
            IngressDialect::Responses,
            "\"response.output_item.done\"",
        ),
    ];

    for (name, dialect, last_kind) in rows {
        let frames = without_terminal(dialect);
        let last = frames.last().map_or("", |(_, data)| data.as_str());
        assert!(
            last.contains(last_kind),
            "row {name}: prefix ends with {last}"
        );

        let result = assemble(dialect, &frames, StreamExit::Ended);

        assert_eq!(
            result,
            Assembled::Incomplete(IncompleteReason::Disconnect),
            "row {name}"
        );
    }
}

#[test]
fn each_dialect_error_frame_is_an_upstream_error() {
    for dialect in IngressDialect::ALL {
        let head = without_terminal(dialect);
        let tail: Vec<_> = captured_frames(dialect)
            .into_iter()
            .skip(head.len())
            .collect();
        assert!(
            !tail.is_empty(),
            "row {}: fixture has no terminal",
            dialect.label()
        );
        let mut frames = head;
        frames.push(error_frame(dialect));
        frames.extend(tail);

        let result = assemble(dialect, &frames, StreamExit::Ended);

        assert_eq!(
            result,
            Assembled::Incomplete(IncompleteReason::UpstreamError),
            "row {}",
            dialect.label()
        );
    }
}

#[test]
fn responses_non_completed_terminals_are_upstream_errors() {
    for kind in [
        "response.failed",
        "response.incomplete",
        "response.cancelled",
    ] {
        let mut frames = without_terminal(IngressDialect::Responses);
        frames.push((
            None,
            json!({"type": kind, "response": {"id": "resp_fixture", "status": "x"}}).to_string(),
        ));

        let result = assemble(IngressDialect::Responses, &frames, StreamExit::Ended);

        assert_eq!(
            result,
            Assembled::Incomplete(IncompleteReason::UpstreamError),
            "row {kind}"
        );
    }
}

#[test]
fn chat_benign_error_fields_do_not_fail_the_stream() {
    let rows = [
        ("null error", json!(null)),
        ("empty error object", json!({})),
    ];

    for (name, error) in rows {
        let frames = json_frames(&[
            json!({"id":"c","error":error,"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null}]}),
            json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]);

        let result = assemble(IngressDialect::Chat, &frames, StreamExit::Ended);

        assert!(
            matches!(result, Assembled::Complete(_)),
            "row {name}: {result:?}"
        );
    }
}

#[test]
fn each_dialect_over_the_byte_cap_is_budget() {
    for dialect in IngressDialect::ALL {
        let mut assembler = StreamAssembler::with_limits(dialect, 256, 1_000);
        feed(&mut assembler, &captured_frames(dialect));

        let result = assembler.finish(StreamExit::Ended);

        assert_eq!(
            result,
            Assembled::Incomplete(IncompleteReason::Budget),
            "row {}",
            dialect.label()
        );
    }
}

#[test]
fn each_dialect_over_the_node_cap_is_budget() {
    for dialect in IngressDialect::ALL {
        let mut assembler = StreamAssembler::with_limits(dialect, usize::MAX, 4);
        feed(&mut assembler, &captured_frames(dialect));

        let result = assembler.finish(StreamExit::Ended);

        assert_eq!(
            result,
            Assembled::Incomplete(IncompleteReason::Budget),
            "row {}",
            dialect.label()
        );
    }
}

#[test]
fn each_dialect_within_both_caps_is_complete() {
    for dialect in IngressDialect::ALL {
        let frames = captured_frames(dialect);
        let bytes: usize = frames
            .iter()
            .map(|(e, d)| d.len() + e.as_ref().map_or(0, String::len))
            .sum();
        let mut assembler = StreamAssembler::with_limits(dialect, bytes, 1_000);
        feed(&mut assembler, &frames);

        let result = assembler.finish(StreamExit::Ended);

        assert!(
            matches!(result, Assembled::Complete(_)),
            "row {}: {result:?}",
            dialect.label()
        );
    }
}

#[test]
fn responses_sparse_part_index_is_budget_not_an_allocation() {
    let mut frames = without_terminal(IngressDialect::Responses);
    frames.push((
        None,
        json!({"type":"response.output_text.delta","output_index":0,"content_index":u64::MAX,"delta":"x"}).to_string(),
    ));
    frames.push((
        None,
        json!({"type":"response.output_text.delta","output_index":1,"content_index":10_000_000_000_u64,"delta":"x"})
            .to_string(),
    ));

    let result = assemble(IngressDialect::Responses, &frames, StreamExit::Ended);

    assert_eq!(result, Assembled::Incomplete(IncompleteReason::Budget));
}

#[test]
fn hostile_frames_never_panic_and_fail_closed() {
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let deep_object = format!("{}1{}", "{\"a\":".repeat(10_000), "}".repeat(10_000));
    let rows: [(&str, Option<&str>, &str); 9] = [
        (
            "truncated json",
            None,
            "{\"type\":\"message_start\",\"message\":{",
        ),
        ("bare array", None, "[1,2,3]"),
        ("bare number", None, "42"),
        ("bare string", None, "\"hello\""),
        ("invalid utf8 escape", None, "{\"type\":\"\\ud800\"}"),
        ("deep array nesting", None, &deep),
        ("deep object nesting", Some("message_start"), &deep_object),
        ("binary noise", None, "\u{1}\u{2}\u{3}"),
        ("done then garbage", None, "[DONE] trailing"),
    ];

    for dialect in IngressDialect::ALL {
        for (name, event, data) in rows {
            let mut assembler = StreamAssembler::new(dialect);
            assembler.push(SseFrame::new(event, data));

            let result = assembler.finish(StreamExit::Ended);

            assert_eq!(
                result,
                Assembled::Incomplete(IncompleteReason::UpstreamError),
                "row {} {name}",
                dialect.label()
            );
        }
    }
}

#[test]
fn wrong_typed_fields_never_panic() {
    let frames = json_frames(&[
        json!({"type":"message_start","message":"not an object"}),
        json!({"type":"content_block_start","index":"zero","content_block":7}),
        json!({"type":"content_block_delta","index":-1,"delta":[1]}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":5}}),
        json!({"type":"message_delta","delta":null,"usage":"x"}),
        json!({"id":7,"choices":[1,{"index":"a","delta":{"tool_calls":[null,{"index":"b","function":3}]},"finish_reason":5}]}),
        json!({"choices":{"index":0}}),
        json!({"type":"response.output_item.added","output_index":"x","item":1}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":{"a":1}}),
        json!({"type":"response.output_text.delta","output_index":2,"content_index":"y","delta":3}),
        json!({"type":"response.completed","response":[]}),
    ]);

    for dialect in IngressDialect::ALL {
        for exit in [StreamExit::Ended, StreamExit::Disconnect] {
            let mut assembler = StreamAssembler::new(dialect);
            feed(&mut assembler, &frames);

            let _ = assembler.finish(exit);
        }
    }
}

#[test]
fn keepalive_frames_are_ignored() {
    for dialect in IngressDialect::ALL {
        let mut frames = Vec::new();
        for frame in captured_frames(dialect) {
            frames.push((None, String::new()));
            frames.push((None, ": keepalive".to_owned()));
            frames.push(frame);
        }

        let with_keepalives = assemble(dialect, &frames, StreamExit::Ended);
        let plain = assemble(dialect, &captured_frames(dialect), StreamExit::Ended);

        assert_eq!(with_keepalives, plain, "row {}", dialect.label());
        assert!(
            matches!(plain, Assembled::Complete(_)),
            "row {}",
            dialect.label()
        );
    }
}

#[test]
fn incomplete_reason_labels_are_distinct_static_tokens() {
    let labels: Vec<&str> = IncompleteReason::ALL.iter().map(|r| r.label()).collect();

    assert_eq!(
        labels,
        [
            "disconnect",
            "cancelled",
            "render_failure",
            "upstream_error",
            "budget"
        ]
    );
}

#[test]
fn debug_renders_no_frame_or_document_content() {
    let secret = "do-not-render-this";
    let frame = SseFrame::new(Some("message_start"), secret);
    let document = Assembled::Complete(json!({"text": secret}));
    let mut assembler = StreamAssembler::new(IngressDialect::Anthropic);
    assembler.push(SseFrame::new(
        None,
        &json!({"type":"message_start","message":{"id":secret}}).to_string(),
    ));

    let rendered = format!("{frame:?} {document:?} {assembler:?}");

    assert!(!rendered.contains(secret), "{rendered}");
    assert!(rendered.contains("data_len"), "{rendered}");
}
