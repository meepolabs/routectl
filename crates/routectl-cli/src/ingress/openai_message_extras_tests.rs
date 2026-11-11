//! The Chat ingress keeps each message's and each content part's unmodeled
//! members on that node's carrier, after the rewrites that fold a dialect
//! spelling into a modeled member.

use axum::http::HeaderMap;
use routectl_core::{ChatRequest, ContentPart, KnownContentPart, MessageContent, Role};
use serde_json::{Map, Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::openai::OpenAiIngress;

fn parse(messages: Value) -> ChatRequest {
    OpenAiIngress
        .parse_request_value(
            &HeaderMap::new(),
            json!({"model": "m", "messages": messages}),
        )
        .expect("parses")
}

fn part_extras(content: &MessageContent, index: usize) -> &Map<String, Value> {
    match content {
        MessageContent::Parts(parts) => match &parts[index] {
            ContentPart::Known(KnownContentPart::Text { extras, .. }) => extras,
            other => panic!("expected a text part, got {other:?}"),
        },
        other => panic!("expected parts, got {other:?}"),
    }
}

#[test]
fn message_and_part_members_land_on_their_own_carriers() {
    let req = parse(json!([
        {"role": "user", "note": "caf\u{e9}", "content": [{"type": "text", "text": "hi", "caller": 1}]},
        {"role": "assistant", "content": "ok"}
    ]));

    assert_eq!(
        req.messages[0].extras.get("note"),
        Some(&json!("caf\u{e9}"))
    );
    assert_eq!(
        part_extras(&req.messages[0].content, 0).get("caller"),
        Some(&json!(1))
    );
    assert!(req.messages[0].extras.get("caller").is_none());
    assert!(req.messages[1].extras.is_empty());
}

#[test]
fn folded_spellings_are_modeled_members_not_carried_ones() {
    let req = parse(json!([
        {"role": "developer", "content": [{"type": "text", "text": "be brief"}], "note": 1},
        {"role": "user", "content": "q"},
        {"role": "assistant", "content": "a", "reasoning_content": "r"}
    ]));

    let developer = &req.messages[0];
    assert!(
        matches!(developer.role, Role::System),
        "rewritten before the split"
    );
    assert_eq!(developer.extras.get("note"), Some(&json!(1)));
    assert!(developer.extras.get("role").is_none());
    let assistant = &req.messages[2];
    assert_eq!(assistant.reasoning.as_deref(), Some("r"));
    assert!(assistant.extras.is_empty(), "{:?}", assistant.extras);
}
