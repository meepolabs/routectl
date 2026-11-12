// Members a Responses client declared on a function tool beyond the ones
// the Responses function shape types: emitted on the first-party surfaces
// with the typed members winning, withheld on Bedrock Mantle under its own
// class, and withheld as foreign on another dialect's egress.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

/// A Responses request declaring `tool` as its one tool, normalized the way
/// the Responses ingress normalizes it.
fn declaring_tool(tool: &Value) -> ChatRequest {
    let mut req = req_with(vec![user_text("hi")]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
    req.tools = Some(vec![ToolDef::Custom(
        CustomTool::from_responses_function(tool).expect("a function declaration"),
    )]);
    req
}

fn lookup_with_member() -> Value {
    json!({"type": "function", "name": "lookup", "parameters": {"type": "object"},
           "future_member": {"n": "caf\u{e9}"}})
}

#[test]
fn a_function_tool_member_rides_the_first_party_surfaces_as_declared() {
    for (surface, cfg) in [("api-key", cfg_api_key()), ("chatgpt-oauth", cfg())] {
        // Arrange
        let declared = lookup_with_member();
        let req = declaring_tool(&declared);

        // Act
        let wire = translate_to_json(&cfg, &req);

        // Assert
        assert_eq!(wire["tools"][0], declared, "{surface}: {wire}");
    }
}

/// An extras key named like a member the function shape types never
/// replaces the typed value, and never recreates an absent one.
#[test]
fn a_tool_member_named_like_a_typed_member_never_reaches_the_wire() {
    // Arrange
    let mut req = declaring_tool(&lookup_with_member());
    let ToolDef::Custom(tool) = &mut req.tools.as_mut().expect("tools")[0] else {
        panic!("a custom tool");
    };
    for (key, value) in [
        ("name", json!("planted")),
        ("parameters", json!({"planted": true})),
        ("description", json!("planted")),
        ("type", json!("planted")),
        ("strict", json!(true)),
    ] {
        tool.extras.insert(key.to_owned(), value);
    }

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    let emitted = &wire["tools"][0];
    assert_eq!(
        emitted,
        &json!({"type": "function", "name": "lookup", "parameters": {"type": "object"},
                "future_member": {"n": "caf\u{e9}"}}),
        "{wire}"
    );
}

#[test]
fn mantle_withholds_a_function_tool_member_declares_it_and_counts_once() {
    // Rows: the tool member as the request's only withhold (the count is
    // its own), and beside a carried message member (still one count).
    for (row, message_member) in [
        ("tool member alone", false),
        ("beside a message member", true),
    ] {
        // Arrange
        let mut req = declaring_tool(&lookup_with_member());
        if message_member {
            req.messages = vec![with_members(
                user_text("hi"),
                members(&[("id", json!("msg_1"))]),
            )]
            .into();
        }
        let withheld_before = responses_drop_count("unknown_member_withheld");
        let foreign_before = responses_drop_count("foreign_dialect_extra");

        // Act
        let (wire, edits) = translate_declaring(&cfg_bedrock_mantle(), &req);

        // Assert
        assert_eq!(
            wire["tools"][0],
            json!({"type": "function", "name": "lookup", "parameters": {"type": "object"}}),
            "{row}: {wire}"
        );
        assert!(
            edits.contains(&(
                routectl_core::wire_check::EditSite::AuthSurfaceGate,
                "/tools/0/future_member".to_owned(),
                routectl_core::wire_check::EditKind::Remove,
            )),
            "{row}: {edits:?}"
        );
        assert_eq!(
            responses_drop_count("unknown_member_withheld") - withheld_before,
            1,
            "{row}"
        );
        assert_eq!(
            responses_drop_count("foreign_dialect_extra") - foreign_before,
            0,
            "{row}"
        );
    }
}

/// The control: a function tool without members withholds nothing on
/// Mantle, so the count above is the member's.
#[test]
fn a_member_free_function_tool_withholds_nothing_on_mantle() {
    // Arrange
    let req = declaring_tool(&json!({"type": "function", "name": "lookup"}));

    // Act
    let (wire, edits) = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        "unknown_member_withheld",
        || translate_declaring(&cfg_bedrock_mantle(), &req),
    );

    // Assert
    assert_eq!(wire["tools"][0]["name"], json!("lookup"), "{wire}");
    assert!(
        !edits.iter().any(|(_, path, _)| path.starts_with("/tools")),
        "{edits:?}"
    );
}

/// A member another dialect's client carried on a custom tool stays off a
/// Responses wire: the function shape emits a Responses client's members
/// only.
#[test]
fn a_foreign_client_tool_member_stays_off_the_responses_wire() {
    // Arrange
    let mut req = declaring_tool(&json!({"type": "function", "name": "lookup"}));
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
    let ToolDef::Custom(tool) = &mut req.tools.as_mut().expect("tools")[0] else {
        panic!("a custom tool");
    };
    tool.extras
        .insert("eager_input_streaming".into(), json!(true));

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    assert_eq!(
        wire["tools"][0],
        json!({"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {}}}),
        "{wire}"
    );
}
