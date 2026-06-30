//! Non-stream renderer tests for the openai-responses ingress.
//!
//! Loaded via `#[path = "render_tests.rs"] mod tests;` in `render.rs`.
//! These pin the canonical `ChatResponse` -> Responses `response` object
//! mapping (the inverse of the egress response parser). Field names and
//! tag values are asserted exactly against the deserialize-side wire
//! types in `routectl-providers/.../response_types.rs`.

use serde_json::{json, Value};

use routectl_core::{
    schema::Choice, ChatResponse, ContentPart, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Role, Usage,
};

use super::render_responses_response;
use crate::ingress::openai_responses::OPENAI_RESPONSES_FORMAT;

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn assistant_message(content: MessageContent) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn response_with(message: Message, finish_reason: Option<&str>) -> ChatResponse {
    ChatResponse {
        id: "resp_01".into(),
        model: "gpt-5-codex".into(),
        created: 1_700_000_000,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message,
            finish_reason: finish_reason.map(str::to_string),
            matched_stop_sequence: None,
        }],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    }
}

fn render(message: Message, finish_reason: Option<&str>) -> Value {
    render_responses_response(response_with(message, finish_reason)).unwrap()
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.to_string(),
        cache_control: None,
    })
}

fn responses_detail(kind: ReasoningDetailKind, id: &str, payload: Value) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: Some(id.to_string()),
        format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
        index: None,
        payload,
    }
}

fn output(v: &Value) -> &Vec<Value> {
    v["output"].as_array().expect("output array")
}

// ---------------------------------------------------------------------------
// Top-level shape
// ---------------------------------------------------------------------------

#[test]
fn renders_top_level_response_envelope() {
    // Arrange
    let msg = assistant_message(MessageContent::Text("hello".into()));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    assert_eq!(v["object"], "response");
    assert_eq!(v["id"], "resp_01");
    assert_eq!(v["created_at"], 1_700_000_000);
    assert_eq!(v["model"], "gpt-5-codex");
    assert!(v["output"].is_array());
}

#[test]
fn text_only_response_emits_output_text_message_item() {
    // Arrange
    let msg = assistant_message(MessageContent::Text("answer".into()));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["role"], "assistant");
    assert_eq!(items[0]["status"], "completed");
    let blocks = items[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "output_text");
    assert_eq!(blocks[0]["text"], "answer");
    assert_eq!(blocks[0]["annotations"], json!([]));
}

#[test]
fn empty_text_response_emits_no_message_item() {
    // A finished turn with no renderable content produces an empty
    // output array (not a message item with an empty content array).
    // Arrange
    let msg = assistant_message(MessageContent::Text(String::new()));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    assert!(output(&v).is_empty());
}

#[test]
fn null_content_response_emits_no_message_item() {
    // Arrange
    let msg = assistant_message(MessageContent::Null);

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    assert!(output(&v).is_empty());
}

#[test]
fn no_choices_response_emits_empty_output() {
    // Arrange: a response with no choices at all (degenerate upstream).
    let resp = ChatResponse {
        id: "resp_x".into(),
        model: "m".into(),
        created: 0,
        choices: vec![],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };

    // Act
    let v = render_responses_response(resp).unwrap();

    // Assert
    assert_eq!(v["status"], "completed");
    assert!(output(&v).is_empty());
}

// ---------------------------------------------------------------------------
// status + incomplete_details (inverse of map_finish_reason)
// ---------------------------------------------------------------------------

#[test]
fn finish_stop_maps_to_status_completed_without_incomplete_details() {
    // Act
    let v = render(
        assistant_message(MessageContent::Text("x".into())),
        Some("stop"),
    );

    // Assert
    assert_eq!(v["status"], "completed");
    assert!(v.get("incomplete_details").is_none());
}

#[test]
fn finish_tool_calls_maps_to_status_completed() {
    // A tool-call turn is still a completed response in the Responses
    // shape; the function_call output item carries the tool intent.
    // Act
    let v = render(assistant_message(MessageContent::Null), Some("tool_calls"));

    // Assert
    assert_eq!(v["status"], "completed");
    assert!(v.get("incomplete_details").is_none());
}

#[test]
fn finish_length_maps_to_incomplete_max_output_tokens() {
    // Act
    let v = render(
        assistant_message(MessageContent::Text("trunc".into())),
        Some("length"),
    );

    // Assert
    assert_eq!(v["status"], "incomplete");
    assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
}

#[test]
fn finish_content_filter_maps_to_incomplete_content_filter() {
    // Act
    let v = render(
        assistant_message(MessageContent::Null),
        Some("content_filter"),
    );

    // Assert
    assert_eq!(v["status"], "incomplete");
    assert_eq!(v["incomplete_details"]["reason"], "content_filter");
}

