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

fn with_provenance(mut req: ChatRequest, provenance: RequestProvenance) -> ChatRequest {
    req.routectl_internal.provenance = provenance;
    req
}

#[test]
fn an_anthropic_ingress_metadata_object_is_absent_from_the_openai_compat_wire_and_counted_once() {
    // Arrange -- the identity block alone, as the Anthropic ingress sweeps it.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"metadata": {"user_id": "zz-user-tell"}}));
    let req = with_provenance(req, RequestProvenance::AnthropicIngress);
    let foreign_before = compat_drop_count("foreign_dialect_extra");

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_policy_count("client_fingerprint_stripped")
    });

    // Assert -- one class, once: the fingerprint withhold, never also foreign.
    assert!(!rendered.contains("zz-user-tell"), "{rendered}");
    assert_eq!(delta, 1);
    assert_eq!(compat_drop_count("foreign_dialect_extra"), foreign_before);
}

#[test]
fn a_chat_ingress_metadata_object_forwards_to_the_openai_compat_wire() {
    // Arrange -- `metadata` is a Chat Completions field when a Chat client
    // sends it.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"metadata": {"tag": "zz-chat-meta"}}));
    let req = with_provenance(req, RequestProvenance::OpenaiIngress);

    // Act
    let (rendered, delta) = wire_and_delta(&req, None, || {
        compat_policy_count("client_fingerprint_stripped")
    });

    // Assert
    assert!(rendered.contains("zz-chat-meta"), "{rendered}");
    assert_eq!(delta, 0);
}

#[test]
fn foreign_dialect_extras_are_absent_from_the_openai_compat_wire_and_counted_once() {
    // Arrange -- two Anthropic-only fields from one Anthropic-ingress request:
    // one drop event. The same bag from a Chat client is the positive control.
    let extras = json!({
        "context_management": {"edits": []},
        "mcp_servers": [{"name": "zz-mcp-tell"}],
    });
    let mut foreign = simple_req("m");
    foreign.provider_extras = Some(extras.clone());
    let foreign = with_provenance(foreign, RequestProvenance::AnthropicIngress);
    let mut same_dialect = simple_req("m");
    same_dialect.provider_extras = Some(extras);
    let same_dialect = with_provenance(same_dialect, RequestProvenance::OpenaiIngress);
    let count = || compat_drop_count("foreign_dialect_extra");

    // Act
    let (foreign_wire, foreign_delta) = wire_and_delta(&foreign, None, count);
    let (control_wire, control_delta) = wire_and_delta(&same_dialect, None, count);

    // Assert
    assert!(
        !foreign_wire.contains("context_management"),
        "{foreign_wire}"
    );
    assert!(!foreign_wire.contains("zz-mcp-tell"), "{foreign_wire}");
    assert_eq!(foreign_delta, 1);
    assert!(control_wire.contains("zz-mcp-tell"), "{control_wire}");
    assert_eq!(control_delta, 0);
}

#[test]
fn a_chat_ingress_service_tier_still_forwards_to_the_openai_compat_wire() {
    // Arrange
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"service_tier": "flex"}));
    let req = with_provenance(req, RequestProvenance::OpenaiIngress);

    // Act
    let (rendered, delta) =
        wire_and_delta(&req, None, || compat_drop_count("foreign_dialect_extra"));

    // Assert
    assert!(rendered.contains("\"service_tier\":\"flex\""), "{rendered}");
    assert_eq!(delta, 0);
}

#[test]
fn a_library_caller_forwards_its_extras_to_the_openai_compat_wire_unchanged() {
    // Arrange -- Anthropic-shape keys a library caller chose to send.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({
        "context_management": {"edits": ["zz-lib-edit"]},
        "top_k": 40,
    }));

    // Act
    let (rendered, delta) =
        wire_and_delta(&req, None, || compat_drop_count("foreign_dialect_extra"));

    // Assert
    assert!(rendered.contains("zz-lib-edit"), "{rendered}");
    assert!(rendered.contains("\"top_k\":40"), "{rendered}");
    assert_eq!(delta, 0);
}

#[test]
fn a_foreign_client_key_is_dropped_while_an_operator_payload_extra_forwards_on_openai_compat() {
    // Arrange -- the router merged the operator's `top_k` and a Responses
    // client's `top_k` and `store` into one bag; the client won `top_k`.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"top_k": 99, "store": true, "seed_note": "op"}));
    req.routectl_internal.operator_payload_extras =
        Some(std::sync::Arc::new(json!({"top_k": 40, "seed_note": "op"})));
    let req = with_provenance(req, RequestProvenance::ResponsesIngress);

    // Act
    let (rendered, delta) =
        wire_and_delta(&req, None, || compat_drop_count("foreign_dialect_extra"));

    // Assert -- the operator's value ships, the client's never does.
    let body: serde_json::Value = serde_json::from_str(&rendered).expect("json");
    assert_eq!(body["top_k"], 40, "{rendered}");
    assert_eq!(body["seed_note"], "op", "{rendered}");
    assert!(body.get("store").is_none(), "{rendered}");
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
    // leftover object is withheld and counts on neither extras class, though
    // the Anthropic ingress sent it.
    let mut req = simple_req("m");
    req.provider_extras = Some(json!({"output_config": {"format": {"type": "json_object"}}}));
    let req = with_provenance(req, RequestProvenance::AnthropicIngress);
    let drops = || compat_drop_count("foreign_dialect_extra");
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

// ---------------------------------------------------------------------------
// thinking_type_unrepresentable
// ---------------------------------------------------------------------------

#[test]
fn an_unmodeled_thinking_type_counts_one_openai_compat_drop() {
    for (row, carrier, expected) in crate::translation_drop_metrics::thinking_carrier_rows() {
        // Arrange
        let mut req = simple_req("m");
        req.routectl_internal.anthropic_thinking = carrier;

        // Act
        let (rendered, delta) = wire_and_delta(&req, None, || {
            compat_drop_count("thinking_type_unrepresentable")
        });

        // Assert
        let unmodeled = crate::translation_drop_metrics::UNMODELED_THINKING_TYPE;
        assert!(!rendered.contains(unmodeled), "{row}: {rendered}");
        assert!(
            rendered.contains("hello"),
            "{row}: the turn ships: {rendered}"
        );
        assert_eq!(delta, expected, "{row}");
    }
}
