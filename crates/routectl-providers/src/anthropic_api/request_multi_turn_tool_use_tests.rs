use super::*;
use base64::{Engine, engine::general_purpose::STANDARD as B64_STANDARD};
use routectl_core::{ChatRequest, CoreHistoryReasoning, Message, Role};
use serde_json::json;

/// A genuine Claude-shaped thinking signature: E-prefixed base64 of a
/// payload whose first byte is 0x12. The egress strip preserves only
/// Claude-shaped signatures, so test fixtures that must survive the
/// strip use this rather than an arbitrary placeholder string.
fn claude_signature() -> String {
    B64_STANDARD.encode([0x12u8, 0x34, 0x56, 0x78])
}

/// A distinct Claude-shaped signature, varied by a trailing byte so two
/// surviving thinking blocks in one fixture stay distinguishable.
fn claude_signature_variant(tag: u8) -> String {
    B64_STANDARD.encode([0x12u8, 0x34, 0x56, tag])
}

fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_msg(text: &str, tool_calls: Option<Vec<Value>>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls,
    }
}

/// On a multi-turn assistant turn, `Message.tool_calls` (the
/// canonical OpenAI-shape representation produced by
/// `walk_content_blocks` on the response side) must be re-emitted
/// as Anthropic `ContentBlock::ToolUse` entries. Without this,
/// echoing a canonical Message back through the Anthropic egress
/// drops the tool_use blocks and the next user `tool_result` turn
/// fails upstream with "tool_use ids were found without preceding
/// tool_use blocks".
#[test]
fn assistant_message_with_tool_calls_emits_tool_use_blocks() {
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("calculate 2+2"),
            assistant_msg(
                "Let me calculate.",
                Some(vec![json!({
                    "id": "toolu_abc123",
                    "type": "function",
                    "function": {
                        "name": "calc",
                        "arguments": "{\"expr\":\"2+2\"}",
                    }
                })]),
            ),
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
    let assistant = messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .expect("assistant message must be present");
    let blocks = assistant
        .get("content")
        .and_then(|v| v.as_array())
        .expect("assistant content must be Blocks form when tool_calls present");

    let tool_use = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
        .expect("assistant must carry a tool_use block on multi-turn replay");
    assert_eq!(tool_use["id"], "toolu_abc123");
    assert_eq!(tool_use["name"], "calc");
    assert_eq!(tool_use["input"], json!({"expr": "2+2"}));
}

#[test]
fn strips_unsigned_thinking_block_keeps_other_blocks() {
    // Multi-turn input with [text, signed_thinking, unsigned_thinking,
    // tool_use] -> outgoing assistant content has [text,
    // signed_thinking, tool_use]. The unsigned block is dropped;
    // every other content part survives unmodified.
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("compute 2+2"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "Let me think.".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "signed analysis".into(),
                        signature: Some(claude_signature()),
                    }),
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "unsigned analysis".into(),
                        signature: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "toolu_1".into(),
                        name: "calc".into(),
                        input: json!({"expr": "2+2"}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
    let assistant = messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .expect("assistant message present");
    let blocks = assistant
        .get("content")
        .and_then(|v| v.as_array())
        .expect("assistant content is Blocks form");

    let types: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        types,
        vec!["text", "thinking", "tool_use"],
        "expected unsigned thinking dropped, others preserved; got {types:?}"
    );

    // The signed thinking block survives with its signature intact.
    let signed = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
        .unwrap();
    assert_eq!(signed["signature"], claude_signature());

    // Other survivors keep their fields.
    let tool_use = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
        .unwrap();
    assert_eq!(tool_use["id"], "toolu_1");
    assert_eq!(tool_use["name"], "calc");
}

#[test]
fn passes_through_when_all_thinking_signed() {
    // No mutation when every thinking block carries a signature.
    // Pin: signed-only histories must produce the same body the
    // pre-strip code produced.
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "first".into(),
                        signature: Some(claude_signature_variant(0x01)),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "second".into(),
                        signature: Some(claude_signature_variant(0x02)),
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let assistant = body
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        })
        .unwrap();
    let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
    let types: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        types,
        vec!["thinking", "text", "thinking"],
        "all blocks pass through unchanged when every thinking is signed"
    );
    assert_eq!(blocks[0]["signature"], claude_signature_variant(0x01));
    assert_eq!(blocks[2]["signature"], claude_signature_variant(0x02));
}

#[test]
fn drops_assistant_message_when_only_block_was_unsigned_thinking() {
    // When stripping leaves the assistant message with content: []
    // AND the message has no reasoning_details / tool_calls to fill
    // the wire content array, drop the whole message. Anthropic's
    // wire spec rejects content: []; emitting it would just trade
    // one 400 for another.
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("hello"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Thinking {
                        thinking: "let me think".into(),
                        signature: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("any update?"),
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
    // The empty-after-strip assistant message is gone; only the
    // two user messages remain.
    assert_eq!(
        messages.len(),
        2,
        "empty-after-strip assistant message must be dropped, got: {messages:?}"
    );
    let assistant_present = messages
        .iter()
        .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"));
    assert!(
        !assistant_present,
        "no assistant message must remain when its only block was an unsigned thinking, \
         got: {messages:?}"
    );
}

#[test]
fn keeps_message_with_only_unsigned_thinking_when_tool_calls_present() {
    // Pin the corner: stripping leaves Parts empty BUT the message
    // carries tool_calls. The wire content array still gets blocks
    // from `emit_tool_use_blocks_from_calls`, so the message must
    // be kept (don't drop the tool_calls along with the empty Parts).
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Thinking {
                        thinking: "let me think".into(),
                        signature: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "toolu_xyz",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{\"x\":1}"}
                })]),
            },
        ]
        .into(),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
    let assistant = messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .expect("assistant message must survive when tool_calls fill content");
    let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
    let has_tool_use = blocks
        .iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"));
    assert!(
        has_tool_use,
        "tool_use block must reach the wire from tool_calls; got: {blocks:?}"
    );
    // Pin id + name so a translation regression that emits a
    // tool_use block with the wrong identity still fails.
    let tool_block = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
    assert_eq!(tool_block["id"], "toolu_xyz");
    assert_eq!(tool_block["name"], "calc");
    // No thinking block leaks through; the unsigned was dropped.
    let has_thinking = blocks
        .iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"));
    assert!(
        !has_thinking,
        "unsigned thinking must not appear; got: {blocks:?}"
    );
}