#[test]
fn finish_error_maps_to_status_failed() {
    // Act
    let v = render(assistant_message(MessageContent::Null), Some("error"));

    // Assert
    assert_eq!(v["status"], "failed");
    assert!(v.get("incomplete_details").is_none());
}

#[test]
fn unknown_finish_reason_defaults_to_completed() {
    // A finish_reason the egress never emits (or a passthrough) still
    // renders a finished turn rather than an error.
    // Act
    let v = render(
        assistant_message(MessageContent::Text("x".into())),
        Some("some_future_reason"),
    );

    // Assert
    assert_eq!(v["status"], "completed");
    assert!(v.get("incomplete_details").is_none());
}

#[test]
fn missing_finish_reason_defaults_to_completed() {
    // Act
    let v = render(assistant_message(MessageContent::Text("x".into())), None);

    // Assert
    assert_eq!(v["status"], "completed");
}

// ---------------------------------------------------------------------------
// function_call items (inverse of FunctionCall walk)
// ---------------------------------------------------------------------------

#[test]
fn tool_calls_render_to_function_call_items_with_string_arguments() {
    // Arrange
    let mut msg = assistant_message(MessageContent::Null);
    msg.tool_calls = Some(vec![json!({
        "id": "call_abc",
        "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"loc\":\"Tokyo\"}"}
    })]);

    // Act
    let v = render(msg, Some("tool_calls"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "function_call");
    assert_eq!(items[0]["call_id"], "call_abc");
    assert_eq!(items[0]["name"], "get_weather");
    // arguments stays a JSON STRING on the Responses wire.
    assert_eq!(items[0]["arguments"], "{\"loc\":\"Tokyo\"}");
    assert!(items[0]["arguments"].is_string());
}

#[test]
fn multiple_tool_calls_render_one_item_each_in_order() {
    // Arrange
    let mut msg = assistant_message(MessageContent::Null);
    msg.tool_calls = Some(vec![
        json!({"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}}),
        json!({"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{}"}}),
    ]);

    // Act
    let v = render(msg, Some("tool_calls"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["call_id"], "c1");
    assert_eq!(items[1]["call_id"], "c2");
}

// ---------------------------------------------------------------------------
// reasoning items (inverse of the egress reasoning walk + lift)
// ---------------------------------------------------------------------------

#[test]
fn reasoning_summary_renders_summary_text_block() {
    // Arrange
    let mut msg = assistant_message(MessageContent::Text("ans".into()));
    msg.reasoning_details = vec![responses_detail(
        ReasoningDetailKind::Summary,
        "rs_1",
        json!({"text": "thinking step"}),
    )];

    // Act
    let v = render(msg, Some("stop"));

    // Assert: reasoning item comes first, then the message item.
    let items = output(&v);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[0]["id"], "rs_1");
    let summary = items[0]["summary"].as_array().unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["type"], "summary_text");
    assert_eq!(summary[0]["text"], "thinking step");
    assert_eq!(items[1]["type"], "message");
}

#[test]
fn reasoning_text_renders_reasoning_text_content_block() {
    // Arrange
    let mut msg = assistant_message(MessageContent::Null);
    msg.reasoning_details = vec![responses_detail(
        ReasoningDetailKind::Text,
        "rs_1",
        json!({"text": "chain of thought"}),
    )];

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "reasoning_text");
    assert_eq!(content[0]["text"], "chain of thought");
}

#[test]
fn first_encrypted_detail_becomes_item_level_signature() {
    // Inverse of the egress: the item-level encrypted_content signature
    // round-trips as a single Encrypted detail. Render must lift it back
    // onto the item, NOT into a content block.
    // Arrange
    let mut msg = assistant_message(MessageContent::Null);
    msg.reasoning_details = vec![responses_detail(
        ReasoningDetailKind::Encrypted,
        "rs_1",
        json!({"encrypted_content": "SIG_PAYLOAD"}),
    )];

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["encrypted_content"], "SIG_PAYLOAD");
    // No inner content block emitted for the lone signature.
    assert!(items[0].get("content").is_none());
}

