//! Members of Responses input items and content blocks the canonical model
//! does not type, captured by the ingress into the node they belong to.

use axum::http::HeaderMap;
use routectl_core::{ChatRequest, ContentPart, KnownContentPart, MessageContent, Role};
use serde_json::{Map, Value, json};

use super::ResponsesIngress;
use crate::ingress::IngressAdapter;

fn parse(body: Value) -> ChatRequest {
    ResponsesIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("expected an object, got {other}"),
    }
}

fn part_extras(part: &ContentPart) -> &Map<String, Value> {
    match part {
        ContentPart::Known(
            KnownContentPart::Text { extras, .. } | KnownContentPart::ImageUrl { extras, .. },
        ) => extras,
        other => panic!("expected a modeled text or image part, got {other:?}"),
    }
}

/// Every message-item member but `type`, `role` and `content` rides the
/// message, non-ASCII values included; a modeled block's members ride its
/// part, without the members the part types.
#[test]
fn message_and_block_members_ride_the_nodes_they_belong_to() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "message", "id": "msg_1", "role": "user", "phase": "caf\u{e9}",
         "content": [
            {"type": "input_text", "text": "hi", "annotations": []},
            {"type": "input_image", "image_url": "https://example.com/a.png",
             "detail": "low", "file_id": "f_1"}]}]});

    // Act
    let req = parse(body);

    // Assert
    let message = &req.messages[0];
    assert_eq!(
        message.extras,
        map(json!({"id": "msg_1", "phase": "caf\u{e9}"}))
    );
    let MessageContent::Parts(parts) = &message.content else {
        panic!("expected parts, got {:?}", message.content);
    };
    assert_eq!(part_extras(&parts[0]), &map(json!({"annotations": []})));
    assert_eq!(part_extras(&parts[1]), &map(json!({"file_id": "f_1"})));
}

/// A block's `prompt_cache_breakpoint` is the breakpoint record's, never
/// the part's carrier.
#[test]
fn a_prompt_cache_breakpoint_never_enters_a_part_carrier() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "a", "prompt_cache_breakpoint": {"mode": "explicit"}},
            {"type": "input_text", "text": "b"}]}]});

    // Act
    let req = parse(body);

    // Assert
    let MessageContent::Parts(parts) = &req.messages[0].content else {
        panic!("expected parts, got {:?}", req.messages[0].content);
    };
    assert!(part_extras(&parts[0]).is_empty(), "{:?}", parts[0]);
    assert_eq!(req.routectl_internal.responses_cache_breakpoints.len(), 1);
}

/// A single text block collapses to flat text only when it carries nothing
/// else; one with a member stays a part so the member keeps a home.
#[test]
fn a_single_text_block_collapses_only_when_it_carries_no_member() {
    let rows = [
        (
            "plain block",
            json!({"type": "input_text", "text": "hi"}),
            true,
        ),
        (
            "block with a member",
            json!({"type": "input_text", "text": "hi", "annotations": []}),
            false,
        ),
    ];
    for (row, block, collapses) in rows {
        // Arrange
        let body = json!({"model": "m", "input": [
            {"type": "message", "role": "user", "content": [block]}]});

        // Act
        let req = parse(body);

        // Assert
        let content = &req.messages[0].content;
        assert_eq!(
            matches!(content, MessageContent::Text(t) if t == "hi"),
            collapses,
            "{row}: {content:?}"
        );
        if !collapses {
            let MessageContent::Parts(parts) = content else {
                panic!("{row}: expected parts, got {content:?}");
            };
            assert_eq!(
                part_extras(&parts[0]),
                &map(json!({"annotations": []})),
                "{row}"
            );
        }
    }
}

/// A `function_call_output` item's members other than `type`, `call_id`
/// and `output` ride the tool message.
#[test]
fn function_call_output_members_ride_the_tool_message() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        {"type": "function_call_output", "id": "fco_1", "call_id": "c1", "output": "42",
         "status": "completed"}]});

    // Act
    let req = parse(body);

    // Assert
    let tool = &req.messages[1];
    assert!(matches!(tool.role, Role::Tool));
    assert_eq!(
        tool.extras,
        map(json!({"id": "fco_1", "status": "completed"}))
    );
}

