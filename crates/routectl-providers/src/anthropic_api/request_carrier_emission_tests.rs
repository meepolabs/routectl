//! Members a same-dialect client sent at message, block, custom-tool and
//! system-block level ride the Anthropic-shaped body on the node they came
//! from, after the typed members, which win a name clash; reserved names
//! never reach the wire, and a request from another dialect emits none.

use std::sync::Arc;

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{
    ChatRequest, ContentPart, CustomTool, KnownContentPart, Message, MessageContent,
    RequestProvenance, Role, SystemBlock, SystemContent, ToolDef,
};
use serde_json::{Map, Value, json};

fn carried(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
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

fn text_part(text: &str, extras: Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn custom_tool(extras: Map<String, Value>) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: "lookup".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
        extras,
    })
}

fn system_block(extras: Map<String, Value>) -> SystemContent {
    SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "be brief".into(),
        cache_control: None,
        citations: None,
        extras,
    }])
}

/// A request carrying one member at each carrier level.
fn carrying_request(provenance: RequestProvenance) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: Arc::from(vec![message(
            Role::User,
            MessageContent::Parts(vec![text_part(
                "hi",
                carried(&[("caller", json!({"type": "direct"}))]),
            )]),
            carried(&[("output_config", json!({"effort": "low"}))]),
        )]),
        tools: Some(vec![custom_tool(carried(&[(
            "eager_input_streaming",
            json!(true),
        )]))]),
        system: Some(system_block(carried(&[("origin", json!("hook"))]))),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req
}

fn anthropic_body(req: &ChatRequest) -> Value {
    super::normalize("test", req, false, None, false, true, false).expect("anthropic body")
}

#[cfg(feature = "bedrock")]
fn invoke_body(req: &ChatRequest) -> Value {
    use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
    let cfg = BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-4-7".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    };
    crate::bedrock::invoke::normalize_request(&cfg, req).expect("invoke body")
}

fn bodies(req: &ChatRequest) -> Vec<(&'static str, Value)> {
    vec![
        ("anthropic-api", anthropic_body(req)),
        #[cfg(feature = "bedrock")]
        ("bedrock-invoke", invoke_body(req)),
    ]
}

#[test]
fn every_carrier_level_reaches_the_wire_on_its_own_node() {
    let req = carrying_request(RequestProvenance::AnthropicIngress);

    for (lane, body) in bodies(&req) {
        assert_eq!(
            body["messages"][0]["output_config"],
            json!({"effort": "low"}),
            "{lane}"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["caller"],
            json!({"type": "direct"}),
            "{lane}"
        );
        assert_eq!(
            body["tools"][0]["eager_input_streaming"],
            json!(true),
            "{lane}"
        );
        assert_eq!(body["system"][0]["origin"], json!("hook"), "{lane}");
    }
}

#[test]
fn a_request_from_another_dialect_emits_no_carried_member() {
    let req = carrying_request(RequestProvenance::OpenaiIngress);

    for (lane, body) in bodies(&req) {
        assert!(body["messages"][0].get("output_config").is_none(), "{lane}");
        assert!(
            body["messages"][0]["content"][0].get("caller").is_none(),
            "{lane}"
        );
        assert!(
            body["tools"][0].get("eager_input_streaming").is_none(),
            "{lane}"
        );
        assert!(body["system"][0].get("origin").is_none(), "{lane}");
    }
}

#[test]
fn reserved_names_never_reach_the_wire_even_when_the_typed_value_is_absent() {
    let reserved = carried(&[
        ("type", json!("image")),
        ("text", json!("forged")),
        ("cache_control", json!({"type": "ephemeral"})),
    ]);
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.messages = Arc::from(vec![message(
        Role::User,
        MessageContent::Parts(vec![
            text_part("hi", reserved.clone()),
            ContentPart::Known(KnownContentPart::Image {
                source: json!({"type": "base64", "media_type": "image/png", "data": "AAAA"}),
                cache_control: None,
                extras: carried(&[
                    ("type", json!("text")),
                    ("cache_control", json!({"type": "ephemeral"})),
                    (
                        "source",
                        json!({"type": "url", "url": "https://example.com/x.png"}),
                    ),
                ]),
            }),
        ]),
        carried(&[("role", json!("assistant")), ("content", json!("forged"))]),
    )]);

    for (lane, body) in bodies(&req) {
        let text = &body["messages"][0]["content"][0];
        let image = &body["messages"][0]["content"][1];
        assert_eq!(text["type"], "text", "{lane}");
        assert_eq!(text["text"], "hi", "{lane}");
        assert!(text.get("cache_control").is_none(), "{lane}: {text}");
        assert_eq!(image["type"], "image", "{lane}");
        assert_eq!(image["source"]["type"], "base64", "{lane}: {image}");
        assert!(image.get("cache_control").is_none(), "{lane}: {image}");
        assert_eq!(body["messages"][0]["role"], "user", "{lane}");
    }
}

