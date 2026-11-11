use serde_json::{Map, Value, json};

use super::*;
use crate::schema::{Message, MessageContent, Role};

#[test]
fn message_members_equal_the_serialized_keys_of_a_populated_message() {
    // Arrange
    let message = Message {
        role: Role::Assistant,
        content: MessageContent::Text("c".into()),
        reasoning: Some("r".into()),
        reasoning_details: vec![
            serde_json::from_value(json!({"type": "reasoning.text", "text": "t"})).unwrap(),
        ],
        name: Some("n".into()),
        tool_call_id: Some("i".into()),
        tool_calls: Some(vec![json!({})]),
        refusal: Some("no".into()),
        extras: Map::new(),
    };

    // Act
    let wire = serde_json::to_value(&message).unwrap();

    // Assert
    let mut serialized: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut typed = MESSAGE_MEMBERS.to_vec();
    serialized.sort_unstable();
    typed.sort_unstable();
    assert_eq!(typed, serialized);
}

#[test]
fn leftover_message_members_are_split_out_by_position() {
    // (row, message, expected extras)
    let rows = [
        (
            "output_config",
            json!({"role": "user", "content": "hi", "output_config": {"effort": "low"}}),
            json!({"output_config": {"effort": "low"}}),
        ),
        (
            "non_ascii_value",
            json!({"role": "user", "content": "hi", "note": "caf\u{e9}"}),
            json!({"note": "caf\u{e9}"}),
        ),
        (
            "empty_map",
            json!({"role": "assistant", "content": null}),
            json!({}),
        ),
        (
            "reserved_name_collision",
            json!({"role": "user", "content": "typed", "extras": {"content": "nested"}}),
            json!({"extras": {"content": "nested"}}),
        ),
        ("non_object", json!("not a message"), json!({})),
    ];
    let mut messages: Vec<Value> = rows.iter().map(|(_, m, _)| m.clone()).collect();

    // Act
    let extras = split_message_extras(&mut messages);

    // Assert
    assert_eq!(extras.len(), rows.len());
    for (((row, _, expected), split), remaining) in rows.iter().zip(&extras).zip(&messages) {
        assert_eq!(&Value::Object(split.clone()), expected, "row `{row}`");
        if let Value::Object(obj) = remaining {
            assert!(
                obj.keys().all(|k| MESSAGE_MEMBERS.contains(&k.as_str())),
                "row `{row}` left {remaining}"
            );
        }
    }
}

#[test]
fn split_messages_still_parse_with_their_typed_members() {
    // Arrange
    let mut messages =
        vec![json!({"role": "user", "content": "typed", "extras": {"content": "x"}, "id": "m1"})];

    // Act
    let extras = split_message_extras(&mut messages);
    let parsed: Message = serde_json::from_value(messages.remove(0)).unwrap();

    // Assert
    assert!(matches!(parsed.content, MessageContent::Text(ref t) if t == "typed"));
    assert_eq!(extras[0].len(), 2);
    assert_eq!(extras[0]["id"], "m1");
}

#[test]
fn an_empty_message_list_yields_no_maps() {
    assert!(split_message_extras(&mut []).is_empty());
}
