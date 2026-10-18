//! Tests for canonical tool call / tool result pairing validation.

use serde_json::{Map, Value, json};

use super::*;

fn message(role: Role, content: MessageContent) -> Message {
    Message {
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn user(text: &str) -> Message {
    message(Role::User, MessageContent::Text(text.into()))
}

fn assistant(text: &str) -> Message {
    message(Role::Assistant, MessageContent::Text(text.into()))
}

fn openai_call(id: &str) -> Value {
    json!({"id": id, "type": "function", "function": {"name": "lookup", "arguments": "{}"}})
}

/// Assistant turn carrying calls on the OpenAI `tool_calls` field only.
fn assistant_tool_calls(ids: &[&str]) -> Message {
    Message {
        tool_calls: Some(ids.iter().map(|id| openai_call(id)).collect()),
        ..message(Role::Assistant, MessageContent::Null)
    }
}

/// `Role::Tool` result turn (OpenAI shape).
fn tool_turn(id: &str) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        ..message(Role::Tool, MessageContent::Text("result".into()))
    }
}

fn tool_use_part(id: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ToolUse {
        id: id.into(),
        name: "lookup".into(),
        input: json!({}),
        cache_control: None,
    })
}

fn tool_result_part(id: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ToolResult {
        tool_use_id: id.into(),
        content: json!("result"),
        is_error: None,
        cache_control: None,
    })
}

fn other_part(type_tag: &str, key: &str, id: &str) -> ContentPart {
    let mut extras = Map::new();
    extras.insert(key.into(), json!(id));
    ContentPart::Other {
        type_tag: type_tag.into(),
        cache_control: None,
        extras,
    }
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
    })
}

/// Assistant turn carrying calls as Anthropic `tool_use` parts only.
fn assistant_parts(parts: Vec<ContentPart>) -> Message {
    message(Role::Assistant, MessageContent::Parts(parts))
}

/// User turn carrying Anthropic `tool_result` parts.
fn user_parts(parts: Vec<ContentPart>) -> Message {
    message(Role::User, MessageContent::Parts(parts))
}

fn defect_of(messages: &[Message]) -> ToolPairingDefect {
    validate_tool_pairing(messages)
        .expect_err("transcript must be rejected")
        .defect()
}

// ---------------------------------------------------------------------------
// Accepted transcripts
// ---------------------------------------------------------------------------

#[test]
fn accepts_transcript_without_tools() {
    // Arrange
    let messages = [user("hi"), assistant("hello"), user("bye")];

    // Act
    let result = validate_tool_pairing(&messages);

    // Assert
    assert_eq!(result, Ok(()));
}