#[test]
fn emits_warn_when_stripping_occurs() {
    // Capture the WARN log emitted during normalize and assert:
    // - structured fields `provider`, `dropped_blocks`,
    //   `affected_messages` are present
    // - block content (the `thinking` text) is NEVER logged --
    //   could be reasoning over sensitive data.
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "TOPSECRET-REASONING-PAYLOAD".into(),
                        signature: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    let captured = routectl_testkit::capture_events(|| {
        normalize("provider-x", &req, false, &[], false, None).expect("normalize succeeds");
    });

    let strip_event = captured
        .iter()
        .find(|e| e.message.contains("stripping unsigned thinking blocks"))
        .unwrap_or_else(|| panic!("expected strip WARN, got events: {captured:?}"));
    assert_eq!(strip_event.level, tracing::Level::WARN);

    // Structured fields present.
    let field_keys: Vec<&str> = strip_event.fields.iter().map(|(k, _)| k.as_str()).collect();
    for key in &["provider", "dropped_blocks", "affected_messages"] {
        assert!(
            field_keys.contains(key),
            "expected field `{key}` in WARN, got fields: {:?}",
            strip_event.fields
        );
    }
    let provider_value = strip_event
        .fields
        .iter()
        .find(|(k, _)| k == "provider")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(provider_value, "provider-x");

    // Block content must not appear anywhere in the captured events.
    for evt in &captured {
        assert!(
            !evt.message.contains("TOPSECRET-REASONING-PAYLOAD"),
            "thinking block content leaked into log message: {evt:?}"
        );
        for (_, v) in &evt.fields {
            assert!(
                !v.contains("TOPSECRET-REASONING-PAYLOAD"),
                "thinking block content leaked into log fields: {evt:?}"
            );
        }
    }
}

#[test]
fn tool_message_without_tool_call_id_is_rejected() {
    // Anthropic requires `tool_result` to reference the
    // `tool_use.id` it answers. An empty / missing
    // `tool_call_id` on a Role::Tool message used to fall
    // through as `unwrap_or_default()` (empty string) and
    // upstream returned a vague 400. Reject locally with a
    // precise NormalizeRequest error.
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Text("result content".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    };
    let err = normalize("test-anthropic", &req, false, &[], false, None).unwrap_err();
    assert!(
        err.to_string().contains("tool_call_id"),
        "must mention tool_call_id; got: {err}"
    );
}

#[test]
fn unsigned_thinking_block_is_stripped_not_rejected() {
    // Regression: prior behavior was a HTTP 400
    // ("thinking block without signature"). New behavior STRIPS
    // the unsigned block from the outgoing body and forwards the
    // rest. Cross-provider fallback (deepseek -> Anthropic) and
    // SDKs that fail to round-trip the signature field rely on
    // this -- a hard reject would 400 every multi-turn after
    // such a turn.
    use routectl_core::{ContentPart, KnownContentPart};
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![
            user_msg("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "let me think".into(),
                        signature: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };
    // Must NOT error: the new behavior is to strip the unsigned
    // block, not reject the request.
    let body = normalize("test-anthropic", &req, false, &[], false, None).expect(
        "normalize must accept the request and strip the unsigned block; \
         a hard reject would regress the cross-provider fallback path",
    );
    let assistant = body
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        })
        .unwrap();
    let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
    // Only the text block survives; the unsigned thinking is dropped.
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "text");
}

#[test]
fn assistant_tool_call_with_unparseable_arguments_wraps_under_underscore() {
    // Defensive fallback: a tool_call.arguments string that
    // isn't valid JSON shouldn't silently produce a malformed
    // upstream body. We wrap under {"_arguments": "..."} and
    // emit a WARN, so the upstream returns a useful error.
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![assistant_msg(
            "",
            Some(vec![json!({
                "id": "toolu_xyz",
                "type": "function",
                "function": {"name": "calc", "arguments": "this is not json"}
            })]),
        )]
        .into(),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
    let assistant = messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        .unwrap();
    let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
    let tool_use = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
        .unwrap();
    assert_eq!(tool_use["input"], json!({"_arguments": "this is not json"}));
}

/// Ordering invariant (tool_choice-forces-use path): thinking is composed
/// -- forcing `temperature=1.0` and dropping `top_p` -- and then
/// `strip_thinking_when_tool_choice_forces_use` removes `thinking` because
/// tool_choice forces tool use. Sampling params must be recomputed from
/// the SOURCE request: `temperature == req.temperature` (None here, so
/// absent) and `top_p` re-emitted per the else-branch (present because
/// temperature is None). This is the documented Claude Code WebSearch
/// production path.
#[test]
fn normalize_recomputes_sampling_after_tool_choice_forces_use_strip() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("search the web")].into(),
        max_tokens: Some(2048),
        temperature: None,
        top_p: Some(0.9),
        tool_choice: Some(json!("required")),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped when tool_choice forces tool use; got: {body}"
    );
    assert!(
        body.get("temperature").is_none(),
        "temperature must revert to the caller's None once thinking is stripped; got: {body}"
    );
    assert_eq!(
        body.get("top_p").and_then(Value::as_f64),
        Some(0.9),
        "top_p must be re-emitted per the else-branch once thinking is stripped; got: {body}"
    );
}

/// With `adaptive = true`, the wire shape is the
/// Opus 4.7+ form -- `thinking: {type:"adaptive"}` (no
/// `budget_tokens`) plus a top-level `output_config: {effort:...}`
/// carrying the canonical `reasoning.effort` string verbatim.
#[test]
fn adaptive_emits_adaptive_shape_with_output_config() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("xhigh".into()),
            max_tokens: Some(8000),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();

    // thinking serializes to {"type":"adaptive"} -- no budget_tokens.
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "adaptive");
    assert!(
        thinking.get("budget_tokens").is_none(),
        "adaptive shape must not carry budget_tokens, got {thinking:?}"
    );

    // output_config carries the effort verbatim.
    let oc = body.get("output_config").expect("output_config present");
    assert_eq!(oc["effort"], "xhigh");

    // Anthropic requires temperature == 1.0 with thinking active --
    // both Enabled and Adaptive variants trigger the same constraint.
    assert_eq!(body["temperature"], 1.0);
}

