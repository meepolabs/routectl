// Translation-drop telemetry for the canonical fields and extras entries the
// openai-compat request path keeps off the wire. Each counted site has one
// SINGLE-SOURCE case: its request carries only that site's input, so deleting
// that site's record reds exactly the case its marker names. Every case also
// asserts the claimed drop is absent from the serialized body.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

fn compat_drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == super::super::LANE && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

fn compat_policy_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == super::super::LANE && e.policy_class == class)
        .map_or(0, |e| e.action_count)
}

/// The rendered lenient wire body plus the delta `read` saw across it.
fn wire_and_delta(
    req: &ChatRequest,
    payload_extras: Option<&serde_json::Value>,
    read: impl Fn() -> u64,
) -> (String, u64) {
    let before = read();
    let body = normalize(
        "test",
        req,
        ReasoningDialect::Passthrough,
        HistoryReasoning::Auto,
        payload_extras,
        false,
    )
    .expect("lenient normalize must succeed");
    let after = read();
    (
        serde_json::to_string(&body).expect("body renders"),
        after - before,
    )
}

fn user_parts_message(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

// ---------------------------------------------------------------------------
// cache_control_unsupported: four sites, one class, one case per site.
// ---------------------------------------------------------------------------

#[test]
fn a_top_level_cache_control_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("cache_control_unsupported")
    });

    // Assert
    assert!(!rendered.contains("cache_control"), "{rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn a_system_block_cache_control_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    req.system = Some(marked_system());

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("cache_control_unsupported")
    });

    // Assert
    assert!(!rendered.contains("cache_control"), "{rendered}");
    assert!(
        rendered.contains("be helpful"),
        "the system text survives: {rendered}"
    );
    assert_eq!(delta, 1);
}

#[test]
fn a_tool_cache_control_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    req.tools = Some(vec![marked_tool()]);

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("cache_control_unsupported")
    });

    // Assert
    assert!(!rendered.contains("cache_control"), "{rendered}");
    assert!(rendered.contains("calc"), "the tool survives: {rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn a_message_part_cache_control_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    push_msg(
        &mut req,
        user_parts_message(vec![marked_text_part("cache me")]),
    );

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("cache_control_unsupported")
    });

    // Assert
    assert!(!rendered.contains("cache_control"), "{rendered}");
    assert!(
        rendered.contains("cache me"),
        "the part text survives: {rendered}"
    );
    assert_eq!(delta, 1);
}

#[test]
fn breakpoints_on_every_surface_of_one_request_count_one_drop() {
    // Arrange -- all four surfaces at once: one request, one drop event.
    let mut req = simple_req("m");
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
    req.system = Some(marked_system());
    req.tools = Some(vec![marked_tool()]);
    push_msg(
        &mut req,
        user_parts_message(vec![marked_text_part("a"), marked_text_part("b")]),
    );

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("cache_control_unsupported")
    });

    // Assert
    assert!(!rendered.contains("cache_control"), "{rendered}");
    assert_eq!(delta, 1, "the count is per request, not per breakpoint");
}

fn tool_block(fields: serde_json::Value) -> ContentPart {
    ContentPart::Other {
        type_tag: fields["type"].as_str().unwrap().into(),
        cache_control: None,
        extras: fields.as_object().unwrap().clone(),
    }
}

fn lenient_messages(req: &ChatRequest, name: &str) -> Vec<serde_json::Value> {
    let body = normalize(
        "test",
        req,
        ReasoningDialect::Passthrough,
        HistoryReasoning::Auto,
        None,
        false,
    )
    .unwrap_or_else(|e| panic!("{name}: the transcript must forward, got {e}"));
    body["messages"].as_array().unwrap().clone()
}

/// A `tool_use` block's `id` and `name` ride verbatim into the built body,
/// absent or not a string included: nothing on this egress reads them
/// after the lift, so only the upstream can object.
#[test]
fn tool_use_blocks_with_absent_or_non_string_id_or_name_reach_the_openai_compat_body() {
    let rows = [
        (
            "absent id",
            json!({"type": "tool_use", "name": "f", "input": {}}),
            json!({"type": "function", "function": {"name": "f", "arguments": "{}"}}),
        ),
        (
            "non-string id",
            json!({"type": "tool_use", "id": 7, "name": "f", "input": {}}),
            json!({"id": 7, "type": "function", "function": {"name": "f", "arguments": "{}"}}),
        ),
        (
            "absent name",
            json!({"type": "tool_use", "id": "t1", "input": {}}),
            json!({"id": "t1", "type": "function", "function": {"arguments": "{}"}}),
        ),
        (
            "non-string name",
            json!({"type": "tool_use", "id": "t1", "name": ["n"], "input": {}}),
            json!({"id": "t1", "type": "function", "function": {"name": ["n"], "arguments": "{}"}}),
        ),
    ];
    for (name, block, expected) in rows {
        // Arrange
        let mut req = simple_req("m");
        push_msg(
            &mut req,
            Message {
                role: Role::Assistant,
                ..user_parts_message(vec![tool_block(block)])
            },
        );

        // Act
        let messages = lenient_messages(&req, name);

        // Assert
        assert_eq!(messages[1]["tool_calls"], json!([expected]), "{name}");
    }
}