#[test]
fn accepts_openai_field_pair() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["call_1"]),
        tool_turn("call_1"),
        assistant("done"),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_anthropic_part_pair_with_trailing_text_in_result_turn() {
    // Arrange: a user turn carrying the tool_result plus trailing text is
    // still the result turn for the preceding call.
    let messages = [
        user("q"),
        assistant_parts(vec![text_part("checking"), tool_use_part("toolu_1")]),
        user_parts(vec![tool_result_part("toolu_1"), text_part("continue")]),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_passthrough_part_pair() {
    // Arrange: tool blocks that fell to the forward-compat catchall.
    let messages = [
        user("q"),
        assistant_parts(vec![other_part("tool_use", "id", "toolu_1")]),
        user_parts(vec![other_part("tool_result", "tool_use_id", "toolu_1")]),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_mixed_carriers_for_one_logical_pair() {
    // Arrange: the call rides a tool_use part AND the tool_calls field under
    // one id; the result rides a Role::Tool turn.
    let call = Message {
        tool_calls: Some(vec![openai_call("call_1")]),
        ..assistant_parts(vec![tool_use_part("call_1")])
    };
    let messages = [user("q"), call, tool_turn("call_1"), assistant("done")];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_openai_call_answered_by_anthropic_result_part() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["call_1"]),
        user_parts(vec![tool_result_part("call_1")]),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_tool_turn_carrying_result_on_both_carriers() {
    // Arrange: a Role::Tool turn naming the same id on its tool_call_id and
    // on a tool_result part is one result, not a duplicate.
    let result = Message {
        tool_call_id: Some("call_1".into()),
        ..message(
            Role::Tool,
            MessageContent::Parts(vec![tool_result_part("call_1")]),
        )
    };
    let messages = [user("q"), assistant_tool_calls(&["call_1"]), result];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_parallel_results_in_call_order() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a", "b"]),
        tool_turn("a"),
        tool_turn("b"),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_parallel_results_in_reverse_order() {
    // Arrange
    let messages = [
        user("q"),
        assistant_parts(vec![tool_use_part("a"), tool_use_part("b")]),
        user_parts(vec![tool_result_part("b"), tool_result_part("a")]),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn accepts_consecutive_tool_segments() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        tool_turn("a"),
        assistant_tool_calls(&["b"]),
        tool_turn("b"),
        assistant("done"),
    ];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

/// A `Role::Tool` turn whose `tool_call_id` is `id` (`None` = absent).
fn tool_turn_with(id: Option<&str>) -> Message {
    Message {
        tool_call_id: id.map(Into::into),
        ..message(Role::Tool, MessageContent::Text("r".into()))
    }
}

/// A `tool_result` passthrough block with no `tool_use_id` key at all.
fn other_result_without_id() -> ContentPart {
    ContentPart::Other {
        type_tag: "tool_result".into(),
        cache_control: None,
        extras: Map::new(),
    }
}

/// A `tool_result` passthrough block whose `tool_use_id` is `id`.
fn other_result_with_id(id: Value) -> ContentPart {
    let mut extras = Map::new();
    extras.insert("tool_use_id".into(), id);
    ContentPart::Other {
        type_tag: "tool_result".into(),
        cache_control: None,
        extras,
    }
}

/// A `tool_calls` entry with no `id` key at all.
fn assistant_call_without_id() -> Message {
    Message {
        tool_calls: Some(vec![
            json!({"type": "function", "function": {"name": "lookup", "arguments": "{}"}}),
        ]),
        ..message(Role::Assistant, MessageContent::Null)
    }
}

#[test]
fn an_empty_id_call_with_no_result_demands_none() {
    // An empty-id call has no id a result could name, so leaving it
    // unanswered breaks no pairing rule; what comes after it still pairs.
    let rows: [(&str, Vec<Message>); 5] = [
        ("trailing", vec![user("q"), assistant_tool_calls(&[""])]),
        (
            "before a semantic turn",
            vec![user("q"), assistant_tool_calls(&[""]), user("next")],
        ),
        (
            "an absent id before a semantic turn",
            vec![user("q"), assistant_call_without_id(), assistant("done")],
        ),
        (
            "beside an answered call",
            vec![user("q"), assistant_tool_calls(&["", "a"]), tool_turn("a")],
        ),
        (
            "twice in one turn",
            vec![
                user("q"),
                assistant_parts(vec![tool_use_part(""), tool_use_part("")]),
            ],
        ),
    ];
    for (name, messages) in rows {
        // Act
        let result = validate_tool_pairing(&messages);

        // Assert
        assert_eq!(result, Ok(()), "{name}");
    }
}

#[test]
fn a_tool_turn_with_no_tool_call_id_pairs_on_its_result_part() {
    // Arrange: the result part names the call; the absent field adds no
    // second, empty-id result.
    let result = Message {
        tool_call_id: None,
        ..message(
            Role::Tool,
            MessageContent::Parts(vec![tool_result_part("a")]),
        )
    };
    let messages = [user("q"), assistant_tool_calls(&["a"]), result];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn an_empty_id_result_with_no_call_turn_before_it_is_left_to_the_egress() {
    // Arrange: no call segment is open, so no call awaits this result.
    let messages = [user("q"), tool_turn_with(Some(""))];

    // Act + Assert
    assert_eq!(validate_tool_pairing(&messages), Ok(()));
}

#[test]
fn rejects_an_empty_id_result_inside_a_call_segment() {
    // Every result carrier, with the id empty or absent, answering either an
    // empty-id call or a call that has an id.
    let rows: [(&str, Vec<Message>, usize, Option<usize>); 9] = [
        (
            "tool turn, empty id, answering an empty-id call",
            vec![
                user("q"),
                assistant_tool_calls(&[""]),
                tool_turn_with(Some("")),
            ],
            2,
            None,
        ),
        (
            "tool turn, absent id, answering an absent-id call",
            vec![user("q"), assistant_call_without_id(), tool_turn_with(None)],
            2,
            None,
        ),
        (
            "tool result part, empty id, answering an empty-id tool_use part",
            vec![
                user("q"),
                assistant_parts(vec![tool_use_part("")]),
                user_parts(vec![text_part("x"), tool_result_part("")]),
            ],
            2,
            Some(1),
        ),
        (
            "passthrough result, empty id",
            vec![
                user("q"),
                assistant_parts(vec![other_part("tool_use", "id", "")]),
                user_parts(vec![other_part("tool_result", "tool_use_id", "")]),
            ],
            2,
            Some(0),
        ),
        (
            "passthrough result, absent id",
            vec![
                user("q"),
                assistant_parts(vec![other_part("tool_use", "id", "")]),
                user_parts(vec![other_result_without_id()]),
            ],
            2,
            Some(0),
        ),
        (
            "passthrough result, non-string id, which names no call",
            vec![
                user("q"),
                assistant_parts(vec![other_part("tool_use", "id", "")]),
                user_parts(vec![other_result_with_id(json!(7))]),
            ],
            2,
            Some(0),
        ),
        (
            "tool turn, empty id, beside a call that has an id",
            vec![
                user("q"),
                assistant_tool_calls(&["a"]),
                tool_turn("a"),
                tool_turn_with(Some("")),
            ],
            3,
            None,
        ),
        (
            "tool turn, empty id, before the answer to a call that has an id",
            vec![
                user("q"),
                assistant_tool_calls(&["a"]),
                tool_turn_with(Some("")),
                tool_turn("a"),
            ],
            2,
            None,
        ),
        (
            "tool turn, empty id, in a later segment",
            vec![
                user("q"),
                assistant_tool_calls(&["a"]),
                tool_turn("a"),
                assistant("t"),
                assistant_tool_calls(&[""]),
                tool_turn_with(Some("")),
            ],
            5,
            None,
        ),
    ];
    for (name, messages, message_index, item_index) in rows {
        // Act
        let err = validate_tool_pairing(&messages).expect_err(name);

        // Assert
        assert_eq!(err.defect(), ToolPairingDefect::EmptyResultId, "{name}");
        assert_eq!(err.message_index(), message_index, "{name}");
        assert_eq!(err.item_index(), item_index, "{name}");
    }
}

#[test]
fn tool_names_do_not_pair() {
    // Arrange: same tool name, different ids -> the result is an orphan.
    let messages = [
        user("q"),
        assistant_tool_calls(&["call_1"]),
        tool_turn("call_2"),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::OrphanResult);
}

// ---------------------------------------------------------------------------
// Rejected transcripts, one per defect class
// ---------------------------------------------------------------------------

#[test]
fn rejects_orphan_result() {
    // Arrange
    let messages = [user("q"), tool_turn("call_1")];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("orphan result");

    // Assert
    assert_eq!(err.defect(), ToolPairingDefect::OrphanResult);
    assert_eq!(err.message_index(), 1);
    assert_eq!(err.item_index(), None);
}

#[test]
fn rejects_result_before_call() {
    // Arrange
    let messages = [
        user("q"),
        tool_turn("call_1"),
        assistant_tool_calls(&["call_1"]),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::ResultBeforeCall);
}

#[test]
fn rejects_duplicate_call_id_within_one_turn() {
    // Arrange
    let messages = [
        user("q"),
        assistant_parts(vec![tool_use_part("a"), tool_use_part("a")]),
        user_parts(vec![tool_result_part("a")]),
    ];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("duplicate call id");

    // Assert
    assert_eq!(err.defect(), ToolPairingDefect::DuplicateCallId);
    assert_eq!(err.message_index(), 1);
    assert_eq!(err.item_index(), Some(1));
}

#[test]
fn rejects_call_id_reused_by_a_later_turn() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        tool_turn("a"),
        assistant_tool_calls(&["a"]),
        tool_turn("a"),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::DuplicateCallId);
}

#[test]
fn rejects_duplicate_result() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        tool_turn("a"),
        tool_turn("a"),
    ];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("duplicate result");

    // Assert
    assert_eq!(err.defect(), ToolPairingDefect::DuplicateResult);
    assert_eq!(err.message_index(), 3);
}

#[test]
fn rejects_duplicate_result_parts_within_one_turn() {
    // Arrange
    let messages = [
        user("q"),
        assistant_parts(vec![tool_use_part("a")]),
        user_parts(vec![tool_result_part("a"), tool_result_part("a")]),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::DuplicateResult);
}

#[test]
fn rejects_cross_segment_result() {
    // Arrange: "a" was answered and its segment closed by the text turn;
    // a result for it inside the next segment is a cross-segment match.
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        tool_turn("a"),
        assistant("thinking"),
        assistant_tool_calls(&["b"]),
        tool_turn("b"),
        tool_turn("a"),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::CrossSegmentResult);
}

#[test]
fn rejects_semantic_turn_between_call_and_result() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        user("interrupt"),
        tool_turn("a"),
    ];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("interrupted");

    // Assert: the error points at the pending CALL, not the interrupter.
    assert_eq!(err.defect(), ToolPairingDefect::InterruptedCalls);
    assert_eq!(err.message_index(), 1);
    assert_eq!(err.item_index(), Some(0));
}