/// End-to-end: a ChatRequest whose canonical `reasoning.effort` was
/// set (as the OpenAI ingress does when promoting a top-level
/// `reasoning_effort`) must compose thinking on the egress AND carry
/// no stray top-level `reasoning_effort` key. Proves both halves of
/// the fix: thinking composed + leak gone.
#[test]
fn reasoning_effort_composes_thinking_and_does_not_leak() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    // Thinking composed from the effort string.
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "enabled");

    // No stray reasoning_effort key leaked into the egress body.
    assert!(
        body.get("reasoning_effort").is_none(),
        "reasoning_effort must not leak into egress body, got {body:?}"
    );
}

/// `reasoning.effort == "none"` must disable thinking (the
/// `thinking` field emits `{"type":"disabled"}`, not a budget) and
/// never leak a top-level `reasoning_effort` key.
#[test]
fn reasoning_effort_none_disables_thinking_and_does_not_leak() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: Some("none".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    // Disabled thinking emits the disabled shape, not a budget.
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "disabled");
    assert!(
        thinking.get("budget_tokens").is_none(),
        "disabled thinking must not carry a budget, got {thinking:?}"
    );
    assert!(
        body.get("reasoning_effort").is_none(),
        "reasoning_effort must not leak into egress body, got {body:?}"
    );
}
/// shape is the legacy `Enabled { budget_tokens }` form. Older
/// Claude models (4.5/4.6 family) still want this shape and would
/// 400 on the adaptive form.
#[test]
fn legacy_thinking_unchanged_when_flag_false() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "enabled");
    // table("high")=24576 clamped to window ceiling max_tokens-1 = 2047.
    assert_eq!(thinking["budget_tokens"], 2047);

    // No output_config on the legacy path.
    assert!(
        body.get("output_config").is_none(),
        "legacy shape must not emit output_config, got {body:?}"
    );

    assert_eq!(body["temperature"], 1.0);
}

/// `effort = "max"` on the legacy path maps via the exact table to
/// 128000, which the `[1024, max_tokens-1]` window clamps down to
/// `max_tokens - 1`. The adaptive path passes "max" verbatim into
/// `output_config.effort` and never consults the table. This test
/// pins the legacy mapping so a non-adaptive provider receiving
/// `max` from the canonical surface still produces a serializable
/// body.
#[test]
fn effort_max_maps_to_window_ceiling_legacy_path() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2000),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    let thinking = body.get("thinking").unwrap();
    assert_eq!(thinking["type"], "enabled");
    // table("max")=128000 clamped to window ceiling max_tokens-1 = 1999.
    assert_eq!(thinking["budget_tokens"], 1999);
}

/// `reasoning.effort = "none"` produces `Disabled` on both
/// paths. The adaptive flag does not coerce a Disabled into an
/// Adaptive -- if the caller said no thinking, we honor it.
#[test]
fn disabled_thinking_unchanged_under_adaptive_flag() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(512),
        reasoning: Some(ReasoningConfig {
            effort: Some("none".into()),
            max_tokens: None,
            exclude: None,
            enabled: None,
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
    let thinking = body.get("thinking").unwrap();
    assert_eq!(thinking["type"], "disabled");
    assert!(body.get("output_config").is_none());
}

/// The barefoot adaptive case -- `reasoning.enabled = true`
/// with no effort and no budget. Adaptive shape applies; effort
/// defaults to "medium". This is the only path where
/// `derive_effort` returns the fallback string, so we pin it
/// explicitly. (Without this test the default would silently
/// drift if anyone changed `derive_effort`.)
#[test]
fn adaptive_defaults_effort_to_medium_when_unset() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "medium");
}

/// When `adaptive = true` AND the caller sets an
/// explicit `reasoning.max_tokens`, the budget is dropped (the
/// adaptive wire shape has no field for it) and a tracing::warn
/// fires at normalize time. We can't easily assert the warn in a
/// unit test without `tracing-test`, but we CAN pin that the
/// resulting body is the adaptive shape with the caller's
/// effort string (or "medium" fallback), with no budget_tokens
/// leaking into the wire.
#[test]
fn adaptive_drops_max_tokens_silently() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("low".into()),
            max_tokens: Some(8000),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    // budget_tokens MUST NOT leak into the adaptive shape.
    assert!(
        body["thinking"].get("budget_tokens").is_none(),
        "adaptive shape must not carry budget_tokens, got {body:?}"
    );
    // The caller's effort string survives even though the budget
    // was dropped.
    assert_eq!(body["output_config"]["effort"], "low");
}

/// Real claude-code probe shape: `max_tokens=64` + operator
/// `effort="high"`. The legacy `Enabled` wire shape would emit
/// `budget_tokens=51` (64*0.80) which Anthropic 400s on the
/// `budget_tokens >= 1024` validator. routectl must drop thinking
/// for this request rather than emit a body that cannot succeed.
/// Caller's `max_tokens` is preserved verbatim.
#[test]
fn small_max_tokens_drops_legacy_thinking() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("thinking").is_none(),
        "thinking must be absent on probe-sized legacy requests, got {body:?}"
    );
    assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
}

/// Companion of the effort=high case for `effort="medium"` (ratio
/// 0.50): `max_tokens=64` derives `budget_tokens=32`, well below
/// the 1024 floor. routectl must drop thinking; caller's
/// `max_tokens` is preserved verbatim (the contract that motivated
/// rejecting clamp-and-raise).
#[test]
fn small_max_tokens_drops_legacy_thinking_effort_medium() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("medium".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("thinking").is_none(),
        "thinking must be absent on effort=medium probe-sized legacy requests, got {body:?}"
    );
    assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
}

/// Companion for `effort="xhigh"` (ratio 0.95): `max_tokens=64`
/// derives `budget_tokens=60`, still well below the 1024 floor.
/// Even at the highest sub-`max` ratio the gate must fire and
/// the caller's `max_tokens` survives unchanged.
#[test]
fn small_max_tokens_drops_legacy_thinking_effort_xhigh() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("xhigh".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("thinking").is_none(),
        "thinking must be absent on effort=xhigh probe-sized legacy requests, got {body:?}"
    );
    assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
}

/// Variant of the above with an explicit sub-1024 `reasoning
/// .max_tokens`. Even an explicit caller budget must be dropped
/// when `max_tokens` cannot carry it: emitting `Enabled
/// { budget_tokens: 500 }` would still 400.
#[test]
fn small_max_tokens_drops_thinking_with_explicit_budget() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(500),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert!(body.get("thinking").is_none());
}

/// The adaptive shape is unaffected by the legacy floor: probe-
/// sized `max_tokens` still receives adaptive thinking because
/// the wire has no `budget_tokens` field and no Anthropic minimum
/// to violate. Pins that the new gate is legacy-only.
#[test]
fn small_max_tokens_keeps_adaptive() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

