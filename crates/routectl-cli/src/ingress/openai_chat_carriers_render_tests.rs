//! The Chat render returns the members an upstream Chat Completions body
//! carried that canonical does not model (on choice messages, their typed
//! parts and stream usage) only when the response was parsed from that
//! dialect.

use routectl_core::schema::{Choice, UsageDelta};
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    ChatChunk, ChatResponse, ContentPart, KnownContentPart, Message, MessageContent, Role,
    UpstreamMeta,
};
use serde_json::{Map, Value, json};

use super::openai::OpenAiIngress;
use super::{IngressAdapter, StreamRequestContext};

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("expected an object, got {other}"),
    }
}

fn stamped(dialect: IngressDialect) -> UpstreamMeta {
    UpstreamMeta::default().with_wire_dialect(dialect)
}

fn carrying_response(dialect: Option<IngressDialect>) -> ChatResponse {
    let part = KnownContentPart::Text {
        text: "hi".into(),
        cache_control: None,
        citations: None,
        extras: object(json!({"part_note": 1, "text": "never"})),
    };
    let message = Message {
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(part)]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
        extras: object(json!({"annotations": [], "note": "caf\u{e9}", "role": "never"})),
    };
    ChatResponse {
        id: "c1".into(),
        model: "m".into(),
        created: 1,
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        upstream_meta: dialect.map(stamped),
        ..ChatResponse::default()
    }
}

fn render(resp: ChatResponse) -> Value {
    serde_json::from_slice(&OpenAiIngress.render_response(resp).expect("renders")).expect("json")
}

fn usage_chunk(dialect: Option<IngressDialect>) -> ChatChunk {
    ChatChunk {
        usage: Some(UsageDelta {
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            total_tokens: Some(12),
            extras: object(json!({"cost": 0.5, "prompt_tokens": 999})),
            ..UsageDelta::default()
        }),
        upstream_meta: dialect.map(stamped),
        ..ChatChunk::default()
    }
}

fn render_usage_chunk(chunk: ChatChunk) -> Value {
    let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
    let events = OpenAiIngress
        .render_chunk(chunk, state.as_mut())
        .expect("renders");
    let [event] = events.as_slice() else {
        panic!("one event per chunk, got {}", events.len());
    };
    let rendered: Value = serde_json::from_str(&event.data).expect("json");
    rendered["usage"].clone()
}

#[test]
fn a_chat_stamped_response_gets_its_message_and_part_members_back() {
    let body = render(carrying_response(Some(IngressDialect::Chat)));

    let message = &body["choices"][0]["message"];
    assert_eq!(message["note"], json!("caf\u{e9}"));
    assert_eq!(message["annotations"], json!([]));
    assert_eq!(message["content"][0]["part_note"], json!(1));
}

#[test]
fn a_carried_name_the_node_types_never_replaces_the_typed_value() {
    let body = render(carrying_response(Some(IngressDialect::Chat)));

    let message = &body["choices"][0]["message"];
    assert_eq!(message["role"], json!("assistant"));
    assert_eq!(message["content"][0]["text"], json!("hi"));
}

#[test]
fn message_and_part_members_from_another_dialect_or_unstamped_are_not_rendered() {
    for (row, dialect) in [
        ("unstamped", None),
        ("anthropic", Some(IngressDialect::Anthropic)),
        ("responses", Some(IngressDialect::Responses)),
    ] {
        let body = render(carrying_response(dialect));

        let message = &body["choices"][0]["message"];
        assert!(message.get("note").is_none(), "{row}: {body}");
        assert!(message.get("annotations").is_none(), "{row}: {body}");
        assert!(
            message["content"][0].get("part_note").is_none(),
            "{row}: {body}"
        );
    }
}

#[test]
fn a_chat_stamped_usage_chunk_renders_its_members_with_typed_counts_winning() {
    let usage = render_usage_chunk(usage_chunk(Some(IngressDialect::Chat)));

    assert_eq!(usage["cost"], json!(0.5));
    assert_eq!(usage["prompt_tokens"], json!(10));
}

#[test]
fn usage_members_on_an_unstamped_or_foreign_chunk_are_not_rendered() {
    for (row, dialect) in [
        ("unstamped", None),
        ("anthropic", Some(IngressDialect::Anthropic)),
        ("responses", Some(IngressDialect::Responses)),
    ] {
        let usage = render_usage_chunk(usage_chunk(dialect));

        assert!(usage.get("cost").is_none(), "{row}: {usage}");
        assert_eq!(usage["prompt_tokens"], json!(10), "{row}");
    }
}
