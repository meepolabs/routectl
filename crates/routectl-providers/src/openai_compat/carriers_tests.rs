use routectl_core::{
    CacheControl, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent,
    RequestProvenance, Role,
};
use serde_json::{Map, Value, json};

use crate::openai_compat::HistoryReasoning;
use crate::openai_compat::dialect::ReasoningDialect;
use crate::openai_compat::request::normalize;

const BILLING_BLOCK: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=c4rrierfp";

fn members(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn text_part(text: &str, extras: Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn message(role: Role, content: MessageContent, extras: Map<String, Value>) -> Message {
    Message {
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
        extras,
    }
}

/// A user turn whose message carries `sentinel_message` and whose text part
/// carries `sentinel_part`.
fn carried_user_turn() -> Message {
    message(
        Role::User,
        MessageContent::Parts(vec![text_part(
            "hi",
            members(&[("sentinel_part", json!({"note": "caf\u{e9}"}))]),
        )]),
        members(&[("sentinel_message", json!(1))]),
    )
}

fn request(provenance: RequestProvenance, messages: Vec<Message>) -> ChatRequest {
    let mut req = ChatRequest {
        model: "gpt-4o".into(),
        messages: messages.into(),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req
}

fn wire(req: &ChatRequest) -> Value {
    normalize(
        "test",
        req,
        ReasoningDialect::OpenAi,
        HistoryReasoning::Auto,
        None,
        false,
    )
    .expect("normalize")
}

#[test]
fn a_chat_request_emits_message_and_part_carriers_on_their_own_slots() {
    for provenance in [RequestProvenance::OpenaiIngress, RequestProvenance::Library] {
        let req = request(provenance, vec![carried_user_turn()]);

        let body = wire(&req);

        let turn = &body["messages"][0];
        assert_eq!(turn["sentinel_message"], json!(1), "{provenance:?}: {body}");
        assert_eq!(
            turn["content"][0]["sentinel_part"],
            json!({"note": "caf\u{e9}"}),
            "{provenance:?}: {body}"
        );
        assert!(
            turn.get("sentinel_part").is_none(),
            "{provenance:?}: {body}"
        );
    }
}

#[test]
fn another_dialects_carriers_never_reach_this_wire() {
    for provenance in [
        RequestProvenance::AnthropicIngress,
        RequestProvenance::ResponsesIngress,
    ] {
        let carried = request(provenance, vec![carried_user_turn()]);
        let mut bare_turn = carried_user_turn();
        bare_turn.extras = Map::new();
        bare_turn.content = MessageContent::Parts(vec![text_part("hi", Map::new())]);
        let bare = request(provenance, vec![bare_turn]);

        assert_eq!(wire(&carried), wire(&bare), "{provenance:?}");
    }
}

#[test]
fn reserved_names_in_carriers_never_override_or_appear() {
    // Arrange: carried members named like typed, managed and dialect-written
    // members, each with its typed value absent or set.
    let part = ContentPart::Known(KnownContentPart::Text {
        text: "typed text".into(),
        citations: None,
        cache_control: Some(CacheControl::ephemeral_5m()),
        extras: members(&[
            ("type", json!("image_url")),
            ("text", json!("carried text")),
            ("cache_control", json!({"type": "ephemeral"})),
            ("citations", json!([1])),
            ("part_note", json!(2)),
        ]),
    });
    let turn = message(
        Role::User,
        MessageContent::Parts(vec![part]),
        members(&[
            ("role", json!("system")),
            ("content", json!("carried content")),
            ("name", json!("forged")),
            ("reasoning_content", json!("forged")),
            ("message_note", json!(1)),
        ]),
    );
    let req = request(RequestProvenance::OpenaiIngress, vec![turn]);

    // Act
    let body = wire(&req);

    // Assert
    let turn = &body["messages"][0];
    assert_eq!(turn["message_note"], json!(1), "premise: merged: {body}");
    assert_eq!(turn["role"], json!("user"), "{body}");
    assert!(turn.get("name").is_none(), "{body}");
    assert!(turn.get("reasoning_content").is_none(), "{body}");
    let part = &turn["content"][0];
    assert_eq!(part["part_note"], json!(2), "premise: merged: {body}");
    assert_eq!(part["type"], json!("text"), "{body}");
    assert_eq!(part["text"], json!("typed text"), "{body}");
    assert!(part.get("cache_control").is_none(), "{body}");
    assert!(part.get("citations").is_none(), "{body}");
}

#[test]
fn a_reasoning_content_carrier_never_shadows_the_dialect_rename() {
    let mut turn = message(
        Role::Assistant,
        MessageContent::Text("answer".into()),
        members(&[("reasoning_content", json!("forged"))]),
    );
    turn.reasoning = Some("typed reasoning".into());
    let req = request(
        RequestProvenance::OpenaiIngress,
        vec![
            message(Role::User, MessageContent::Text("q".into()), Map::new()),
            turn,
        ],
    );

    let body = normalize(
        "test",
        &req,
        ReasoningDialect::DeepSeek,
        HistoryReasoning::Preserve,
        None,
        false,
    )
    .expect("normalize");

    assert_eq!(
        body["messages"][1]["reasoning_content"],
        json!("typed reasoning"),
        "{body}"
    );
}

#[test]
fn carriers_survive_the_system_turn_reserialization() {
    // A system turn holding the billing block makes the egress re-serialize
    // `messages`; the carried members must come back on that pass too.
    let system = message(
        Role::System,
        MessageContent::Parts(vec![
            text_part(BILLING_BLOCK, Map::new()),
            text_part("be brief", members(&[("sentinel_system_part", json!(3))])),
        ]),
        members(&[("sentinel_system", json!(4))]),
    );
    let req = request(
        RequestProvenance::OpenaiIngress,
        vec![system, carried_user_turn()],
    );

    let body = wire(&req);

    let messages = &body["messages"];
    assert!(!body.to_string().contains("c4rrierfp"), "premise: withheld");
    assert_eq!(messages[0]["sentinel_system"], json!(4), "{body}");
    assert_eq!(
        messages[0]["content"][0]["sentinel_system_part"],
        json!(3),
        "{body}"
    );
    assert_eq!(messages[1]["sentinel_message"], json!(1), "{body}");
    assert_eq!(
        messages[1]["content"][0]["sentinel_part"],
        json!({"note": "caf\u{e9}"})
    );
}

#[test]
fn a_carrier_free_chat_request_serializes_exactly_as_before() {
    let turn = message(
        Role::User,
        MessageContent::Parts(vec![text_part("hi", Map::new())]),
        Map::new(),
    );
    let messages = vec![turn];
    let mut slots = serde_json::to_value(&messages).unwrap();
    let before = serde_json::to_vec(&slots).unwrap();

    super::emit(&mut slots, &messages);

    assert_eq!(serde_json::to_vec(&slots).unwrap(), before);
}