/// `effort="high"` on `max_tokens=1100` looks up the exact table
/// (24576), which the `[1024, max_tokens-1]` window then clamps down
/// to the ceiling `max_tokens-1 = 1099`. 1099 < 1100 holds, so
/// Anthropic's `max_tokens > budget_tokens` constraint is satisfied;
/// visible-output budget shrinks to 1. Pins the ceiling clamp on the
/// effort path in the just-above-floor band.
#[test]
fn effort_budget_ceiling_clamped_in_carryable_band() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1100),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 1099);
}

/// Boundary: `max_tokens=1025` is the smallest value the gate
/// admits (`max > MIN`, not `max >= MIN`). Pins the off-by-one
/// and confirms the clamp lands at exactly 1024.
#[test]
fn exactly_1025_max_tokens_keeps_thinking() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1025),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
}

/// Anthropic also requires `max_tokens > budget_tokens`. A caller
/// who sends an explicit `reasoning.max_tokens` larger than
/// `req.max_tokens` would otherwise produce a wire body that
/// 400s. The clamp caps the budget at `max_tokens - 1`, leaving
/// at least one visible-output token. Pins that the cap fires on
/// the explicit-budget arm.
#[test]
fn explicit_budget_above_max_tokens_capped_to_max_minus_one() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1100),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(1200),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 1099);
    // Anthropic invariant: max_tokens > budget_tokens.
    assert_eq!(body["max_tokens"], 1100);
}

/// Caller's `reasoning.max_tokens` of 500 sits BELOW the
/// Anthropic floor (1024). With `req.max_tokens=2048` the gate
/// accepts, and the per-arm clamp raises the budget to 1024.
/// Pins the silent-promotion behavior on the explicit arm; the
/// accompanying WARN is observable in production via
/// `ROUTECTL_LOG=routectl=warn`.
#[test]
fn explicit_budget_below_floor_clamped_up_to_min() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(500),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
}

/// `reasoning.enabled = false` short-circuits to `Disabled`
/// before the new gate runs. Without this pin, a future refactor
/// that moved the gate above the `enabled=false` check would
/// silently rewrite an explicit opt-out into absent-thinking.
#[test]
fn explicit_disable_wins_over_small_max_tokens() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(false),
        }),
        ..Default::default()
    };
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "disabled");
}

/// Tool-choice translation lives in the egress (different upstreams
/// want different shapes; the OpenAI ingress passes wire `tool_choice`
/// through verbatim). Pin the canonical -> Anthropic mapping for
/// every shape we expect callers to send.
#[test]
fn tool_choice_string_auto_translates_to_anthropic_object() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(json!("auto")),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert_eq!(body["tool_choice"], json!({"type":"auto"}));
}

#[test]
fn tool_choice_string_required_translates_to_any() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(json!("required")),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert_eq!(body["tool_choice"], json!({"type":"any"}));
}

#[test]
fn tool_choice_string_none_drops_field() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(json!("none")),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
        "expected tool_choice dropped, got: {body:?}"
    );
    assert!(
        body.get("tools").is_none() || body["tools"].is_null(),
        "expected no tools field when caller sent neither tools nor tool_choice"
    );
}

/// `tool_choice = "none"` plus `tools` present must drop BOTH on the
/// Anthropic wire. Anthropic has no native "none" -- if we send the
/// tools but no tool_choice, Anthropic defaults to auto-select and
/// the caller's "do not call tools" intent silently flips to "auto".
#[test]
fn tool_choice_none_with_tools_strips_tools_too() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(json!("none")),
        tools: Some(vec![routectl_core::ToolDef::Custom(
            routectl_core::CustomTool {
                name: "get_weather".into(),
                description: Some("weather lookup".into()),
                input_schema: json!({"type":"object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            },
        )]),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert!(
        body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
        "expected tool_choice dropped, got: {body:?}"
    );
    assert!(
        body.get("tools").is_none() || body["tools"].is_null(),
        "expected tools dropped alongside tool_choice=none, got: {body:?}"
    );
}

#[test]
fn tool_choice_function_object_translates_to_anthropic_tool() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(json!({"type":"function","function":{"name":"get_weather"}})),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert_eq!(
        body["tool_choice"],
        json!({"type":"tool","name":"get_weather"})
    );
}

/// Anthropic-shape tool_choice (e.g. from claude-code via Anthropic
/// ingress) must passthrough verbatim. Without this, the Anthropic
/// ingress -> Anthropic egress path would double-translate and
/// silently corrupt the field.
#[test]
fn tool_choice_already_anthropic_shape_passes_through_verbatim() {
    for tc in [
        json!({"type":"auto"}),
        json!({"type":"any"}),
        json!({"type":"tool","name":"X"}),
        json!({"type":"none"}),
    ] {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")].into(),
            tool_choice: Some(tc.clone()),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], tc, "expected passthrough for {tc:?}");
    }
}

/// Unknown shapes are not coerced; let the upstream surface its
/// own error. The OpenAI ingress still passes them through the
/// canonical body, so the egress sees them here.
#[test]
fn tool_choice_unknown_object_passes_through_verbatim() {
    let weird = json!({"type":"some_future_mode","extra":"bag"});
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        tool_choice: Some(weird.clone()),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert_eq!(body["tool_choice"], weird);
}

/// `output_config` arriving via `provider_extras` (the path used
/// by the Anthropic ingress for structured-output requests) is
/// merged into the upstream body so `output_config.format` reaches
/// api.anthropic.com unchanged. The egress doesn't need a
/// dedicated field for this -- the provider_extras allow-list
/// already lets `output_config` through.
#[test]
fn structured_output_format_merges_from_provider_extras() {
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object"}
                }
            }
        })),
        ..Default::default()
    };
    let body = normalize("test", &req, false, &[], false, None).unwrap();
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
}

/// Review follow-up to Bug K: when the provider is NOT adaptive
/// (Sonnet, Haiku -- no adaptive capability declared), the
/// `output_config.effort` field set by cc must be stripped from
/// the outgoing body. Anthropic 400s with "This model does not
/// support the effort parameter" otherwise.
#[test]
fn output_config_effort_stripped_on_non_adaptive_provider() {
    let req = ChatRequest {
        model: "claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        provider_extras: Some(json!({
            "output_config": {"effort": "high"}
        })),
        ..Default::default()
    };
    let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
    // effort stripped; output_config now empty, so the whole
    // object is removed for wire cleanliness.
    assert!(
        body.get("output_config").is_none(),
        "non-adaptive provider must have output_config removed when effort \
         was the only sub-key, got body: {body}",
    );
}