/// Every carrier node of a carrier-free same-dialect request holds its typed
/// members and nothing else. Full-body bytes for carrier-free traffic are
/// pinned by the `contract_egress` and `contract_egress_bedrock_invoke`
/// snapshot suites.
#[test]
fn a_carrier_free_request_emits_only_typed_members_on_every_carrier_node() {
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.messages = Arc::from(vec![message(
        Role::User,
        MessageContent::Parts(vec![text_part("hi", Map::new())]),
        Map::new(),
    )]);
    req.tools = Some(vec![custom_tool(Map::new())]);
    req.system = Some(system_block(Map::new()));

    for (lane, body) in bodies(&req) {
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]),
            "{lane}"
        );
        assert_eq!(
            body["tools"],
            json!([{"name": "lookup", "input_schema": {"type": "object"}}]),
            "{lane}"
        );
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "be brief"}]),
            "{lane}"
        );
    }
}

#[test]
fn a_message_level_cache_control_member_never_reaches_the_wire() {
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.messages = Arc::from(vec![message(
        Role::User,
        MessageContent::Text("hi".into()),
        carried(&[
            ("cache_control", json!({"type": "ephemeral"})),
            ("note", json!(1)),
        ]),
    )]);

    for (lane, body) in bodies(&req) {
        let turn = &body["messages"][0];
        assert_eq!(turn["note"], json!(1), "{lane}: premise: merged: {turn}");
        assert!(turn.get("cache_control").is_none(), "{lane}: {turn}");
    }
}

/// A user turn whose only part is `part`.
fn single_part_request(part: KnownContentPart) -> ChatRequest {
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.messages = Arc::from(vec![message(
        Role::User,
        MessageContent::Parts(vec![ContentPart::Known(part)]),
        Map::new(),
    )]);
    req
}

/// An OpenAI `file` part sent as a wire `document` never has a carried
/// member fill one of the document's own typed members, even one the
/// translation left absent (no `filename` means no `title`).
#[test]
fn a_file_part_sent_as_a_document_never_fills_a_document_member_from_its_carriers() {
    let req = single_part_request(KnownContentPart::File {
        file: json!({"file_data": "data:application/pdf;base64,JVBERg=="}),
        cache_control: None,
        extras: carried(&[
            ("title", json!("forged")),
            ("citations", json!({"enabled": true})),
            (
                "source",
                json!({"type": "url", "url": "https://example.com/forged"}),
            ),
            ("note", json!(1)),
        ]),
    });

    for (lane, body) in bodies(&req) {
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "document", "{lane}: {block}");
        assert_eq!(block["note"], json!(1), "{lane}: premise: merged: {block}");
        assert_eq!(block["source"]["type"], "base64", "{lane}: {block}");
        assert!(block.get("title").is_none(), "{lane}: {block}");
        assert!(block.get("citations").is_none(), "{lane}: {block}");
    }
}

fn tool_turn(id: &str, extras: Map<String, Value>) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        ..message(Role::Tool, MessageContent::Text("done".into()), extras)
    }
}

/// An assistant turn calling two tools, then their two results as
/// consecutive tool turns (indices 1 and 2) the egress joins.
fn tool_run_request(first: Map<String, Value>, second: Map<String, Value>) -> ChatRequest {
    let call = |id: &str| {
        ContentPart::Known(KnownContentPart::ToolUse {
            id: id.into(),
            name: "lookup".into(),
            input: json!({}),
            cache_control: None,
            extras: Map::new(),
        })
    };
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: Arc::from(vec![
            message(
                Role::Assistant,
                MessageContent::Parts(vec![call("t1"), call("t2")]),
                Map::new(),
            ),
            tool_turn("t1", first),
            tool_turn("t2", second),
        ]),
        ..Default::default()
    };
    req.routectl_internal.provenance = RequestProvenance::AnthropicIngress;
    req
}

fn body_and_edits(req: &ChatRequest) -> (Value, Vec<(EditSite, String, EditKind)>) {
    body_and_edits_on(req, WireTap::default())
}

fn body_and_edits_on(
    req: &ChatRequest,
    tap: WireTap,
) -> (Value, Vec<(EditSite, String, EditKind)>) {
    let mut req = req.clone();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let body = anthropic_body(&req);
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .map(|e| (e.site(), e.path().to_string(), e.kind()))
        .collect();
    (body, edits)
}

#[test]
fn a_joined_tool_run_emits_the_earlier_value_and_declares_one_rewrite() {
    let req = tool_run_request(
        carried(&[("tag", json!("first")), ("only_first", json!(1))]),
        carried(&[("tag", json!("second")), ("only_second", json!(2))]),
    );

    let (body, edits) = body_and_edits(&req);

    let joined = &body["messages"][1];
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(joined["tag"], json!("first"));
    assert_eq!(joined["only_first"], json!(1));
    assert_eq!(joined["only_second"], json!(2));
    let rewrites: Vec<_> = edits
        .iter()
        .filter(|(_, path, kind)| path.ends_with("/tag") && *kind == EditKind::Rewrite)
        .collect();
    assert_eq!(
        rewrites,
        [&(
            EditSite::DialectFieldStrip,
            "/messages/2/tag".to_owned(),
            EditKind::Rewrite
        )]
    );
}

