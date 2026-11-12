// Members a client of another dialect carried on its messages, parts and
// tools: absent from every Responses surface, declared removed, and counted
// once as `foreign_dialect_extra`, never as this lane's own
// `unknown_member_withheld`.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

const FOREIGN_CLASS: &str = "foreign_dialect_extra";

/// A user turn carrying a message member and a part member, and a custom
/// tool carrying a member, from `provenance` with `extras` at the top level.
fn foreign_carrying(provenance: routectl_core::RequestProvenance, extras: Value) -> ChatRequest {
    let member = |key: &str, value: Value| {
        let mut map = serde_json::Map::new();
        map.insert(key.to_owned(), value);
        map
    };
    let mut turn = user_parts(vec![ContentPart::Known(KnownContentPart::Text {
        text: "hi".into(),
        citations: None,
        cache_control: None,
        extras: member("zz_part_member", json!("caf\u{e9}")),
    })]);
    turn.extras = member("zz_message_member", json!("zz-message-tell"));
    let mut req = req_with(vec![turn]);
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "calc".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
        extras: member("zz_tool_member", json!(1)),
    })]);
    req.provider_extras = Some(extras);
    req.routectl_internal.provenance = provenance;
    req
}

/// Every Responses auth surface, by name.
fn responses_surfaces() -> [(&'static str, OpenAiResponsesConfig); 3] {
    [
        ("api-key", cfg_api_key()),
        ("chatgpt-oauth", cfg()),
        ("bedrock-mantle", cfg_bedrock_mantle()),
    ]
}

#[test]
fn a_foreign_client_carried_member_is_absent_declared_and_counted_once_on_every_responses_surface()
{
    use routectl_core::RequestProvenance;
    use routectl_core::wire_check::{EditKind, EditSite};
    let provenances = [
        (RequestProvenance::AnthropicIngress, "/messages/*"),
        (RequestProvenance::OpenaiIngress, "/messages/*"),
    ];
    for (surface, cfg) in responses_surfaces() {
        for (provenance, messages) in provenances {
            // Arrange -- carried members only, no top-level extras at all.
            let req = foreign_carrying(provenance, json!({}));
            let foreign_before = responses_drop_count(FOREIGN_CLASS);
            let withheld_before = responses_drop_count("unknown_member_withheld");
            let seen_before = responses_lane_seen_count();

            // Act
            let (wire, edits) = translate_declaring(&cfg, &req);

            // Assert
            let row = format!("{surface} {provenance:?}");
            let text = wire.to_string();
            for member in ["zz_message_member", "zz_part_member", "zz_tool_member"] {
                assert!(!text.contains(member), "{row} {member}: {text}");
            }
            for path in [
                format!("{messages}/zz_message_member"),
                format!("{messages}/content/*/zz_part_member"),
                "/tools/*/zz_tool_member".to_owned(),
            ] {
                let removed = (EditSite::ManagedExtrasStrip, path, EditKind::Remove);
                assert!(edits.contains(&removed), "{row} {removed:?}: {edits:?}");
            }
            assert_eq!(
                responses_drop_count(FOREIGN_CLASS) - foreign_before,
                1,
                "{row}"
            );
            assert_eq!(
                responses_drop_count("unknown_member_withheld") - withheld_before,
                0,
                "{row}: a foreign member is not this surface's withhold"
            );
            assert_eq!(responses_lane_seen_count() - seen_before, 1, "{row}");
        }
    }
}

#[test]
fn a_foreign_top_level_key_beside_a_carried_member_counts_the_responses_request_once() {
    use routectl_core::wire_check::{EditKind, EditSite};
    // Arrange -- the top-level key and the carried members share the
    // request's one foreign-dialect tally.
    let req = foreign_carrying(
        routectl_core::RequestProvenance::AnthropicIngress,
        json!({"container": "zz-container-tell"}),
    );
    let before = responses_drop_count(FOREIGN_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_api_key(), &req);

    // Assert
    let text = wire.to_string();
    assert!(!text.contains("zz-container-tell"), "{text}");
    assert!(!text.contains("zz-message-tell"), "{text}");
    let removed = (
        EditSite::ManagedExtrasStrip,
        "/container".to_owned(),
        EditKind::Remove,
    );
    assert!(edits.contains(&removed), "{edits:?}");
    assert_eq!(responses_drop_count(FOREIGN_CLASS) - before, 1);
}

/// The positive control: the same carriers from a Responses client are this
/// wire's own. The first-party surfaces emit them; Bedrock Mantle withholds
/// them under its own class. Neither counts them as foreign.
#[test]
fn a_responses_client_carried_member_is_never_counted_foreign() {
    for (surface, cfg) in responses_surfaces() {
        // Arrange
        let req = foreign_carrying(
            routectl_core::RequestProvenance::ResponsesIngress,
            json!({}),
        );

        // Act
        let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
            "openai-responses",
            FOREIGN_CLASS,
            || translate_to_json(&cfg, &req),
        );

        // Assert
        let emitted = wire["input"][0].get("zz_message_member").is_some();
        assert_eq!(emitted, surface != "bedrock-mantle", "{surface}: {wire}");
    }
}