/// Companion to the above: when output_config carries BOTH effort
/// and a structured-output `format` field, the strip removes only
/// effort; `format` is preserved (orthogonal to the effort beta
/// and supported across the model family).
#[test]
fn output_config_effort_stripped_preserves_sibling_format_on_non_adaptive() {
    let req = ChatRequest {
        model: "claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        provider_extras: Some(json!({
            "output_config": {
                "effort": "high",
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "required": ["x"]}
                }
            }
        })),
        ..Default::default()
    };
    let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
    let oc = body.get("output_config").expect("output_config preserved");
    assert!(oc.get("effort").is_none(), "effort stripped: {oc}");
    assert_eq!(oc["format"]["type"], "json_schema");
    assert_eq!(oc["format"]["schema"]["required"][0], "x");
}

/// Adaptive providers (Opus 4.7 with supports_adaptive_thinking=true)
/// must preserve `output_config.effort` -- the model accepts it. Pin
/// this so a future refactor doesn't accidentally strip on the
/// adaptive path too. The request carries `reasoning` so adaptive
/// thinking is actually composed: the late enforcer keys off the
/// assembled body's `thinking.type == adaptive`, so effort is only
/// valid (and preserved) when thinking is genuinely present.
#[test]
fn output_config_effort_preserved_on_adaptive_provider() {
    use routectl_core::ReasoningConfig;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(64),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {"effort": "high"}
        })),
        ..Default::default()
    };
    let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    let oc = body.get("output_config").expect("output_config preserved");
    assert_eq!(oc["effort"], "high");
}

// -----------------------------------------------------------------
// tool_choice + thinking conflict resolution
//
// Anthropic's extended-thinking docs explicitly forbid pairing
// `thinking` with a `tool_choice` value that forces tool use:
// `{"type":"any"}` or `{"type":"tool", "name": "..."}`. The
// Messages API 400s the request with "Thinking may not be enabled
// when tool_choice forces tool use." Real-world trigger: Claude
// Code's WebSearch tool fires sub-requests with
// `tool_choice: {type:"tool", name:"web_search"}` AND
// `thinking: {type:"adaptive"}`. The strip preserves the caller's
// tool_choice (which carries intent) and drops thinking (which is
// a routectl-composed convenience) so the request can complete.
// -----------------------------------------------------------------

/// Helper: build a request with both reasoning (-> thinking) and
/// the provided `tool_choice`. `max_tokens=2048` keeps thinking on
/// the legacy `Enabled` path above the 1024 floor; the legacy and
/// adaptive paths share the same conflict resolution.
fn req_with_thinking_and_tool_choice(tool_choice: Option<Value>) -> ChatRequest {
    use routectl_core::ReasoningConfig;
    ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: Some("medium".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        tool_choice,
        ..Default::default()
    }
}

#[test]
fn tool_choice_any_with_thinking_strips_thinking() {
    // Arrange
    let req = req_with_thinking_and_tool_choice(Some(json!({"type": "any"})));

    // Act
    let body = normalize("test", &req, false, &[], false, None).unwrap();

    // Assert: thinking dropped, tool_choice preserved verbatim.
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped when tool_choice forces tool use, got: {body}"
    );
    assert_eq!(body["tool_choice"], json!({"type": "any"}));
}

#[test]
fn tool_choice_tool_with_thinking_strips_thinking() {
    // Arrange: the Claude Code WebSearch shape that motivated the fix.
    let req =
        req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "web_search"})));

    // Act
    let body = normalize("test", &req, false, &[], false, None).unwrap();

    // Assert: thinking dropped, tool_choice preserved verbatim.
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped when tool_choice.type=tool, got: {body}"
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": "web_search"})
    );
}

#[test]
fn adaptive_thinking_forced_tool_choice_strips_thinking_and_output_config_effort() {
    // Arrange: the adaptive-thinking path emits both `thinking:
    // {type:adaptive}` AND a top-level `output_config: {effort}`.
    // A forcing tool_choice must strip BOTH -- `output_config.effort`
    // is only valid alongside adaptive thinking, so an orphaned
    // effort 400s.
    let req = req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "ws"})));

    // Act: adaptive=true so build_output_config emits output_config.effort.
    let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();

    // Assert: thinking dropped AND the orphaned effort dropped.
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped when tool_choice forces tool use, got: {body}"
    );
    assert!(
        body.get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_none(),
        "output_config.effort must be stripped alongside thinking, got: {body}"
    );
}

#[test]
fn forced_tool_choice_strips_effort_but_preserves_sibling_format() {
    // Arrange: adaptive output_config.effort plus a structured-output
    // `format` sibling layered in via provider_extras. The strip must
    // drop only effort; format is orthogonal and must survive.
    let mut req = req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "ws"})));
    req.provider_extras = Some(json!({
        "output_config": {
            "format": {"type": "json_schema", "schema": {"type": "object"}}
        }
    }));

    // Act
    let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();

    // Assert: thinking + effort gone, format preserved.
    assert!(body.get("thinking").is_none(), "thinking stripped: {body}");
    let oc = body
        .get("output_config")
        .expect("output_config preserved for format");
    assert!(oc.get("effort").is_none(), "effort stripped: {oc}");
    assert_eq!(oc["format"]["type"], "json_schema");
}

