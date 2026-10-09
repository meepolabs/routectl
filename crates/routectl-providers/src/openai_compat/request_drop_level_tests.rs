// Log level of the Anthropic-only field drops on the openai-compat egress.
// Each is inherent to routing Anthropic-shape traffic here, so it logs once
// per carrier at DEBUG and stays out of the default WARN stream.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

fn marked_text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: Some(routectl_core::CacheControl::ephemeral_5m()),
    })
}

fn marked_system() -> routectl_core::SystemContent {
    routectl_core::SystemContent::Blocks(vec![routectl_core::SystemBlock {
        kind: "text".into(),
        text: "be helpful".into(),
        cache_control: Some(routectl_core::CacheControl::ephemeral_5m()),
        citations: None,
    }])
}

fn marked_tool() -> ToolDef {
    ToolDef::Custom(routectl_core::CustomTool {
        name: "calc".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: Some(routectl_core::CacheControl::ephemeral_5m()),
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

#[test]
fn anthropic_only_field_drops_log_once_at_debug() {
    // Arrange: one row per carrier, each a request carrying only that field.
    let mut top_level = simple_req("m");
    top_level.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
    let mut beta = simple_req("m");
    beta.anthropic_beta = vec!["oauth-2025-04-20".into()];
    let mut system = simple_req("m");
    system.system = Some(marked_system());
    let mut tool = simple_req("m");
    tool.tools = Some(vec![marked_tool()]);
    let mut block = simple_req("m");
    push_msg(
        &mut block,
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![marked_text_part("cache me")]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    );
    let mut metadata = simple_req("m");
    metadata.provider_extras = Some(json!({"metadata": {"user_id": "abc123"}}));
    let rows = [
        (
            "top-level cache_control",
            top_level,
            "top-level cache_control dropped",
        ),
        ("anthropic_beta", beta, "anthropic_beta flags dropped"),
        (
            "system cache_control",
            system,
            "per-block cache_control on system dropped",
        ),
        ("tool cache_control", tool, "tool cache_control dropped"),
        (
            "message block cache_control",
            block,
            "per-block cache_control dropped",
        ),
        ("metadata", metadata, "`metadata` object dropped"),
    ];

    for (row, req, needle) in rows {
        // Act
        let events = routectl_testkit::capture_events(|| {
            let _ = lenient_normalize(&req);
        });

        // Assert
        let levels: Vec<_> = events
            .iter()
            .filter(|e| e.message.contains(needle))
            .map(|e| e.level)
            .collect();
        assert_eq!(
            levels,
            vec![tracing::Level::DEBUG],
            "row {row}: expected one DEBUG containing {needle:?}; got {events:?}"
        );
    }
}
