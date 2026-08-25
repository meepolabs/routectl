//! Cache-control round-trip plus max_tokens, stop, and temperature request mapping.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn cache_control_on_user_text_block_round_trips_to_wire() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look at this".into(),
                citations: None,
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    // also set top-level for autocache
    req.cache_control = Some(CacheControl::ephemeral_5m());

    let body = provider.normalize_request(&req).unwrap();

    // Wire body must carry cache_control on the user text block.
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "look at this");
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(blocks[0]["cache_control"]["ttl"], "5m");
    // top-level cache_control survives.
    assert_eq!(body["cache_control"]["ttl"], "5m");
}

#[test]
fn system_blocks_with_cache_control_emit_array_form() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "system prompt with cache".into(),
        cache_control: Some(CacheControl::ephemeral_1h()),
        citations: None,
    }]));

    let body = provider.normalize_request(&req).unwrap();
    let arr = body["system"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "text");
    assert_eq!(arr[0]["cache_control"]["ttl"], "1h");
}

#[test]
fn anthropic_beta_array_round_trips_into_body() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    req.anthropic_beta = vec!["context-1m-2025-08-07".into(), "future-flag".into()];
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(
        body["anthropic_beta"],
        json!(["context-1m-2025-08-07", "future-flag"])
    );
}

#[test]
fn typed_custom_tool_with_cache_control_serializes_correctly() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "lookup".into(),
        description: Some("look up docs".into()),
        input_schema: json!({"type": "object", "properties": {}}),
        cache_control: Some(CacheControl::ephemeral_1h()),
        defer_loading: Some(true),
        strict: Some(true),
        type_tag: None,
    })]);
    let body = provider.normalize_request(&req).unwrap();
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "lookup");
    assert_eq!(tool["description"], "look up docs");
    assert_eq!(tool["cache_control"]["ttl"], "1h");
    assert_eq!(tool["defer_loading"], true);
    assert_eq!(tool["strict"], true);
}

#[test]
fn cache_control_on_builtin_tool_counts_against_breakpoint_cap() {
    // A Builtin tool's `cache_control` (extracted from its raw JSON) must
    // count toward the 4-breakpoint cap and TTL ordering. Otherwise an
    // invalid request reaches upstream and 400s there. Pin the
    // routectl-side validator catches it.
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "a".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "b".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "c".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "d".into(),
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    // Fifth breakpoint: a Builtin tool carrying cache_control in raw
    // JSON. The validator must extract this value and count it.
    req.tools = Some(vec![ToolDef::Other(json!({
        "type": "bash_20250124",
        "name": "bash",
        "cache_control": {"type": "ephemeral", "ttl": "5m"}
    }))]);
    let err = provider
        .normalize_request(&req)
        .expect_err("expected breakpoint cap violation");
    let msg = format!("{err}");
    assert!(
        msg.contains("breakpoints") && msg.contains("maximum"),
        "expected breakpoint-cap error message, got: {msg}"
    );
}

#[test]
fn anthropic_builtin_tool_passes_through_verbatim() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    req.tools = Some(vec![ToolDef::Other(json!({
        "type": "bash_20250124",
        "name": "bash",
        "cache_control": {"type": "ephemeral", "ttl": "5m"}
    }))]);
    let body = provider.normalize_request(&req).unwrap();
    let tool = &body["tools"][0];
    // Builtin shape preserved exactly.
    assert_eq!(tool["type"], "bash_20250124");
    assert_eq!(tool["name"], "bash");
    assert_eq!(tool["cache_control"]["ttl"], "5m");
}

#[test]
fn unknown_content_block_type_passes_through_verbatim() {
    let provider = make_provider("https://api.anthropic.com");
    let extras: serde_json::Map<String, Value> = [
        ("id".to_string(), json!("srvtu_01")),
        ("name".to_string(), json!("web_search")),
        ("input".to_string(), json!({"query": "rust"})),
    ]
    .into_iter()
    .collect();
    let req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Other {
                type_tag: "server_tool_use".into(),
                cache_control: Some(CacheControl::ephemeral_1h()),
                extras,
            }]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    );
    let body = provider.normalize_request(&req).unwrap();
    let block = &body["messages"][0]["content"][0];
    assert_eq!(block["type"], "server_tool_use");
    assert_eq!(block["id"], "srvtu_01");
    assert_eq!(block["name"], "web_search");
    assert_eq!(block["input"]["query"], "rust");
    assert_eq!(block["cache_control"]["ttl"], "1h");
}