#[test]
fn rejects_partial_parallel_results_before_semantic_turn() {
    // Arrange
    let messages = [
        user("q"),
        assistant_tool_calls(&["a", "b"]),
        tool_turn("a"),
        assistant("done"),
    ];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("interrupted");

    // Assert
    assert_eq!(err.defect(), ToolPairingDefect::InterruptedCalls);
    assert_eq!(err.item_index(), Some(1));
}

#[test]
fn rejects_new_call_turn_while_calls_pending() {
    // Arrange: a second call turn arrives before the first is answered.
    let messages = [
        user("q"),
        assistant_tool_calls(&["a"]),
        assistant_tool_calls(&["b"]),
        tool_turn("b"),
    ];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::InterruptedCalls);
}

#[test]
fn rejects_new_calls_in_a_result_turn_while_calls_pending() {
    // Arrange: one turn answers "a", leaves "b" pending, and opens "c".
    let answer_and_call = user_parts(vec![tool_result_part("a"), tool_use_part("c")]);
    let messages = [
        user("q"),
        assistant_tool_calls(&["a", "b"]),
        answer_and_call,
        tool_turn("b"),
        tool_turn("c"),
    ];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("interrupted");

    // Assert: the pending call "b" is reported.
    assert_eq!(err.defect(), ToolPairingDefect::InterruptedCalls);
    assert_eq!(err.message_index(), 1);
    assert_eq!(err.item_index(), Some(1));
}