#[test]
fn second_encrypted_detail_becomes_inner_reasoning_encrypted_block() {
    // Two Encrypted details on one id: first is the item-level
    // signature, the second is preserved as an inner
    // reasoning_encrypted content block (mirrors lift_reasoning_details).
    // Arrange
    let mut msg = assistant_message(MessageContent::Null);
    msg.reasoning_details = vec![
        responses_detail(
            ReasoningDetailKind::Encrypted,
            "rs_1",
            json!({"encrypted_content": "SIG"}),
        ),
        responses_detail(
            ReasoningDetailKind::Encrypted,
            "rs_1",
            json!({"encrypted_content": "INNER_ENC"}),
        ),
    ];

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let items = output(&v);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["encrypted_content"], "SIG");
    let content = items[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "reasoning_encrypted");
    assert_eq!(content[0]["encrypted_content"], "INNER_ENC");
}

#[test]
fn full_reasoning_item_round_trips_summary_content_and_signature() {
    // Arrange: a complete reasoning item as the egress would have
    // produced from one upstream Reasoning item (summary + content +
    // item-level signature), all sharing the upstream id.
    let mut msg = assistant_message(MessageContent::Text("answer".into()));
    msg.reasoning_details = vec![
        responses_detail(
            ReasoningDetailKind::Summary,
            "rs_42",
            json!({"text": "step"}),
        ),
        responses_detail(
            ReasoningDetailKind::Text,
            "rs_42",
            json!({"text": "detail"}),
        ),
        responses_detail(
            ReasoningDetailKind::Encrypted,
            "rs_42",
            json!({"encrypted_content": "ENC1"}),
        ),
    ];

    // Act
    let v = render(msg, Some("stop"));

    // Assert: one reasoning item (single id) + one message item.
    let items = output(&v);
    assert_eq!(items.len(), 2);
    let reasoning = &items[0];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["id"], "rs_42");
    assert_eq!(reasoning["summary"][0]["text"], "step");
    assert_eq!(reasoning["content"][0]["type"], "reasoning_text");
    assert_eq!(reasoning["content"][0]["text"], "detail");
    assert_eq!(reasoning["encrypted_content"], "ENC1");
}

#[test]
fn reasoning_details_with_distinct_ids_render_separate_items() {
    // Arrange: two reasoning items, each with its own upstream id.
    let mut msg = assistant_message(MessageContent::Null);
    msg.reasoning_details = vec![
        responses_detail(
            ReasoningDetailKind::Summary,
            "rs_a",
            json!({"text": "first"}),
        ),
        responses_detail(
            ReasoningDetailKind::Summary,
            "rs_b",
            json!({"text": "second"}),
        ),
    ];

    // Act
    let v = render(msg, Some("stop"));

    // Assert: grouping by id yields two reasoning items in first-seen
    // order.
    let items = output(&v);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "rs_a");
    assert_eq!(items[1]["id"], "rs_b");
}

#[test]
fn foreign_format_reasoning_details_are_skipped() {
    // Reasoning history tagged for another dialect (e.g. anthropic)
    // cannot deserialize into the Responses reasoning shape, so it is
    // dropped rather than emitted with a bogus shape.
    // Arrange
    let mut msg = assistant_message(MessageContent::Text("ans".into()));
    msg.reasoning_details = vec![ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("an_1".into()),
        format: Some("anthropic-claude-v1".into()),
        index: None,
        payload: json!({"text": "claude thinking"}),
    }];

    // Act
    let v = render(msg, Some("stop"));

    // Assert: only the message item, no reasoning item.
    let items = output(&v);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
}

// ---------------------------------------------------------------------------
// refusal + forward-compat parts
// ---------------------------------------------------------------------------

#[test]
fn refusal_part_renders_refusal_content_block() {
    // Arrange
    let mut extras = serde_json::Map::new();
    extras.insert("refusal".into(), Value::String("cannot help".into()));
    let msg = assistant_message(MessageContent::Parts(vec![ContentPart::Other {
        type_tag: "refusal".into(),
        cache_control: None,
        extras,
    }]));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let blocks = output(&v)[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "refusal");
    assert_eq!(blocks[0]["refusal"], "cannot help");
}

#[test]
fn forward_compat_other_part_re_emits_type_tag_and_extras() {
    // A future content block preserved as ContentPart::Other must be
    // re-emitted with its type tag and verbatim extra fields.
    // Arrange
    let mut extras = serde_json::Map::new();
    extras.insert("foo".into(), json!("bar"));
    extras.insert("count".into(), json!(3));
    let msg = assistant_message(MessageContent::Parts(vec![ContentPart::Other {
        type_tag: "future_block".into(),
        cache_control: None,
        extras,
    }]));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let blocks = output(&v)[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "future_block");
    assert_eq!(blocks[0]["foo"], "bar");
    assert_eq!(blocks[0]["count"], 3);
}

