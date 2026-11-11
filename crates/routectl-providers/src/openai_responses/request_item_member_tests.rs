// Members of the client's `function_call` and `reasoning` input items that
// canonical does not model, recorded by the Responses ingress: emitted on the
// item that ships under the same identity, withheld (declared and counted)
// otherwise, and never on the Bedrock Mantle wire.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

const ITEM_MEMBER_CLASS: &str = "unknown_member_withheld";

fn item_entry(pointer: &str, pairs: &[(&str, Value)]) -> routectl_core::ResponsesItemMemberEntry {
    routectl_core::ResponsesItemMemberEntry {
        client_pointer: pointer.to_owned(),
        members: pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    }
}

fn reasoning_detail_of(kind: ReasoningDetailKind, id: &str, payload: Value) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: Some(id.into()),
        format: Some(routectl_core::OPENAI_RESPONSES_V1.into()),
        index: None,
        payload,
    }
}

/// The assistant turn the Responses ingress builds from a `reasoning` item
/// (signed when `signed`) followed by a `function_call` item.
fn reasoning_and_call_turn(signed: bool, call_id: &str) -> Message {
    let mut details = vec![reasoning_detail_of(
        ReasoningDetailKind::Summary,
        "rs_1",
        json!({"text": "thought"}),
    )];
    if signed {
        details.push(reasoning_detail_of(
            ReasoningDetailKind::Encrypted,
            "rs_1",
            json!({"encrypted_content": "sig"}),
        ));
    }
    Message {
        role: Role::Assistant,
        content: MessageContent::Null,
        reasoning_details: details,
        tool_calls: Some(vec![json!({"id": call_id, "type": "function",
                                     "function": {"name": "f", "arguments": "{}"}})]),
        ..user_text("")
    }
}