#[test]
fn rejects_trailing_pending_call() {
    // Arrange
    let messages = [user("q"), assistant_tool_calls(&["a"])];

    // Act
    let err = validate_tool_pairing(&messages).expect_err("trailing");

    // Assert
    assert_eq!(err.defect(), ToolPairingDefect::TrailingPendingCalls);
    assert_eq!(err.message_index(), 1);
}

#[test]
fn rejects_duplicate_representation_on_one_carrier() {
    // Arrange: two tool_calls entries under one id in one turn.
    let messages = [user("q"), assistant_tool_calls(&["a", "a"]), tool_turn("a")];

    // Act + Assert
    assert_eq!(defect_of(&messages), ToolPairingDefect::DuplicateCallId);
}

// ---------------------------------------------------------------------------
// Error surface
// ---------------------------------------------------------------------------

#[test]
fn error_display_is_bounded_and_carries_no_id_or_content() {
    // Arrange: an id and a result body that must never be echoed.
    let secret_id = "call_SECRET_".repeat(500);
    let orphan = Message {
        tool_call_id: Some(secret_id.clone()),
        ..message(Role::Tool, MessageContent::Text("TOOL_OUTPUT_BODY".into()))
    };
    let messages = [user("q"), orphan];

    // Act
    let rendered = validate_tool_pairing(&messages)
        .expect_err("orphan")
        .to_string();

    // Assert
    assert!(!rendered.contains("SECRET"), "{rendered}");
    assert!(!rendered.contains("TOOL_OUTPUT_BODY"), "{rendered}");
    assert!(rendered.len() < 160, "{rendered}");
    assert!(rendered.contains("messages[1].tool_call_id"), "{rendered}");
    assert!(rendered.contains("(orphan_result)"), "{rendered}");
}

#[test]
fn error_display_names_the_item_carrier() {
    // Arrange
    let messages = [
        user("q"),
        assistant_parts(vec![text_part("x"), tool_use_part("a")]),
    ];

    // Act
    let rendered = validate_tool_pairing(&messages)
        .expect_err("trailing")
        .to_string();

    // Assert
    assert!(rendered.contains("messages[1].content[1]"), "{rendered}");
    assert!(rendered.contains("(trailing_pending_calls)"), "{rendered}");
}