#[test]
fn mixed_text_and_refusal_parts_preserve_order() {
    // Arrange
    let mut extras = serde_json::Map::new();
    extras.insert("refusal".into(), Value::String("no".into()));
    let msg = assistant_message(MessageContent::Parts(vec![
        text_part("first"),
        ContentPart::Other {
            type_tag: "refusal".into(),
            cache_control: None,
            extras,
        },
    ]));

    // Act
    let v = render(msg, Some("stop"));

    // Assert
    let blocks = output(&v)[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "output_text");
    assert_eq!(blocks[0]["text"], "first");
    assert_eq!(blocks[1]["type"], "refusal");
}

#[test]
fn tool_use_part_is_not_emitted_as_a_content_block() {
    // The egress parses a function_call into BOTH tool_calls and a
    // ToolUse part. The renderer uses tool_calls as the source of truth
    // and must NOT also emit the ToolUse part as a message content block
    // (that would duplicate the call).
    // Arrange
    let mut msg = assistant_message(MessageContent::Parts(vec![
        text_part("here"),
        ContentPart::Known(KnownContentPart::ToolUse {
            id: "call_1".into(),
            name: "f".into(),
            input: json!({}),
            cache_control: None,
        }),
    ]));
    msg.tool_calls = Some(vec![json!({
        "id": "call_1", "type": "function",
        "function": {"name": "f", "arguments": "{}"}
    })]);

    // Act
    let v = render(msg, Some("tool_calls"));

    // Assert: message item carries only the text block; the call comes
    // from the single function_call item.
    let items = output(&v);
    let message = items.iter().find(|i| i["type"] == "message").unwrap();
    let blocks = message["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "output_text");
    let fc_count = items
        .iter()
        .filter(|i| i["type"] == "function_call")
        .count();
    assert_eq!(fc_count, 1);
}

#[test]
fn mixed_reasoning_text_and_tool_call_render_in_arrival_order() {
    // Arrange: reasoning + assistant text + a tool call all on one turn.
    let mut msg = assistant_message(MessageContent::Text("done".into()));
    msg.reasoning_details = vec![responses_detail(
        ReasoningDetailKind::Summary,
        "rs_1",
        json!({"text": "plan"}),
    )];
    msg.tool_calls = Some(vec![json!({
        "id": "call_1", "type": "function",
        "function": {"name": "f", "arguments": "{}"}
    })]);

    // Act
    let v = render(msg, Some("tool_calls"));

    // Assert: order is reasoning -> message -> function_call.
    let items = output(&v);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[1]["type"], "message");
    assert_eq!(items[2]["type"], "function_call");
}

// ---------------------------------------------------------------------------
// usage (inverse of translate_usage)
// ---------------------------------------------------------------------------

#[test]
fn usage_renders_input_output_total_tokens() {
    // Arrange
    let mut resp = response_with(
        assistant_message(MessageContent::Text("x".into())),
        Some("stop"),
    );
    resp.usage = Some(Usage {
        prompt_tokens: 100,
        completion_tokens: 25,
        total_tokens: 125,
        ..Default::default()
    });

    // Act
    let v = render_responses_response(resp).unwrap();

    // Assert
    assert_eq!(v["usage"]["input_tokens"], 100);
    assert_eq!(v["usage"]["output_tokens"], 25);
    assert_eq!(v["usage"]["total_tokens"], 125);
}

#[test]
fn usage_emits_cached_and_reasoning_token_sub_details() {
    // Arrange
    let mut resp = response_with(
        assistant_message(MessageContent::Text("x".into())),
        Some("stop"),
    );
    resp.usage = Some(Usage {
        prompt_tokens: 100,
        completion_tokens: 25,
        total_tokens: 125,
        reasoning_tokens: Some(15),
        cache_read_input_tokens: Some(40),
        ..Default::default()
    });

    // Act
    let v = render_responses_response(resp).unwrap();

    // Assert
    assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 40);
    assert_eq!(v["usage"]["output_tokens_details"]["reasoning_tokens"], 15);
}

#[test]
fn usage_omits_detail_sub_objects_when_sources_absent() {
    // Arrange
    let mut resp = response_with(
        assistant_message(MessageContent::Text("x".into())),
        Some("stop"),
    );
    resp.usage = Some(Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        ..Default::default()
    });

    // Act
    let v = render_responses_response(resp).unwrap();

    // Assert: no empty detail objects on the wire.
    assert!(v["usage"].get("input_tokens_details").is_none());
    assert!(v["usage"].get("output_tokens_details").is_none());
}

#[test]
fn no_usage_omits_usage_object() {
    // Arrange: response with usage None.
    let v = render(
        assistant_message(MessageContent::Text("x".into())),
        Some("stop"),
    );

    // Assert
    assert!(v.get("usage").is_none());
}