/// A developer or system item carrying a member stays in place and keeps
/// it; the ingress records which kept turns were developer items, by their
/// post-lift message index, while a plain one is still lifted.
#[test]
fn a_developer_item_with_a_member_stays_in_place_and_is_recorded() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "message", "role": "developer", "content": "lifted"},
        {"type": "message", "role": "developer", "id": "msg_d1",
         "content": [{"type": "input_text", "text": "rule one"}]},
        {"type": "message", "role": "system", "id": "msg_s1", "content": "rule two"},
        {"type": "message", "role": "user", "content": "hi"}]});

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 3, "{:?}", req.messages);
    assert_eq!(req.messages[0].extras, map(json!({"id": "msg_d1"})));
    assert_eq!(req.messages[1].extras, map(json!({"id": "msg_s1"})));
    assert_eq!(req.routectl_internal.responses_developer_turns, vec![0]);
    let declared: Vec<String> = req
        .routectl_internal
        .declared_edits
        .iter()
        .map(|edit| edit.path().to_string())
        .collect();
    assert!(
        !declared.iter().any(|path| path.ends_with("/role")),
        "a kept developer item keeps its role: {declared:?}"
    );
}

/// Across many interleaved items, the record names exactly the kept
/// developer turns by their post-lift index: lifted developer items and
/// kept system items in between never shift or join it.
#[test]
fn many_developer_items_record_every_kept_developer_turn_in_order() {
    // Arrange
    const GROUPS: usize = 64;
    let mut input = Vec::new();
    for group in 0..GROUPS {
        input.push(
            json!({"type": "message", "role": "developer", "id": format!("msg_d{group}"),
                          "content": [{"type": "input_text", "text": "kept"}]}),
        );
        input.push(json!({"type": "message", "role": "developer", "content": "lifted"}));
        input.push(
            json!({"type": "message", "role": "system", "id": format!("msg_s{group}"),
                          "content": "kept system"}),
        );
        input.push(json!({"type": "message", "role": "user", "content": "hi"}));
    }
    let body = json!({"model": "m", "input": input});

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), GROUPS * 3);
    let expected: Vec<usize> = (0..GROUPS).map(|group| group * 3).collect();
    assert_eq!(req.routectl_internal.responses_developer_turns, expected);
}

/// A `function_call` item's members outside the canonical call ride the
/// item-member record under its raw `call_id`, and a `reasoning` item's
/// under its `id`, each with the item's own `/input/N`; no typed member
/// enters the record.
#[test]
fn function_call_and_reasoning_members_ride_the_item_record_without_typed_members() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "message", "role": "user", "content": "hi"},
        {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "sig",
         "status": "completed"},
        {"type": "function_call", "id": "fc_1", "call_id": "call.a:1", "name": "f",
         "namespace": "ns", "arguments": "{}", "status": "completed",
         "zz_future": {"k": "caf\u{e9}"}}]});

    // Act
    let req = parse(body);

    // Assert
    let record = &req.routectl_internal.responses_item_members;
    let call = &record.function_call["call.a:1"];
    assert_eq!(call.client_pointer, "/input/2");
    assert_eq!(
        call.members,
        map(json!({"id": "fc_1", "status": "completed", "zz_future": {"k": "caf\u{e9}"}}))
    );
    let reasoning = &record.reasoning["rs_1"];
    assert_eq!(reasoning.client_pointer, "/input/1");
    assert_eq!(reasoning.members, map(json!({"status": "completed"})));
    assert!(record.unkeyed.is_empty(), "{record:?}");
}

/// The typed `call_id` and `arguments` keep the values the call is built
/// from; the record holds neither, so nothing can replace them.
#[test]
fn a_function_calls_typed_members_keep_their_typed_values() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f",
         "arguments": "{\"a\":1}"}]});

    // Act
    let req = parse(body);

    // Assert
    let calls = req.messages[0].tool_calls.as_ref().expect("tool calls");
    assert_eq!(calls[0]["id"], json!("call_1"));
    assert_eq!(calls[0]["function"]["arguments"], json!("{\"a\":1}"));
    let members = &req.routectl_internal.responses_item_members.function_call["call_1"].members;
    assert_eq!(members, &map(json!({"id": "fc_1"})));
}

/// An item with nothing outside its modeled members records nothing, and
/// one with no identity to be emitted under is recorded unkeyed.
#[test]
fn item_members_without_an_identity_are_recorded_unkeyed() {
    // Arrange
    let body = json!({"model": "m", "input": [
        {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
        {"type": "reasoning", "summary": [{"type": "summary_text", "text": "t"}],
         "status": "completed"}]});

    // Act
    let req = parse(body);

    // Assert
    let record = &req.routectl_internal.responses_item_members;
    assert!(record.function_call.is_empty(), "{record:?}");
    assert!(record.reasoning.is_empty(), "{record:?}");
    assert_eq!(record.unkeyed.len(), 1, "{record:?}");
    assert_eq!(record.unkeyed[0].client_pointer, "/input/1");
}
