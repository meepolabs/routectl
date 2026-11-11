use serde_json::{Map, Value, json};

use super::{
    CUSTOM_TOOL_KEYS, MESSAGE_KEYS, RequestPrefix, SYSTEM_BLOCK_KEYS, part_typed_keys,
    projection_bytes,
};
use crate::{
    CacheControl, ContentPart, CustomTool, KnownContentPart, Message, MessageContent, Role,
    SystemBlock, SystemContent, ToolDef,
};

fn carried(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn text_part(extras: Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: "hello".into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn message(content: MessageContent, extras: Map<String, Value>) -> Message {
    Message {
        role: Role::User,
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

fn tool(extras: Map<String, Value>) -> CustomTool {
    CustomTool {
        name: "lookup".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
        extras,
    }
}

fn block(extras: Map<String, Value>) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: "be brief".into(),
        cache_control: None,
        citations: None,
        extras,
    }
}

fn projected(bytes: serde_json::Result<Vec<u8>>) -> Value {
    serde_json::from_slice(&bytes.expect("projection serializes")).expect("projection is JSON")
}

#[test]
fn carrier_free_nodes_project_to_their_canonical_bytes() {
    let msg = message(
        MessageContent::Parts(vec![text_part(Map::new())]),
        Map::new(),
    );
    let custom = tool(Map::new());
    let sys = block(Map::new());

    assert_eq!(
        projection_bytes(&msg).unwrap(),
        serde_json::to_vec(&msg).unwrap()
    );
    assert_eq!(
        projection_bytes(&custom).unwrap(),
        serde_json::to_vec(&custom).unwrap()
    );
    assert_eq!(
        projection_bytes(&sys).unwrap(),
        serde_json::to_vec(&sys).unwrap()
    );
}

#[test]
fn each_carrier_level_merges_its_members_into_its_own_node() {
    let msg = message(
        MessageContent::Parts(vec![text_part(carried(&[("caller", json!("é-agent"))]))]),
        carried(&[("output_config", json!({"effort": "low"}))]),
    );
    let custom = tool(carried(&[("eager_input_streaming", json!(true))]));
    let sys = block(carried(&[("origin", json!("hook"))]));

    let msg_value = projected(projection_bytes(&msg));
    let tool_value = projected(projection_bytes(&custom));
    let block_value = projected(projection_bytes(&sys));

    assert_eq!(msg_value["output_config"], json!({"effort": "low"}));
    assert_eq!(msg_value["content"][0]["caller"], json!("é-agent"));
    assert_eq!(tool_value["eager_input_streaming"], json!(true));
    assert_eq!(block_value["origin"], json!("hook"));
}

#[test]
fn reserved_names_never_enter_the_projection() {
    // Arrange: each node carries a member named like one of its typed or
    // managed fields, with that typed value absent.
    let part_extras = carried(&[
        ("type", json!("image")),
        ("cache_control", json!({"type": "ephemeral"})),
        ("citations", json!([1])),
    ]);
    let thinking = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "t".into(),
        signature: None,
        extras: carried(&[("signature", json!("forged")), ("cache_control", json!(1))]),
    });
    let msg = message(
        MessageContent::Parts(vec![text_part(part_extras), thinking]),
        carried(&[("name", json!("forged")), ("reasoning", json!("x"))]),
    );
    let custom = tool(carried(&[
        ("type", json!("custom")),
        ("inputSchema", json!({})),
        ("strict", json!(true)),
    ]));
    let sys = block(carried(&[("cache_control", json!({"type": "ephemeral"}))]));

    // Act / Assert: reserved-only carriers project to the canonical bytes.
    assert_eq!(
        projection_bytes(&msg).unwrap(),
        serde_json::to_vec(&msg).unwrap()
    );
    assert_eq!(
        projection_bytes(&custom).unwrap(),
        serde_json::to_vec(&custom).unwrap()
    );
    assert_eq!(
        projection_bytes(&sys).unwrap(),
        serde_json::to_vec(&sys).unwrap()
    );
}

#[test]
fn a_typed_value_wins_over_a_same_named_carried_member() {
    let mut extras = carried(&[("text", json!("carried")), ("note", json!(1))]);
    extras.insert("cache_control".into(), json!("carried"));
    let part = ContentPart::Known(KnownContentPart::Text {
        text: "typed".into(),
        citations: None,
        cache_control: Some(CacheControl::ephemeral_5m()),
        extras,
    });

    let value = projected(projection_bytes(&part));

    assert_eq!(value["text"], json!("typed"));
    assert_eq!(value["cache_control"]["type"], json!("ephemeral"));
    assert_eq!(value["note"], json!(1));
}

#[test]
fn a_request_prefix_differs_when_only_one_message_carrier_differs() {
    let system = SystemContent::Blocks(vec![block(Map::new())]);
    let tools = vec![ToolDef::Custom(tool(Map::new()))];
    let plain = [message(MessageContent::Text("q".into()), Map::new())];
    let marked = [message(
        MessageContent::Text("q".into()),
        carried(&[("output_config", json!({}))]),
    )];
    let prefix = |messages| RequestPrefix {
        system: Some(&system),
        tools: Some(&tools),
        messages,
    };

    let plain_bytes = projection_bytes(&prefix(&plain)).unwrap();
    let marked_bytes = projection_bytes(&prefix(&marked)).unwrap();
    let marked_again = projection_bytes(&prefix(&marked)).unwrap();

    assert_ne!(plain_bytes, marked_bytes);
    assert_eq!(marked_bytes, marked_again);
    assert_eq!(
        plain_bytes,
        serde_json::to_vec(&(Some(&system), Some(&tools), &plain[..])).unwrap(),
        "a carrier-free prefix keeps its canonical identity"
    );
}

