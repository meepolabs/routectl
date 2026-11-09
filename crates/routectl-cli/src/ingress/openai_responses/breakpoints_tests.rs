//! Behavior tests for the Responses ingress `prompt_cache_breakpoint`
//! capture: where each recorded breakpoint lands in the post-lift text-block
//! stream, and that capturing it leaves canonical content alone.

use axum::http::HeaderMap;
use routectl_core::{ChatRequest, ResponsesCacheBreakpoint};
use routectl_providers::openai_responses::breakpoint_text_digest;
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::openai_responses::ResponsesIngress;

fn parse(body: Value) -> ChatRequest {
    ResponsesIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn breakpoint() -> Value {
    json!({ "mode": "explicit" })
}

fn text_block(kind: &str, text: &str, marked: bool) -> Value {
    let mut block = json!({ "type": kind, "text": text });
    if marked {
        block["prompt_cache_breakpoint"] = breakpoint();
    }
    block
}

fn message(role: &str, blocks: Vec<Value>) -> Value {
    json!({ "type": "message", "role": role, "content": blocks })
}

struct Expected {
    ordinal: Option<usize>,
    pointer: &'static str,
    text: &'static str,
}

fn assert_recorded(row: &str, recorded: &[ResponsesCacheBreakpoint], expected: &[Expected]) {
    assert_eq!(recorded.len(), expected.len(), "{row}: breakpoint count");
    for (got, want) in recorded.iter().zip(expected) {
        assert_eq!(
            got.ordinal, want.ordinal,
            "{row}: ordinal of {}",
            want.pointer
        );
        assert_eq!(got.client_pointer, want.pointer, "{row}: pointer");
        assert_eq!(
            got.text_digest,
            breakpoint_text_digest(want.text),
            "{row}: digest of {}",
            want.pointer
        );
        assert_eq!(got.value, breakpoint(), "{row}: value is verbatim");
    }
}

#[test]
fn breakpoints_record_post_lift_ordinal_digest_and_pointer() {
    struct Row {
        name: &'static str,
        input: Value,
        expected: Vec<Expected>,
    }
    let rows = [
        Row {
            name: "a user input_text gets ordinal 0",
            input: json!([message(
                "user",
                vec![text_block("input_text", "hello", true)]
            )]),
            expected: vec![Expected {
                ordinal: Some(0),
                pointer: "/input/0/content/0",
                text: "hello",
            }],
        },
        Row {
            name: "a lifted developer text-only message gets no ordinal",
            input: json!([
                message("developer", vec![text_block("input_text", "rules", true)]),
                message("user", vec![text_block("input_text", "hello", false)]),
            ]),
            expected: vec![Expected {
                ordinal: None,
                pointer: "/input/0/content/0",
                text: "rules",
            }],
        },
        Row {
            name: "a breakpoint after a lifted message keeps a contiguous ordinal",
            input: json!([
                message(
                    "user",
                    vec![
                        text_block("input_text", "first", false),
                        text_block("input_text", "second", false),
                    ]
                ),
                message("developer", vec![text_block("input_text", "rules", false)]),
                message(
                    "user",
                    vec![
                        json!({ "type": "input_image", "image_url": "https://example.com/a.png" }),
                        text_block("input_text", "third", true),
                    ]
                ),
            ]),
            expected: vec![Expected {
                ordinal: Some(2),
                pointer: "/input/2/content/1",
                text: "third",
            }],
        },
        Row {
            name: "output_text and function_call_output breakpoints get no ordinal",
            input: json!([
                message("user", vec![text_block("input_text", "run it", false)]),
                message("assistant", vec![text_block("output_text", "calling", true)]),
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "run",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [text_block("input_text", "done", true)]
                },
                message("user", vec![text_block("input_text", "next", true)]),
            ]),
            expected: vec![
                Expected {
                    ordinal: None,
                    pointer: "/input/1/content/0",
                    text: "calling",
                },
                Expected {
                    ordinal: None,
                    pointer: "/input/3/output/0",
                    text: "done",
                },
                Expected {
                    ordinal: Some(1),
                    pointer: "/input/4/content/0",
                    text: "next",
                },
            ],
        },
    ];

    for row in rows {
        let req = parse(json!({ "model": "gpt-5.6", "input": row.input }));
        assert_recorded(
            row.name,
            &req.routectl_internal.responses_cache_breakpoints,
            &row.expected,
        );
    }
}

#[test]
fn breakpoint_capture_leaves_canonical_content_unchanged() {
    let input = |marked: bool| {
        json!([
            message("developer", vec![text_block("input_text", "rules", marked)]),
            message(
                "user",
                vec![
                    text_block("input_text", "first", marked),
                    text_block("input_text", "second", marked),
                ]
            ),
            message(
                "assistant",
                vec![text_block("output_text", "reply", marked)]
            ),
            message("user", vec![text_block("input_text", "again", marked)]),
        ])
    };
    let body = |marked: bool| json!({ "model": "gpt-5.6", "instructions": "be brief", "input": input(marked) });

    let plain = parse(body(false));
    let marked = parse(body(true));

    assert!(
        plain
            .routectl_internal
            .responses_cache_breakpoints
            .is_empty()
    );
    assert_eq!(
        marked.routectl_internal.responses_cache_breakpoints.len(),
        5,
        "every marked block is recorded",
    );
    let canonical =
        |req: &ChatRequest| serde_json::to_string(req).expect("the canonical request serializes");
    assert_eq!(
        canonical(&marked),
        canonical(&plain),
        "the canonical request is byte-identical with and without breakpoints",
    );
}