#[test]
fn req_system_field_wins_system_field_and_role_system_turn_is_forwarded() {
    let provider = make_provider("https://api.anthropic.com");
    // Both present: a canonical req.system AND a Role::System message in the
    // array. The canonical system owns the wire `system` field, and the
    // system turn rides the messages array in place -- the legacy lift never
    // ran, so nothing else carries it.
    //
    // Index 0 puts the system turn before a USER turn, which is an ILLEGAL
    // upstream position (a wire system turn must precede an assistant turn or
    // end the array). Forwarding it anyway is deliberate: routectl does not
    // repair what the client sent, and a 400 naming the position is louder
    // and more actionable than a silent deletion.
    let mut req = base_req(
        "claude-opus-4-7",
        vec![system_msg("mid-conversation system"), user_msg("hi")],
    );
    req.system = Some(SystemContent::Text("structured top-level system".into()));
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(body["system"], "structured top-level system");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "got: {body}");
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "mid-conversation system");
    assert_eq!(msgs[1]["role"], "user");
}

#[test]
fn multi_turn_signature_preserved_in_assistant_message() {
    let provider = make_provider("https://api.anthropic.com");

    // Simulate an assistant turn that came back with a thinking block.
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("rd_01".into()),
        format: Some("anthropic-claude-v1".into()),
        index: Some(0),
        payload: json!({"text": "I reasoned about it", "signature": "sig_preserve_me"}),
    };
    let assistant_msg = Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("Sure!".into()),
        reasoning: None,
        reasoning_details: vec![detail],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };

    let req = base_req(
        "claude-3-opus",
        vec![
            user_msg("Think about X"),
            assistant_msg,
            user_msg("Continue"),
        ],
    );
    let body = provider.normalize_request(&req).unwrap();
    let msgs = body["messages"].as_array().unwrap();

    // Find the assistant message
    let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
    let content = asst["content"].as_array().unwrap();

    // Must have a thinking block with the signature
    let thinking_block = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("thinking block missing");
    assert_eq!(thinking_block["signature"], "sig_preserve_me");
    assert_eq!(thinking_block["thinking"], "I reasoned about it");
}

#[test]
fn tool_role_parts_are_translated_to_anthropic_blocks() {
    let provider = make_provider("https://api.anthropic.com");
    let req = base_req(
        "claude-opus-4-7",
        vec![Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ImageUrl {
                image_url: json!({"url": "https://example.com/img.png"}),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some("toolu_01".into()),
            tool_calls: None,
        }],
    );
    let body = provider.normalize_request(&req).unwrap();
    let block = &body["messages"][0]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["content"][0]["type"], "image");
    assert_eq!(block["content"][0]["source"]["type"], "url");
    assert_eq!(
        block["content"][0]["source"]["url"],
        "https://example.com/img.png"
    );
}

#[test]
fn max_tokens_defaults_to_routectl_internal_when_not_set() {
    // v0.8: when the caller omits `max_tokens` AND no per-model
    // override is set on `routectl_internal.max_output_tokens`,
    // the anthropic-api egress falls back to its hardcoded 64000
    // baseline.
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.max_tokens = None;
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(body["max_tokens"], 64_000u64);
}

/// When `req.max_tokens` is None but
/// `req.routectl_internal.max_output_tokens` is set by the
/// router from the per-model `[models.X].max_output_tokens`
/// override, the wire body's `max_tokens` field must reflect the
/// internal value. Pins the contract that the egress reads the
/// carrier rather than always landing the hardcoded baseline.
#[test]
fn max_tokens_reflects_routectl_internal_when_caller_omitted() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.max_tokens = None;
    req.routectl_internal.max_output_tokens = 8000;
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(body["max_tokens"], 8000u64);
}

/// req.max_tokens wins over the routectl_internal value
/// (good-translator priority: the caller's explicit ask is
/// honored). The internal carrier is only consulted when
/// req.max_tokens is None.
#[test]
fn req_max_tokens_wins_over_routectl_internal() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.max_tokens = Some(1024);
    req.routectl_internal.max_output_tokens = 8000;
    let body = provider.normalize_request(&req).unwrap();
    assert_eq!(body["max_tokens"], 1024u64);
}

#[test]
fn stop_translated_to_stop_sequences() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.stop = Some(vec!["STOP".into(), "END".into()]);
    let body = provider.normalize_request(&req).unwrap();
    let seqs = body["stop_sequences"].as_array().unwrap();
    assert_eq!(seqs[0], "STOP");
    assert_eq!(seqs[1], "END");
}

#[test]
fn reasoning_enabled_sets_temperature_to_one() {
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req("claude-3-opus", vec![user_msg("hi")]);
    req.temperature = Some(0.5);
    req.reasoning = Some(ReasoningConfig {
        max_tokens: Some(1000),
        ..Default::default()
    });
    let body = provider.normalize_request(&req).unwrap();
    // When thinking is enabled, Anthropic requires temperature=1.
    assert_eq!(body["temperature"], 1.0f64);
}
