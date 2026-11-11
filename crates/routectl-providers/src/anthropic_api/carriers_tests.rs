use routectl_core::{
    ChatRequest, KnownContentPart, Message, MessageContent, RequestProvenance, Role,
};
use serde_json::{Map, Value, json};

use super::Carriers;

fn carried(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn tool_turn(extras: Map<String, Value>) -> Message {
    Message {
        role: Role::Tool,
        content: MessageContent::Text("ok".into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some("t".into()),
        tool_calls: None,
        refusal: None,
        extras,
    }
}

#[test]
fn emits_for_the_anthropic_ingress_and_a_library_caller_only() {
    let rows = [
        (RequestProvenance::AnthropicIngress, Carriers::Emit),
        (RequestProvenance::Library, Carriers::Emit),
        (RequestProvenance::OpenaiIngress, Carriers::Withhold),
        (RequestProvenance::ResponsesIngress, Carriers::Withhold),
    ];
    for (provenance, expected) in rows {
        let mut req = ChatRequest::default();
        req.routectl_internal.provenance = provenance;

        assert_eq!(Carriers::for_request(&req), expected, "{provenance:?}");
    }
}

#[test]
fn a_withheld_request_emits_no_member_at_any_level() {
    let part = KnownContentPart::Text {
        text: "t".into(),
        citations: None,
        cache_control: None,
        extras: carried(&[("caller", json!("x"))]),
    };
    let message = tool_turn(carried(&[("output_config", json!({}))]));

    assert!(Carriers::Withhold.part(&part).is_empty());
    assert!(Carriers::Withhold.message(&message).is_empty());
    assert_eq!(Carriers::Emit.part(&part).len(), 1, "control");
    assert_eq!(Carriers::Emit.message(&message).len(), 1, "control");
}

#[test]
fn reserved_names_are_skipped_even_when_the_typed_value_is_absent() {
    let part = KnownContentPart::Text {
        text: "t".into(),
        citations: None,
        cache_control: None,
        extras: carried(&[
            ("type", json!("image")),
            ("text", json!("forged")),
            ("cache_control", json!({"type": "ephemeral"})),
            ("citations", json!([])),
            ("caller", json!("kept")),
        ]),
    };

    let emitted = Carriers::Emit.part(&part);

    assert_eq!(emitted, carried(&[("caller", json!("kept"))]));
}

#[test]
fn a_joined_run_unions_members_with_the_earlier_turn_winning() {
    let first = tool_turn(carried(&[("tag", json!("first")), ("a", json!(1))]));
    let second = tool_turn(carried(&[
        ("tag", json!("second")),
        ("a", json!(1)),
        ("b", json!(2)),
    ]));

    let (union, replaced) = Carriers::Emit.joined_messages(&[&first, &second], 4);

    assert_eq!(
        union,
        carried(&[("tag", json!("first")), ("a", json!(1)), ("b", json!(2))])
    );
    assert_eq!(
        replaced,
        vec![(5, "tag".to_owned())],
        "an equal value replaces nothing"
    );
}