#[test]
fn tool_choice_auto_with_thinking_keeps_thinking() {
    // Regression guard: `auto` does not force tool use, so thinking
    // must survive.
    let req = req_with_thinking_and_tool_choice(Some(json!("auto")));

    // translate_tool_choice normalizes bare "auto" -> {"type":"auto"}
    // before strip_thinking_when_tool_choice_forces_use runs.
    let body = normalize("test", &req, false, &[], false, None).unwrap();

    assert_eq!(body["tool_choice"], json!({"type": "auto"}));
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn tool_choice_none_with_thinking_keeps_thinking() {
    // Regression guard: `none` translates to no tool_choice on the
    // wire AND drops the tools array; thinking is unaffected.
    let req = req_with_thinking_and_tool_choice(Some(json!("none")));

    let body = normalize("test", &req, false, &[], false, None).unwrap();

    assert!(
        body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
        "tool_choice=none must drop the field"
    );
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn no_tool_choice_with_thinking_keeps_thinking() {
    // Regression guard: absent tool_choice never triggers the strip.
    let req = req_with_thinking_and_tool_choice(None);

    let body = normalize("test", &req, false, &[], false, None).unwrap();

    assert!(body.get("tool_choice").is_none() || body["tool_choice"].is_null());
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn tool_choice_any_without_thinking_no_op() {
    // Regression guard: when thinking was never composed, the strip
    // is harmless and tool_choice survives.
    let req = ChatRequest {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        tool_choice: Some(json!({"type": "any"})),
        ..Default::default()
    };

    let body = normalize("test", &req, false, &[], false, None).unwrap();

    assert!(body.get("thinking").is_none());
    assert_eq!(body["tool_choice"], json!({"type": "any"}));
}

// ----------------------------------------------------------------
// history_reasoning gating of the unsigned-thinking strip.
//
// deepseek v4's `/anthropic` endpoint (provider kind anthropic-api)
// emits thinking blocks WITHOUT a signature yet 400s the next turn
// unless that thinking is echoed back. `history_reasoning =
// "preserve"` tells the egress to skip the unsigned-thinking strip
// for those endpoints; Auto/Strip/unset keep the real-Anthropic-safe
// strip.
// ----------------------------------------------------------------

/// Build a multi-turn assistant message shaped `[text, thinking,
/// tool_use]`. `signature = None` makes the thinking block unsigned
/// (deepseek shape); `Some(..)` makes it signed.
fn assistant_with_thinking(signature: Option<&str>) -> Message {
    use routectl_core::{ContentPart, KnownContentPart};
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(vec![
            ContentPart::Known(KnownContentPart::Text {
                text: "Let me think.".into(),
                citations: None,
                cache_control: None,
            }),
            ContentPart::Known(KnownContentPart::Thinking {
                thinking: "deepseek reasoning".into(),
                signature: signature.map(std::string::ToString::to_string),
            }),
            ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "calc".into(),
                input: json!({"expr": "2+2"}),
                cache_control: None,
            }),
        ]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// Multi-turn request carrying the given `history_reasoning` policy
/// on the dispatch carrier. `None` mirrors the dispatch default (no
/// per-model policy resolved).
fn req_with_hr(hr: Option<CoreHistoryReasoning>, assistant: Message) -> ChatRequest {
    let mut req = ChatRequest {
        model: "deepseek-chat".into(),
        messages: vec![user_msg("compute 2+2"), assistant].into(),
        ..Default::default()
    };
    req.routectl_internal.history_reasoning = hr;
    req
}

/// Pull the assistant message's wire content blocks from a
/// normalized body.
fn assistant_blocks(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
        })
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_array())
        .cloned()
        .expect("assistant message with Blocks-form content present")
}

fn block_types(blocks: &[Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
        .collect()
}

#[test]
fn preserve_history_reasoning_keeps_unsigned_thinking_for_anthropic_api() {
    // Arrange: deepseek-shape unsigned thinking + history_reasoning =
    // Preserve.
    let req = req_with_hr(
        Some(CoreHistoryReasoning::Preserve),
        assistant_with_thinking(None),
    );

    // Act: normalize under a capture so we can also assert no strip
    // WARN fires.
    let mut body = None;
    let captured = routectl_testkit::capture_events(|| {
        body =
            Some(normalize("deepseek", &req, false, &[], false, None).expect("normalize succeeds"));
    });
    let body = body.expect("normalize ran");

    // Assert: all three blocks survive; the unsigned thinking is
    // preserved (deepseek requires it echoed back).
    let blocks = assistant_blocks(&body);
    assert_eq!(
        block_types(&blocks),
        vec!["text", "thinking", "tool_use"],
        "Preserve must retain the unsigned thinking block"
    );
    let thinking = blocks
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
        .expect("thinking block present under Preserve");
    assert_eq!(thinking["thinking"], "deepseek reasoning");
    // Unsigned: signature serializes as the empty string, not dropped.
    assert_eq!(thinking["signature"], "");

    // No strip => no WARN.
    assert!(
        !captured
            .iter()
            .any(|e| e.message.contains("stripping unsigned thinking blocks")),
        "Preserve must not emit the strip WARN; got events: {captured:?}"
    );
}

#[test]
fn strip_mode_still_strips_unsigned_thinking() {
    // Arrange.
    let req = req_with_hr(
        Some(CoreHistoryReasoning::Strip),
        assistant_with_thinking(None),
    );

    // Act.
    let body = normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

    // Assert: unsigned thinking removed, text + tool_use survive.
    let blocks = assistant_blocks(&body);
    assert_eq!(
        block_types(&blocks),
        vec!["text", "tool_use"],
        "Strip must drop the unsigned thinking block"
    );
}

#[test]
fn auto_and_unset_default_to_strip() {
    // The dispatch default (None) and explicit Auto both resolve to
    // strip for the anthropic-api egress: there is no dialect-default
    // concept here, so Auto means strip (real-Anthropic-safe). Pins
    // that the default path is unchanged by the Preserve gate.
    for hr in [None, Some(CoreHistoryReasoning::Auto)] {
        // Arrange.
        let req = req_with_hr(hr, assistant_with_thinking(None));

        // Act.
        let body =
            normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

        // Assert.
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "tool_use"],
            "Auto/unset ({hr:?}) must strip unsigned thinking"
        );
    }
}

#[test]
fn signed_thinking_passes_through_in_all_modes() {
    // A SIGNED thinking block is never the target of the
    // unsigned-strip, so it survives under both Preserve and Strip.
    // Pins that the gate only ever affects unsigned blocks.
    let sig = claude_signature();
    for hr in [CoreHistoryReasoning::Preserve, CoreHistoryReasoning::Strip] {
        // Arrange.
        let req = req_with_hr(Some(hr), assistant_with_thinking(Some(&sig)));

        // Act.
        let body =
            normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

        // Assert.
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "thinking", "tool_use"],
            "signed thinking must survive under {hr:?}"
        );
        let thinking = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
            .unwrap_or_else(|| panic!("thinking block absent under {hr:?}"));
        assert_eq!(
            thinking["signature"], sig,
            "signed thinking keeps its signature under {hr:?}"
        );
    }
}