/// A `tool_result` block's `tool_use_id` rides verbatim as the tool
/// message's `tool_call_id`, absent or not a string included.
#[test]
fn tool_result_blocks_with_absent_or_non_string_id_reach_the_openai_compat_body() {
    let rows = [
        (
            "absent id",
            json!({"type": "tool_result", "content": "a"}),
            json!({"role": "tool", "content": "a"}),
        ),
        (
            "non-string id",
            json!({"type": "tool_result", "tool_use_id": 7, "content": "a"}),
            json!({"role": "tool", "tool_call_id": 7, "content": "a"}),
        ),
    ];
    for (name, block, expected) in rows {
        // Arrange
        let mut req = simple_req("m");
        push_msg(&mut req, user_parts_message(vec![tool_block(block)]));

        // Act
        let messages = lenient_messages(&req, name);

        // Assert
        assert_eq!(messages[1], expected, "{name}");
    }
}

#[test]
fn strict_mode_rejects_a_breakpoint_and_counts_nothing() {
    // Arrange
    let mut req = simple_req("m");
    req.cache_control = Some(routectl_core::CacheControl::ephemeral_5m());
    let before = compat_drop_count("cache_control_unsupported");

    // Act
    let result = normalize(
        "test",
        &req,
        ReasoningDialect::Passthrough,
        HistoryReasoning::Auto,
        None,
        true,
    );

    // Assert -- nothing was dropped, because nothing was sent.
    assert!(matches!(result, Err(Error::Validation(..))), "{result:?}");
    assert_eq!(compat_drop_count("cache_control_unsupported"), before);
}

// ---------------------------------------------------------------------------
// The remaining classes, one site each.
// ---------------------------------------------------------------------------

#[test]
fn anthropic_beta_flags_are_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange -- two flags, one request, one drop event.
    let mut req = simple_req("m");
    req.anthropic_beta = vec!["zz-beta-one-2099".into(), "zz-beta-two-2099".into()];

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("anthropic_beta_unsupported")
    });

    // Assert
    assert!(!rendered.contains("anthropic_beta"), "{rendered}");
    assert!(!rendered.contains("zz-beta"), "{rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn an_ingress_metadata_object_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"metadata": {"user_id": "zz-user-tell"}}));

    // Act
    let (rendered, delta) =
        wire_and_delta(&req, None, || compat_drop_count("metadata_unsupported"));

    // Assert
    assert!(!rendered.contains("zz-user-tell"), "{rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn a_forward_compat_extra_is_absent_from_the_openai_compat_wire_and_counted() {
    // Arrange -- two swept fields, one request, one drop event; a long-tail
    // extra beside them still forwards.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({
        "context_management": {"edits": []},
        "mcp_servers": [{"name": "zz-mcp-tell"}],
        "top_k": 40,
    }));

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_drop_count("forward_compat_extra_unrepresentable")
    });

    // Assert
    assert!(!rendered.contains("context_management"), "{rendered}");
    assert!(!rendered.contains("zz-mcp-tell"), "{rendered}");
    assert!(rendered.contains("\"top_k\":40"), "{rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn a_provider_extra_naming_a_canonical_key_is_refused_and_counted() {
    // Arrange
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"messages": [{"role": "user", "content": "zz-injected"}]}));

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_policy_count("provider_extra_managed_key_conflict")
    });

    // Assert
    assert!(!rendered.contains("zz-injected"), "{rendered}");
    assert!(
        rendered.contains("hello"),
        "the assembled messages survive: {rendered}"
    );
    assert_eq!(delta, 1);
}

#[test]
fn an_operator_extra_naming_a_managed_key_is_refused_and_counted() {
    // Arrange -- two refused keys from the operator: one action.
    let req = simple_req("m");
    let operator = json!({"model": "zz-other-model", "stream": true});

    // Act
    let (rendered, delta) = wire_and_delta(&req, Some(&operator), || {
        compat_policy_count("operator_extra_managed_key_conflict")
    });

    // Assert
    assert!(!rendered.contains("zz-other-model"), "{rendered}");
    assert_eq!(delta, 1);
}

#[test]
fn a_swept_output_config_is_withheld_without_a_second_count() {
    // Arrange -- a json_object format the lift maps, so nothing is lost; the
    // leftover object is withheld and counts on neither extras class.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"output_config": {"format": {"type": "json_object"}}}));
    let drops = || compat_drop_count("forward_compat_extra_unrepresentable");
    let actions = || compat_policy_count("provider_extra_managed_key_conflict");
    let (drops_before, actions_before) = (drops(), actions());

    // Act
    let (rendered, _) = wire_and_delta(&req, None, || 0);

    // Assert
    assert!(!rendered.contains("output_config"), "{rendered}");
    assert!(
        rendered.contains("json_object"),
        "the lifted format ships: {rendered}"
    );
    assert_eq!(drops(), drops_before);
    assert_eq!(actions(), actions_before);
}
