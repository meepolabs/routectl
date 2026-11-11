//! Members a response content block carries that the wire types do not
//! model ride the canonical part's carrier, and the response is stamped as
//! parsed from an Anthropic body. Bedrock Invoke shares the parser.

use routectl_core::wire_check::IngressDialect;
use routectl_core::{ChatResponse, ContentPart, KnownContentPart, MessageContent};
use serde_json::{Map, Value, json};

use super::normalize;

fn response_with_blocks(content: Value) -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": content,
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

fn part_members(resp: &ChatResponse) -> Vec<(String, Map<String, Value>)> {
    let MessageContent::Parts(parts) = &resp.choices[0].message.content else {
        panic!("expected parts, got {:?}", resp.choices[0].message.content);
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Known(
                KnownContentPart::Text { extras, .. }
                | KnownContentPart::ToolUse { extras, .. }
                | KnownContentPart::Image { extras, .. }
                | KnownContentPart::Document { extras, .. },
            ) => (part_kind(part), extras.clone()),
            other => panic!("unexpected part {other:?}"),
        })
        .collect()
}

fn part_kind(part: &ContentPart) -> String {
    serde_json::to_value(part).expect("part serializes")["type"]
        .as_str()
        .expect("type tag")
        .to_string()
}

fn members(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

#[test]
fn each_typed_block_keeps_its_unmodeled_members_on_its_part() {
    // Arrange
    let raw = response_with_blocks(json!([
        {"type": "text", "text": "a", "note": "caf\u{e9}"},
        {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"},
         "alt": "x"},
        {"type": "document", "source": {"type": "text", "data": "d"}, "title": "t",
         "page_hint": 2},
        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {},
         "caller": {"type": "direct"}, "estimated_tokens": 12},
    ]));

    // Act
    let resp = normalize("test", raw).expect("normalizes");

    // Assert
    assert_eq!(
        part_members(&resp),
        vec![
            ("text".into(), members(json!({"note": "caf\u{e9}"}))),
            ("image".into(), members(json!({"alt": "x"}))),
            ("document".into(), members(json!({"page_hint": 2}))),
            (
                "tool_use".into(),
                members(json!({"caller": {"type": "direct"}, "estimated_tokens": 12}))
            ),
        ]
    );
}

#[test]
fn typed_and_managed_members_never_land_in_the_carrier() {
    // Arrange
    let raw = response_with_blocks(json!([
        {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": 1},
         "cache_control": {"type": "ephemeral"}},
    ]));

    // Act
    let resp = normalize("test", raw).expect("normalizes");

    // Assert
    assert_eq!(part_members(&resp), vec![("tool_use".into(), Map::new())]);
}

fn stamped_dialect(resp: ChatResponse) -> Option<IngressDialect> {
    resp.upstream_meta.and_then(|meta| meta.wire_dialect)
}

#[test]
fn a_parsed_response_is_stamped_with_the_anthropic_dialect() {
    // Act
    let resp = normalize(
        "test",
        response_with_blocks(json!([{"type": "text", "text": "ok"}])),
    )
    .expect("normalizes");

    // Assert
    assert_eq!(stamped_dialect(resp), Some(IngressDialect::Anthropic));
}

/// Bedrock Invoke speaks the Messages wire and parses through the same
/// normalizer, so its responses carry the same stamp.
#[cfg(feature = "bedrock")]
#[test]
fn a_bedrock_invoke_response_is_stamped_with_the_anthropic_dialect() {
    // Act
    let resp = crate::bedrock::invoke::normalize_response(
        "test",
        response_with_blocks(json!([{"type": "text", "text": "ok"}])),
    )
    .expect("normalizes");

    // Assert
    assert_eq!(stamped_dialect(resp), Some(IngressDialect::Anthropic));
}