#[test]
fn tool_call_id_reject_stays_unconditional_under_preserve() {
    // The tool_result/tool_call_id hard-reject is a separate
    // correctness invariant from the thinking-strip. Preserve must
    // NOT relax it: a Role::Tool message lacking tool_call_id still
    // errors regardless of history_reasoning.
    let mut req = ChatRequest {
        model: "deepseek-chat".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Text("result content".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    };
    req.routectl_internal.history_reasoning = Some(CoreHistoryReasoning::Preserve);

    let err = normalize("deepseek", &req, false, &[], false, None).unwrap_err();
    assert!(
        err.to_string().contains("tool_call_id"),
        "must reject missing tool_call_id even under Preserve; got: {err}"
    );
}

/// `routectl_internal` field path consulted: when `supports_adaptive_thinking`
/// is read from `req.routectl_internal` and is `true`, the adaptive wire
/// shape is emitted. This pins that normalize reads the canonical internal
/// carrier rather than a hardcoded literal passed by the caller.
#[test]
fn normalize_reads_supports_adaptive_thinking_from_routectl_internal() {
    use routectl_core::ReasoningConfig;

    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hello")].into(),
        max_tokens: Some(8192),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    // Set the flag via the routectl_internal carrier (not a parameter).
    req.routectl_internal.supports_adaptive_thinking = true;

    let body = normalize(
        "test",
        &req,
        req.routectl_internal.supports_adaptive_thinking,
        &[],
        false,
        None,
    )
    .expect("normalize must succeed");

    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(
        thinking["type"], "adaptive",
        "routectl_internal.supports_adaptive_thinking=true must yield adaptive shape"
    );
    assert!(
        thinking.get("budget_tokens").is_none(),
        "adaptive shape must not carry budget_tokens"
    );
}

/// The `supports_adaptive_thinking` flag drives the Anthropic egress
/// thinking wire-shape. This flag has NO code-level per-model source: it
/// is set by the operator in config. A mis-set flag produces a
/// fallback-inducing 400 on the upstream. This test pins the
/// wire-shape-follows-flag invariant (not a per-model capability mapping):
/// the flag value alone determines adaptive vs. legacy shape.
///
/// Invariant:
///   supports_adaptive_thinking=true  -> adaptive wire shape
///   supports_adaptive_thinking=false -> legacy wire shape
#[test]
fn per_model_adaptive_thinking_wire_shape_contract() {
    use routectl_core::ReasoningConfig;

    struct Row {
        model: &'static str,
        adaptive: bool,
        expect_adaptive_shape: bool,
    }

    let rows = [
        Row {
            model: "claude-opus-4-8",
            adaptive: true,
            expect_adaptive_shape: true,
        },
        Row {
            model: "claude-opus-4-7",
            adaptive: true,
            expect_adaptive_shape: true,
        },
        Row {
            model: "claude-haiku-4-5",
            adaptive: false,
            expect_adaptive_shape: false,
        },
    ];

    for row in &rows {
        let mut req = ChatRequest {
            model: row.model.into(),
            messages: vec![user_msg("hi")].into(),
            max_tokens: Some(8192),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        req.routectl_internal.supports_adaptive_thinking = row.adaptive;

        let body = normalize(
            "test",
            &req,
            req.routectl_internal.supports_adaptive_thinking,
            &[],
            false,
            None,
        )
        .unwrap_or_else(|e| panic!("normalize failed for {}: {e}", row.model));

        let thinking = body
            .get("thinking")
            .unwrap_or_else(|| panic!("thinking field absent for {}", row.model));

        if row.expect_adaptive_shape {
            assert_eq!(
                thinking["type"], "adaptive",
                "model {} with adaptive=true must emit adaptive shape",
                row.model
            );
            assert!(
                thinking.get("budget_tokens").is_none(),
                "adaptive shape must not carry budget_tokens ({})",
                row.model
            );
        } else {
            assert_ne!(
                thinking["type"], "adaptive",
                "model {} with adaptive=false must NOT emit adaptive shape",
                row.model
            );
            assert!(
                thinking.get("budget_tokens").is_some() || thinking["type"] == "enabled",
                "non-adaptive shape must be legacy enabled for {} (got: {})",
                row.model,
                thinking
            );
        }
    }
}

/// Operator cap applied: max_thinking_budget=2000 with max_tokens=10000
/// clamps the budget DOWN to 2000 before Anthropic's window clamp runs.
#[test]
fn max_thinking_budget_nonzero_clamps_budget_down() {
    use routectl_core::ReasoningConfig;

    let mut req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hello")].into(),
        max_tokens: Some(10000),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(8000),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    // Operator cap of 2000 < caller's explicit 8000.
    req.routectl_internal.max_thinking_budget = 2000;

    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "enabled");
    assert_eq!(
        thinking["budget_tokens"], 2000,
        "max_thinking_budget=2000 must cap the explicit budget of 8000 down to 2000"
    );
}

/// No operator cap: max_thinking_budget=0 passes the budget through
/// unchanged (only Anthropic's window clamp applies).
#[test]
fn max_thinking_budget_zero_no_op() {
    use routectl_core::ReasoningConfig;

    let mut req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hello")].into(),
        max_tokens: Some(10000),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(3000),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    // Zero = no operator cap.
    req.routectl_internal.max_thinking_budget = 0;

    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "enabled");
    // budget=3000 fits in [1024, 9999] unchanged.
    assert_eq!(
        thinking["budget_tokens"], 3000,
        "max_thinking_budget=0 must not alter the budget; got {thinking:?}"
    );
}

// -----------------------------------------------------------------
// emit_reasoning_blocks: non-anthropic format WARN (Finding 2)
// -----------------------------------------------------------------