/// A Responses request: user turn at `/input/0`, reasoning at `/input/1`,
/// function_call at `/input/2`, with the members the ingress recorded.
fn item_member_request(signed: bool, call_id: &str) -> ChatRequest {
    let mut req = req_with(vec![
        user_text("hi"),
        reasoning_and_call_turn(signed, call_id),
    ]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
    let record = &mut req.routectl_internal.responses_item_members;
    record.reasoning.insert(
        "rs_1".into(),
        item_entry("/input/1", &[("status", json!("completed"))]),
    );
    record.function_call.insert(
        call_id.into(),
        item_entry(
            "/input/2",
            &[
                ("id", json!("fc_1")),
                ("zz_future", json!({"k": "caf\u{e9}"})),
            ],
        ),
    );
    req
}

/// On the api-key and ChatGPT-OAuth surfaces a signed reasoning item ships
/// with its id and its members, and a call ships its members under the
/// `call_id` it is emitted with, a sanitized one included.
#[test]
fn item_members_ride_the_items_that_ship_under_their_identity() {
    let rows = [
        ("api-key, wire-safe id", cfg_api_key(), "call_1"),
        ("chatgpt-oauth, wire-safe id", cfg(), "call_1"),
        ("api-key, sanitized id", cfg_api_key(), "call.a:1"),
    ];
    for (row, cfg, call_id) in rows {
        // Arrange
        let req = item_member_request(true, call_id);
        let before = responses_drop_count(ITEM_MEMBER_CLASS);

        // Act
        let (wire, edits) = translate_declaring(&cfg, &req);

        // Assert
        let input = &wire["input"];
        assert_eq!(input[1]["type"], json!("reasoning"), "{row}: {wire}");
        assert_eq!(input[1]["id"], json!("rs_1"), "{row}: {wire}");
        assert_eq!(input[1]["status"], json!("completed"), "{row}: {wire}");
        assert_eq!(input[2]["type"], json!("function_call"), "{row}: {wire}");
        assert_eq!(input[2]["id"], json!("fc_1"), "{row}: {wire}");
        assert_eq!(
            input[2]["zz_future"],
            json!({"k": "caf\u{e9}"}),
            "{row}: {wire}"
        );
        assert_eq!(responses_drop_count(ITEM_MEMBER_CLASS) - before, 0, "{row}");
        assert!(
            !edits
                .iter()
                .any(|(_, path, _)| path.starts_with("/input/1/") || path.starts_with("/input/2/")),
            "{row}: {edits:?}"
        );
    }
}

/// A recorded member naming a member the item writes never replaces it,
/// present or not.
#[test]
fn a_recorded_member_naming_a_typed_member_keeps_the_typed_value() {
    // Arrange
    let mut req = item_member_request(true, "call_1");
    let record = &mut req.routectl_internal.responses_item_members;
    record.function_call.insert(
        "call_1".into(),
        item_entry(
            "/input/2",
            &[
                ("arguments", json!("{\"forged\":1}")),
                ("call_id", json!("call_forged")),
                ("name", json!("forged")),
                ("namespace", json!("forged")),
                ("type", json!("message")),
                ("id", json!("fc_1")),
            ],
        ),
    );
    record.reasoning.insert(
        "rs_1".into(),
        item_entry(
            "/input/1",
            &[
                ("encrypted_content", json!("forged")),
                ("id", json!("rs_forged")),
            ],
        ),
    );

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    assert_eq!(
        wire["input"][2],
        json!({"type": "function_call", "call_id": "call_1", "name": "f",
               "arguments": "{}", "id": "fc_1"}),
        "{wire}"
    );
    assert_eq!(
        wire["input"][1]["encrypted_content"],
        json!("sig"),
        "{wire}"
    );
    assert_eq!(wire["input"][1]["id"], json!("rs_1"), "{wire}");
}

/// An unsigned reasoning group ships without its id, so its members have no
/// item to ride: each is absent, declared removed at the client's item, and
/// the request counts once. The call beside it still carries its members.
#[test]
fn an_unsigned_reasoning_items_members_are_withheld_declared_and_counted_once() {
    // Arrange
    let mut req = item_member_request(false, "call_1");
    req.routectl_internal
        .responses_item_members
        .unkeyed
        .push(item_entry("/input/3", &[("zz_orphan", json!(1))]));
    let before = responses_drop_count(ITEM_MEMBER_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_api_key(), &req);

    // Assert
    let reasoning = &wire["input"][1];
    assert_eq!(reasoning["type"], json!("reasoning"), "{wire}");
    assert!(reasoning.get("id").is_none(), "{wire}");
    assert!(reasoning.get("status").is_none(), "{wire}");
    assert!(!wire.to_string().contains("zz_orphan"), "{wire}");
    assert_eq!(wire["input"][2]["id"], json!("fc_1"), "{wire}");
    let removed = |path: &str| {
        (
            routectl_core::wire_check::EditSite::DialectFieldStrip,
            path.to_owned(),
            routectl_core::wire_check::EditKind::Remove,
        )
    };
    assert!(edits.contains(&removed("/input/1/status")), "{edits:?}");
    assert!(edits.contains(&removed("/input/3/zz_orphan")), "{edits:?}");
    assert_eq!(responses_drop_count(ITEM_MEMBER_CLASS) - before, 1);
}

/// Bedrock Mantle receives none of the recorded members, a url-shaped one
/// included: each is declared removed at its client item, and the request
/// counts once with the lane's other withholds. Positive control: the
/// first-party surface carries the same members.
#[test]
fn mantle_withholds_recorded_item_members_declares_each_and_counts_once() {
    // Arrange
    let mut req = item_member_request(true, "call_1");
    req.messages = vec![carried_user_turn(), reasoning_and_call_turn(true, "call_1")].into();
    req.routectl_internal
        .responses_item_members
        .function_call
        .get_mut("call_1")
        .expect("recorded call")
        .members
        .insert("zz_source".into(), json!({"uri": s3_url()}));
    let before = responses_drop_count(ITEM_MEMBER_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);
    let first_party = translate_to_json(&cfg(), &req);

    // Assert
    let text = wire.to_string();
    for member in ["fc_1", "zz_future", "planted-bucket", "\"status\""] {
        assert!(!text.contains(member), "{member}: {wire}");
    }
    for path in [
        "/input/1/status",
        "/input/2/id",
        "/input/2/zz_future",
        "/input/2/zz_source",
        "/input/*/id",
    ] {
        assert!(edits.contains(&remove(path)), "{path}: {edits:?}");
    }
    assert_eq!(
        responses_drop_count(ITEM_MEMBER_CLASS) - before,
        1,
        "{wire}"
    );
    assert_eq!(first_party["input"][2]["zz_source"]["uri"], json!(s3_url()));
}

/// The item-member record is a Responses client's own: no surface counts it
/// foreign. From another ingress dialect (a record only a library caller or
/// a stray carry could leave there) it is withheld and the request counts
/// foreign once, beside the message carriers.
#[test]
fn recorded_item_members_count_foreign_only_from_another_dialect() {
    for (surface, cfg) in [
        ("api-key", cfg_api_key()),
        ("chatgpt-oauth", cfg()),
        ("bedrock-mantle", cfg_bedrock_mantle()),
    ] {
        // Arrange
        let mut req = item_member_request(true, "call_1");
        req.messages = vec![carried_user_turn(), reasoning_and_call_turn(true, "call_1")].into();

        // Act
        let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
            "openai-responses",
            "foreign_dialect_extra",
            || translate_to_json(&cfg, &req),
        );

        // Assert
        let emitted = wire["input"][2].get("zz_future").is_some();
        assert_eq!(emitted, surface != "bedrock-mantle", "{surface}: {wire}");
    }
    // Arrange
    let mut foreign = item_member_request(true, "call_1");
    foreign.messages = vec![carried_user_turn(), reasoning_and_call_turn(true, "call_1")].into();
    foreign.routectl_internal.provenance = routectl_core::RequestProvenance::OpenaiIngress;
    let before = responses_drop_count("foreign_dialect_extra");

    // Act
    let wire = translate_to_json(&cfg_api_key(), &foreign);

    // Assert
    assert!(!wire.to_string().contains("zz_future"), "{wire}");
    assert_eq!(responses_drop_count("foreign_dialect_extra") - before, 1);
}
