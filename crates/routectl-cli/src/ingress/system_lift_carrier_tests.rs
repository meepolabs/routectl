//! A system turn carrying unmodeled members, on the message or on a text
//! part, is not lifted into `req.system`: the system holds text blocks only,
//! so the turn stays at its index with what it carries.

use routectl_core::{ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role};
use serde_json::{Map, Value, json};

use crate::ingress::lift_system_messages;

fn members(key: &str, value: Value) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(key.into(), value);
    map
}

fn turn(role: Role, content: MessageContent) -> Message {
    Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extras: Map::new(),
    }
}

fn user(text: &str) -> Message {
    turn(Role::User, MessageContent::Text(text.into()))
}

fn text_part(text: &str, extras: Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn system_with_message_members(text: &str) -> Message {
    Message {
        extras: members("id", json!("msg_kept")),
        ..turn(Role::System, MessageContent::Text(text.into()))
    }
}

fn system_with_part_members(text: &str) -> Message {
    turn(
        Role::System,
        MessageContent::Parts(vec![text_part(text, members("annotations", json!([])))]),
    )
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

/// Each row is a system turn that must stay at index 1, beside a liftable
/// text-only system turn at index 2 that is the positive control: the lift
/// ran, and it moved only the control.
#[test]
fn a_system_turn_with_carried_members_stays_at_its_index() {
    let rows = [
        (
            "message members only",
            system_with_message_members("keep me"),
        ),
        (
            "text part members only",
            system_with_part_members("keep me"),
        ),
    ];
    for (name, kept) in rows {
        let mut req = request(vec![
            user("hi"),
            kept,
            turn(Role::System, MessageContent::Text("lift me".into())),
        ]);

        let lifted = lift_system_messages(&mut req);

        assert_eq!(lifted.indices, vec![2], "{name}");
        assert_eq!(
            serde_json::to_value(&req.system).expect("serializes"),
            json!("lift me"),
            "{name}: only the control reached the system"
        );
        assert_eq!(req.messages.len(), 2, "{name}");
        assert!(matches!(req.messages[1].role, Role::System), "{name}");
        assert!(req.messages[1].stays_in_place(), "{name}: carriers kept");
    }
}

/// The carrier-free form of the same turns lifts exactly as before.
#[test]
fn a_carrier_free_system_turn_still_lifts() {
    let rows = [
        (
            "flat text",
            turn(Role::System, MessageContent::Text("a".into())),
        ),
        (
            "text part",
            turn(
                Role::System,
                MessageContent::Parts(vec![text_part("a", Map::new())]),
            ),
        ),
    ];
    for (name, system) in rows {
        let mut req = request(vec![user("hi"), system]);

        let lifted = lift_system_messages(&mut req);

        assert_eq!(lifted.indices, vec![1], "{name}");
        assert_eq!(req.messages.len(), 1, "{name}");
    }
}