/// When `emit_reasoning_blocks` encounters reasoning details whose
/// `format` is not `anthropic-claude-v1` it must drop them (behavior-
/// preserving) AND emit a structured WARN that aggregates the skipped
/// count and the distinct format strings so operators can diagnose why
/// blocks are absent from the replay.
#[test]
fn emit_reasoning_blocks_warns_on_non_anthropic_format() {
    // Arrange: assistant message with two reasoning details that carry
    // non-anthropic formats (one foreign string, one absent / None).
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![
            user_msg("think then reply"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("I thought about it.".into()),
                reasoning: None,
                reasoning_details: vec![
                    ReasoningDetail {
                        kind: ReasoningDetailKind::Text,
                        id: None,
                        format: Some("openai-o-format".to_string()),
                        index: Some(0),
                        payload: json!({"text": "some reasoning", "signature": "sig"}),
                    },
                    ReasoningDetail {
                        kind: ReasoningDetailKind::Encrypted,
                        id: None,
                        // format = None -> not anthropic-claude-v1 -> must also be skipped
                        format: None,
                        index: Some(1),
                        payload: json!({"data": "encrypted-blob"}),
                    },
                ],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    // Act: normalize under capture to observe the emitted WARN.
    let mut body_out: Option<Value> = None;
    let captured = routectl_testkit::capture_events(|| {
        body_out = Some(
            normalize("prov-test", &req, false, &[], false, None).expect("normalize must succeed"),
        );
    });
    let _body = body_out.expect("normalize ran");

    // Assert: the skipped-format WARN must be emitted.
    let warn_event = captured
        .iter()
        .find(|e| {
            e.message
                .contains("skipping reasoning blocks on replay: format is not anthropic-claude-v1")
        })
        .unwrap_or_else(|| panic!("expected non-anthropic-format WARN; got events: {captured:?}"));
    assert_eq!(warn_event.level, tracing::Level::WARN);

    // provider field must identify the caller.
    let provider_val = warn_event
        .fields
        .iter()
        .find(|(k, _)| k == "provider")
        .map(|(_, v)| v.as_str())
        .expect("provider field present");
    assert_eq!(provider_val, "prov-test");

    // skipped_count: both details were dropped.
    let count_val = warn_event
        .fields
        .iter()
        .find(|(k, _)| k == "skipped_count")
        .map(|(_, v)| v.as_str())
        .expect("skipped_count field present");
    assert_eq!(count_val, "2", "both non-anthropic details must be counted");

    // skipped_formats: must contain the foreign format string and the
    // placeholder for the absent format.
    let formats_val = warn_event
        .fields
        .iter()
        .find(|(k, _)| k == "skipped_formats")
        .map(|(_, v)| v.as_str())
        .expect("skipped_formats field present");
    assert!(
        formats_val.contains("openai-o-format"),
        "skipped_formats must include the foreign format string; got: {formats_val:?}",
    );
    assert!(
        formats_val.contains("<none>"),
        "skipped_formats must include <none> for format=None details; got: {formats_val:?}",
    );
}

/// Claude 4.x rejects a body carrying both sampling knobs. When the
/// caller sends both, temperature wins and top_p is dropped.
#[test]
fn drops_top_p_when_temperature_also_set() {
    // Arrange
    let req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(256),
        temperature: Some(0.7),
        top_p: Some(0.9),
        ..Default::default()
    };

    // Act
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    // Assert
    assert_eq!(body["temperature"], 0.7);
    assert!(
        body.get("top_p").is_none(),
        "top_p must be dropped when temperature is set, got {body:?}"
    );
}

/// With only top_p set the body carries top_p and no temperature.
#[test]
fn keeps_top_p_when_temperature_unset() {
    // Arrange
    let req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(256),
        temperature: None,
        top_p: Some(0.9),
        ..Default::default()
    };

    // Act
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    // Assert
    assert_eq!(body["top_p"], 0.9);
    assert!(
        body.get("temperature").is_none(),
        "temperature must be absent when only top_p is set, got {body:?}"
    );
}

/// With only temperature set the body carries temperature and no top_p.
#[test]
fn keeps_temperature_when_top_p_unset() {
    // Arrange
    let req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(256),
        temperature: Some(0.3),
        top_p: None,
        ..Default::default()
    };

    // Act
    let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

    // Assert
    assert_eq!(body["temperature"], 0.3);
    assert!(
        body.get("top_p").is_none(),
        "top_p must be absent when only temperature is set, got {body:?}"
    );
}

/// Thinking forces temperature to 1.0; top_p must then be dropped too,
/// since Anthropic also rejects top_p while thinking is active.
#[test]
fn drops_top_p_when_thinking_forces_temperature() {
    use routectl_core::ReasoningConfig;
    // Arrange
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        top_p: Some(0.9),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };

    // Act
    let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();

    // Assert
    assert_eq!(body["temperature"], 1.0);
    assert!(
        body.get("top_p").is_none(),
        "top_p must be dropped while thinking is active, got {body:?}"
    );
}

// -----------------------------------------------------------------
// Late enforcer of the output_config.effort invariant.
//
// After the reorder, `reconcile_output_config_effort` runs LAST in
// normalize and reads ground truth from the assembled body, not the
// stale `adaptive` arg: output_config.effort is present IFF the body
// carries thinking with type == adaptive.
// -----------------------------------------------------------------

/// Adaptive model whose `provider_extras` carries an
/// `output_config: {format: ...}` with NO `effort` sub-key. The
/// assembled body has `thinking.type == adaptive`, so the late
/// enforcer must re-inject `output_config.effort` from
/// `derive_effort` (clamped) while preserving the sibling `format`.
#[test]
fn adaptive_reinjects_effort_when_provider_extras_omits_it() {
    use routectl_core::ReasoningConfig;
    use serde_json::json;
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {
                "format": {"type": "json_schema", "schema": {"type": "object"}}
            }
        })),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ]);

    let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

    assert_eq!(body["thinking"]["type"], "adaptive");
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "high",
        "effort must be re-injected from derive_effort when provider_extras omits it; got: {oc}"
    );
    assert_eq!(
        oc["format"]["type"], "json_schema",
        "sibling format must be preserved; got: {oc}"
    );
}

/// (tool_choice latent) Adaptive thinking + a forcing `tool_choice`
/// (`type:"tool"`). The late enforcer must observe that thinking was
/// stripped from the body and drop the now-orphan
/// `output_config.effort` -- even though the enforcer no longer runs
/// any effort removal inside `strip_thinking_when_tool_choice_forces_use`.
#[test]
fn adaptive_forced_tool_choice_drops_orphan_effort_via_late_enforcer() {
    use routectl_core::ReasoningConfig;
    use serde_json::json;
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        tool_choice: Some(json!({"type": "tool", "name": "web_search"})),
        ..Default::default()
    };

    let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped when tool_choice forces tool use, got: {body}"
    );
    assert!(
        body.get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_none(),
        "orphan output_config.effort must be dropped by the late enforcer, got: {body}"
    );
}

/// (positive regression-pin) A normal adaptive request whose effort
/// exceeds the operator cap: the late enforcer guarantees presence
/// AND clamps to the cap.
#[test]
fn adaptive_effort_over_cap_clamped_by_late_enforcer() {
    use routectl_core::ReasoningConfig;
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ]);

    let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

    assert_eq!(body["thinking"]["type"], "adaptive");
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "high",
        "effort must be present and clamped to the operator cap; got: {oc}"
    );
}