#[test]
fn a_joined_tool_run_with_agreeing_members_declares_nothing() {
    let same = carried(&[("tag", json!("same"))]);
    let req = tool_run_request(same.clone(), same);

    let (body, edits) = body_and_edits(&req);

    assert_eq!(body["messages"][1]["tag"], json!("same"));
    assert!(
        !edits.iter().any(|(_, path, _)| path.ends_with("/tag")),
        "{edits:?}"
    );
}

#[test]
fn a_message_level_cache_control_member_is_declared_removed() {
    // Arrange
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.messages = Arc::from(vec![message(
        Role::User,
        MessageContent::Text("hi".into()),
        carried(&[("cache_control", json!({"type": "ephemeral"}))]),
    )]);
    let declared = (
        EditSite::CacheMarkerStrip,
        "/messages/*/cache_control".to_owned(),
        EditKind::Remove,
    );

    // Act
    let (with_marker, edits) = body_and_edits(&req);
    let (_, control) = body_and_edits(&carrying_request(RequestProvenance::AnthropicIngress));

    // Assert
    assert!(with_marker["messages"][0].get("cache_control").is_none());
    assert!(edits.contains(&declared), "{edits:?}");
    assert!(!control.contains(&declared), "{control:?}");
}

fn lift_drop_count() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::super::LANE && e.drop_class == "system_part_unrepresentable")
        .map_or(0, |e| e.drop_count)
}

/// A system turn carrying a message member and a text-part member, then a
/// user turn; no canonical system, so the anthropic-api egress lifts it.
fn lifted_carrier_request() -> ChatRequest {
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.system = None;
    req.tools = None;
    req.messages = Arc::from(vec![
        message(
            Role::System,
            MessageContent::Parts(vec![text_part(
                "be brief",
                carried(&[("origin", json!(1))]),
            )]),
            carried(&[("note", json!("caf\u{e9}"))]),
        ),
        message(Role::User, MessageContent::Text("hi".into()), Map::new()),
    ]);
    req
}

#[test]
fn a_lifted_system_turn_loses_its_carried_members_declared_and_counted_once() {
    // Arrange
    let req = lifted_carrier_request();

    // Act
    let before = lift_drop_count();
    let (body, edits) = body_and_edits(&req);
    let delta = lift_drop_count() - before;

    // Assert -- the text is lifted, its members have nowhere to go.
    assert_eq!(body["system"], json!("be brief"), "{body}");
    assert!(!body.to_string().contains("caf\u{e9}"), "{body}");
    for path in ["/messages/0/note", "/messages/0/content/0/origin"] {
        assert!(
            edits.contains(&(
                EditSite::DialectFieldStrip,
                path.to_owned(),
                EditKind::Remove
            )),
            "{path}: {edits:?}"
        );
    }
    assert_eq!(delta, 1);
}

#[test]
fn a_lifted_carrier_free_system_turn_counts_nothing() {
    // Arrange -- the control: the same turn without members.
    let mut req = lifted_carrier_request();
    let mut messages = req.messages.to_vec();
    messages[0] = message(
        Role::System,
        MessageContent::Parts(vec![text_part("be brief", Map::new())]),
        Map::new(),
    );
    req.messages = Arc::from(messages);

    // Act
    let before = lift_drop_count();
    let (body, _) = body_and_edits(&req);

    // Assert
    assert_eq!(body["system"], json!("be brief"), "{body}");
    assert_eq!(lift_drop_count() - before, 0);
}

/// user, an empty system turn the forwarding screen drops, then user: the two
/// user turns join across the seam.
fn seam_join_request(first: Map<String, Value>, second: Map<String, Value>) -> ChatRequest {
    let mut req = carrying_request(RequestProvenance::AnthropicIngress);
    req.tools = None;
    req.messages = Arc::from(vec![
        message(Role::User, MessageContent::Text("earlier".into()), first),
        message(
            Role::System,
            MessageContent::Text(String::new()),
            Map::new(),
        ),
        message(Role::User, MessageContent::Text("later".into()), second),
    ]);
    req
}

#[test]
fn a_same_role_join_across_a_dropped_system_turn_keeps_both_turns_members() {
    // Arrange
    let req = seam_join_request(
        carried(&[("tag", json!("first")), ("only_first", json!(1))]),
        carried(&[("tag", json!("second")), ("only_second", json!(2))]),
    );

    // Act -- an admitted tap: only it tracks which turns a join names.
    let (body, edits) = body_and_edits_on(&req, WireTap::admitted());

    // Assert -- earlier wins a shared name; the later turn's own member
    // survives the join and is declared added to the earlier turn.
    let joined = &body["messages"][0];
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(joined["tag"], json!("first"));
    assert_eq!(joined["only_first"], json!(1));
    assert_eq!(joined["only_second"], json!(2));
    assert!(
        edits.contains(&(
            EditSite::DialectFieldStrip,
            "/messages/2/tag".to_owned(),
            EditKind::Rewrite
        )),
        "{edits:?}"
    );
    assert!(
        edits.contains(&(
            EditSite::DialectFieldStrip,
            "/messages/0/only_second".to_owned(),
            EditKind::Add
        )),
        "{edits:?}"
    );
}