#[test]
fn canonical_serialization_still_omits_every_carrier() {
    let msg = message(
        MessageContent::Parts(vec![text_part(carried(&[("caller", json!("a"))]))]),
        carried(&[("output_config", json!({}))]),
    );

    let canonical = serde_json::to_value(&msg).unwrap();

    assert!(canonical.get("output_config").is_none());
    assert!(canonical["content"][0].get("caller").is_none());
}

fn serialized_keys<T: serde::Serialize>(node: &T) -> Vec<String> {
    let mut keys: Vec<String> = serde_json::to_value(node)
        .unwrap()
        .as_object()
        .expect("node serializes as an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

fn sorted(keys: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
    keys.sort();
    keys
}

fn every_populated_part() -> Vec<KnownContentPart> {
    let cc = || Some(CacheControl::ephemeral_5m());
    let v = || json!({"k": 1});
    let extras = Map::new;
    vec![
        KnownContentPart::Text {
            text: "t".into(),
            citations: Some(v()),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::Image {
            source: v(),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::ImageUrl {
            image_url: v(),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::File {
            file: v(),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::Document {
            source: v(),
            title: Some("t".into()),
            citations: Some(v()),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::ToolUse {
            id: "i".into(),
            name: "n".into(),
            input: v(),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::ToolResult {
            tool_use_id: "i".into(),
            content: v(),
            is_error: Some(true),
            cache_control: cc(),
            extras: extras(),
        },
        KnownContentPart::Thinking {
            thinking: "t".into(),
            signature: Some("s".into()),
            extras: extras(),
        },
        KnownContentPart::RedactedThinking {
            data: "d".into(),
            extras: extras(),
        },
    ]
}

#[test]
fn each_part_reserved_list_matches_its_populated_serialization() {
    for part in every_populated_part() {
        let mut expected: Vec<&str> = part_typed_keys(&part).collect();
        expected.push("type");

        assert_eq!(
            serialized_keys(&part),
            sorted(&expected),
            "{}",
            part.type_tag()
        );
    }
}

#[test]
fn node_reserved_lists_match_their_populated_serialization() {
    let msg = Message {
        role: Role::Assistant,
        content: MessageContent::Text("t".into()),
        reasoning: Some("r".into()),
        reasoning_details: vec![crate::ReasoningDetail {
            kind: crate::ReasoningDetailKind::Text,
            id: None,
            format: None,
            index: None,
            payload: json!({"text": "r"}),
        }],
        name: Some("n".into()),
        tool_call_id: Some("i".into()),
        tool_calls: Some(vec![json!({})]),
        refusal: Some("no".into()),
        extras: Map::new(),
    };
    let custom = CustomTool {
        description: Some("d".into()),
        cache_control: Some(CacheControl::ephemeral_5m()),
        defer_loading: Some(true),
        strict: Some(true),
        type_tag: Some("custom".into()),
        ..tool(Map::new())
    };
    let sys = SystemBlock {
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: Some(json!([])),
        ..block(Map::new())
    };
    let tool_keys: Vec<&str> = CUSTOM_TOOL_KEYS
        .iter()
        .copied()
        .filter(|key| *key != "inputSchema")
        .collect();

    assert_eq!(serialized_keys(&msg), sorted(MESSAGE_KEYS), "Message");
    assert_eq!(serialized_keys(&custom), sorted(&tool_keys), "CustomTool");
    assert_eq!(
        serialized_keys(&sys),
        sorted(SYSTEM_BLOCK_KEYS),
        "SystemBlock"
    );
}

#[test]
fn reserved_names_stay_out_beside_a_mergeable_member() {
    // Arrange: each node also carries one mergeable member, so the merge
    // itself, not the carrier-free shortcut, decides what is skipped.
    let part = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "t".into(),
        signature: None,
        extras: carried(&[
            ("signature", json!("forged")),
            ("cache_control", json!(1)),
            ("type", json!("text")),
            ("note", json!(1)),
        ]),
    });
    let msg = message(
        MessageContent::Parts(vec![part]),
        carried(&[("name", json!("forged")), ("note", json!(2))]),
    );
    let custom = tool(carried(&[
        ("type", json!("custom")),
        ("inputSchema", json!({})),
        ("note", json!(3)),
    ]));
    let sys = block(carried(&[
        ("cache_control", json!({"type": "ephemeral"})),
        ("note", json!(4)),
    ]));

    // Act
    let msg_value = projected(projection_bytes(&msg));
    let tool_value = projected(projection_bytes(&custom));
    let block_value = projected(projection_bytes(&sys));

    // Assert
    let part_value = &msg_value["content"][0];
    assert_eq!(part_value["note"], json!(1), "premise: the part merged");
    assert!(part_value.get("signature").is_none());
    assert!(part_value.get("cache_control").is_none());
    assert_eq!(part_value["type"], json!("thinking"));
    assert_eq!(msg_value["note"], json!(2), "premise: the message merged");
    assert!(msg_value.get("name").is_none());
    assert_eq!(tool_value["note"], json!(3), "premise: the tool merged");
    assert!(tool_value.get("type").is_none());
    assert!(tool_value.get("inputSchema").is_none());
    assert_eq!(block_value["note"], json!(4), "premise: the block merged");
    assert!(block_value.get("cache_control").is_none());
}
