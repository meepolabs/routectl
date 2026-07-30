//! Additional review-finding tests for `super::request::translate`.
//!
//! Lives in a sibling file so `request_tests.rs` stays under the
//! project's 800-line ceiling. Imported via `#[path =
//! "request_tests_round2.rs"] mod tests_round2;` from `request.rs`.
//!
//! Coverage:
//!   - `{"type":"none"}` Anthropic-object tool_choice suppresses
//!     the entire toolConfig (not just toolChoice).
//!   - `req.provider_extras` merges into
//!     additionalModelRequestFields, with managed-key shielding.
//!   - A canonical Document content block prepends an empty
//!     {text} sibling when no Text exists in the same message.
//!   - Role::Tool Parts of type Image / Document dispatch
//!     through the typed translator (no opaque Json wrap).
//!   - HIGH 6: anthropic_beta filter applies on the Converse path
//!     identically to Invoke (allowlist + per-provider floor +
//!     global override hooks).

use super::super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use routectl_core::{
    ChatRequest, ContentPart, CustomTool, KnownContentPart, Message, MessageContent, Role, ToolDef,
};
use serde_json::json;

fn fake_cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: vec![
            "context-1m-2025-08-07".into(),
            "claude-code-20250219".into(),
            "interleaved-thinking-2025-05-14".into(),
            "context-management-2025-06-27".into(),
            "effort-2025-11-24".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
            "computer-use-2025-01-24".into(),
            "computer-use-2024-10-22".into(),
            "mcp-client-2025-04-04".into(),
            "search-results-2025-06-09".into(),
        ],
        allowed_body_fields: vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
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

// ---------------------------------------------------------------------------
// HIGH 1: Anthropic-object {"type":"none"} tool_choice
// ---------------------------------------------------------------------------

#[test]
fn anthropic_object_none_tool_choice_suppresses_tool_config_entirely() {
    // Arrange: the Anthropic-object form {"type":"none"} must
    // suppress the entire toolConfig, identical to the bare-string
    // "none" suppression. Converse defaults toolChoice to `auto`
    // when tools is set but toolChoice isn't -- so emitting
    // tools-without-toolChoice would let the model call tools the
    // caller forbade.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "calc".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        tool_choice: Some(json!({"type": "none"})),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    assert!(
        body.get("toolConfig").is_none(),
        "expected toolConfig entirely suppressed under {{type:\"none\"}}, got: {body}"
    );
}

// ---------------------------------------------------------------------------
// HIGH 2: provider_extras merge into additionalModelRequestFields
// ---------------------------------------------------------------------------

#[test]
fn provider_extras_merge_into_additional_model_request_fields() {
    // Arrange: a custom forward-compat field (the Anthropic ingress
    // sweeps unknown top-level keys into provider_extras) must
    // survive to additionalModelRequestFields verbatim PROVIDED the
    // operator has the field on `[bedrock] allowed_body_fields`.
    // Without this merge, fields like `context_management` and
    // `output_config.format` disappear silently between ingress and
    // Converse egress. Fields NOT on the operator list (e.g.
    // `mcp_servers`, `container`) are dropped; see
    // `body_fields_filter_drops_disallowed_keys_on_converse` for that
    // contract. The client `metadata` fingerprint is stripped
    // unconditionally on this seam (see
    // `client_metadata_fingerprint_skipped_from_converse_bag`).
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        provider_extras: Some(json!({
            "context_management": {"strategy": "summarize"},
            "metadata": {"user_id": "u-1"},
            "top_k": 40,
        })),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag with provider_extras");
    assert_eq!(
        bag["context_management"]["strategy"], "summarize",
        "got {body}"
    );
    assert!(
        !bag.contains_key("metadata"),
        "client metadata fingerprint must be stripped on the Converse seam: {body}"
    );
    assert_eq!(bag["top_k"], 40, "got {body}");
}

#[test]
fn body_fields_filter_drops_disallowed_keys_on_converse() {
    // Arrange: the Anthropic ingress's forward-compat sweep
    // forwards unknown top-level keys (e.g. `mcp_servers`,
    // `container`, `diagnostics`) into provider_extras. The Converse
    // egress must DROP any key not on `[bedrock] allowed_body_fields`
    // before sending; AWS forwards the bag verbatim to Anthropic
    // which 400s the request on the first unrecognized field.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        provider_extras: Some(json!({
            "context_management": {"strategy": "summarize"},  // allowed
            "mcp_servers": [{"url": "https://example.com"}],  // disallowed
            "container": "my-container",                       // disallowed
            "diagnostics": {"trace_id": "abc"},                // disallowed
        })),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();
    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag");

    // Assert: only the allowed key survives.
    assert!(bag.contains_key("context_management"), "got {body}");
    assert!(
        !bag.contains_key("mcp_servers"),
        "mcp_servers leaked through: {body}"
    );
    assert!(
        !bag.contains_key("container"),
        "container leaked through: {body}"
    );
    assert!(
        !bag.contains_key("diagnostics"),
        "diagnostics leaked through: {body}"
    );
}

#[test]
fn provider_extras_cannot_override_managed_keys_on_converse() {
    // Arrange: an attempt to inject managed-key overrides via
    // provider_extras (e.g. a malicious or careless caller setting
    // `provider_extras = {"thinking": ...}`) must drop the keys
    // with a WARN, mirroring is_converse_managed_key.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        provider_extras: Some(json!({
            "thinking": {"type": "evil"},
            "anthropic_beta": ["pwn"],
            // long-tail key MUST pass through:
            "top_k": 40,
        })),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body
        .get("additionalModelRequestFields")
        .and_then(|v| v.as_object());
    if let Some(b) = bag {
        assert!(
            b.get("thinking").is_none_or(|v| v["type"] != "evil"),
            "thinking override leaked: {body}"
        );
        // Long-tail extras DO land.
        assert_eq!(b["top_k"], 40, "got {body}");
    }
}

// ---------------------------------------------------------------------------
// HIGH 3: Document content block prepends sibling Text
// ---------------------------------------------------------------------------

#[test]
fn document_with_existing_text_sibling_does_not_prepend_empty_text() {
    // Arrange: when the user turn already has a sibling text block,
    // ensure_document_has_text_sibling is a no-op -- no extra empty
    // Text is prepended.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "see the attached report".into(),
                    citations: None,
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::Document {
                    source: json!({
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "JVBERi0xLjQK",
                    }),
                    title: None,
                    citations: None,
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "got {body}");
    assert_eq!(blocks[0]["text"], "see the attached report", "got {body}");
    assert!(blocks[1].get("document").is_some(), "got {body}");
}

// ---------------------------------------------------------------------------
// HIGH 4: Role::Tool Image / Document parts dispatch through typed translator
// ---------------------------------------------------------------------------

#[test]
fn role_tool_with_image_parts_uses_image_variant_not_json_wrap() {
    // Arrange: canonical Role::Tool with a Parts content array
    // carrying an Image part. The naive Json wrap would surface
    // the canonical schema upstream and Claude 3+ on Converse
    // rejects the malformed shape. Image parts must dispatch
    // through the typed translator so AWS sees the {image:{format,
    // source:{bytes}}} shape on the toolResult content array.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("look at this"),
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "see attached".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Image {
                        source: json!({
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "AAAA",
                        }),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("toolu_X".into()),
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let messages = body["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b.get("toolResult").is_some()))
        })
        .expect("expected synthesized tool_result message");
    let arr = tool_msg["content"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|b| b.get("toolResult"))
        .expect("toolResult block")["content"]
        .as_array()
        .unwrap();
    assert_eq!(arr.len(), 2, "got {body}");
    assert_eq!(arr[0]["text"], "see attached", "got {body}");
    let img = &arr[1]["image"];
    assert_eq!(img["format"], "png", "got {body}");
    assert_eq!(img["source"]["bytes"], "AAAA", "got {body}");
}

#[test]
fn role_tool_with_document_parts_uses_document_variant_not_json_wrap() {
    // Arrange: canonical Role::Tool with a Parts content array
    // carrying a Document part. AWS expects the document variant
    // {document: {format, name, source: {bytes}}}, NOT a Json wrap
    // of the canonical Document part.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("review the report"),
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Document {
                        source: json!({
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "JVBERi0xLjQK",
                        }),
                        title: Some("report.pdf".into()),
                        citations: None,
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("toolu_doc".into()),
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let messages = body["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b.get("toolResult").is_some()))
        })
        .expect("expected synthesized tool_result message");
    let arr = tool_msg["content"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|b| b.get("toolResult"))
        .expect("toolResult block")["content"]
        .as_array()
        .unwrap();
    assert_eq!(arr.len(), 1, "got {body}");
    let doc = &arr[0]["document"];
    assert_eq!(doc["format"], "pdf", "got {body}");
    assert_eq!(doc["name"], "report_pdf", "got {body}");
    assert_eq!(doc["source"]["bytes"], "JVBERi0xLjQK", "got {body}");
}

// ---------------------------------------------------------------------------
// HIGH 6: anthropic_beta filter on Converse (matches Invoke)
// ---------------------------------------------------------------------------

#[test]
fn anthropic_beta_filtered_against_bedrock_allowlist_in_additional_fields() {
    // Arrange: a request whose canonical anthropic_beta carries
    // both an officially-accepted Bedrock flag and one routectl's
    // shared filter would drop. Converse re-applies the same
    // allowlist as Invoke -- AWS validates anthropic_beta whether
    // it sits on the body (Invoke) or in
    // additionalModelRequestFields (Converse), so the filter
    // applies on both paths.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        anthropic_beta: vec![
            "context-1m-2025-08-07".into(),           // accepted
            "made-up-flag".into(),                    // not in allowlist
            "interleaved-thinking-2025-05-14".into(), // accepted
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag");
    let betas = bag["anthropic_beta"].as_array().expect("expected betas");
    let strs: Vec<&str> = betas.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        strs.contains(&"context-1m-2025-08-07"),
        "accepted flag missing: {strs:?}"
    );
    assert!(
        strs.contains(&"interleaved-thinking-2025-05-14"),
        "accepted flag missing: {strs:?}"
    );
    assert!(
        !strs.contains(&"made-up-flag"),
        "unsupported flag leaked through Converse filter: {strs:?}"
    );
}

#[test]
fn anthropic_beta_provider_config_floor_bypasses_filter_on_converse() {
    // Arrange: the per-provider floor (`[providers.X] anthropic_beta`)
    // applies to Converse identically to Invoke. Operator-asserted
    // flags pass through unconditionally regardless of the routectl
    // allowlist, because the operator typed them into TOML.
    let mut cfg = fake_cfg();
    cfg.anthropic_beta = vec!["future-flag-2099".into()];
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag");
    let strs: Vec<&str> = bag["anthropic_beta"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        strs.contains(&"future-flag-2099"),
        "operator-asserted flag was filtered out on Converse: {strs:?}"
    );
}

#[test]
fn anthropic_beta_global_allowed_betas_filters_against_operator_list_on_converse() {
    // Arrange: `cfg.allowed_betas` (sourced from
    // `[bedrock] allowed_betas` global TOML) is the FULL operator-
    // supplied allowlist -- routectl ships no const default. Same
    // hook, same precedence as Invoke.
    let mut cfg = fake_cfg();
    cfg.allowed_betas = vec!["my-override".into()];
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")].into(),
        anthropic_beta: vec![
            // NOT in operator list: drops.
            "context-1m-2025-08-07".into(),
            // In operator list: survives.
            "my-override".into(),
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag");
    let strs: Vec<&str> = bag["anthropic_beta"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        strs,
        vec!["my-override"],
        "global allowlist override did not replace const: {strs:?}"
    );
}

// ---------------------------------------------------------------------------
// Q4: thinking + redacted_thinking translate to AWS reasoningContent
// ---------------------------------------------------------------------------

#[test]
fn thinking_block_with_signature_translates_to_converse_reasoning_text() {
    // Arrange: a multi-turn assistant replay carrying a Thinking
    // content block. AWS Converse expects the prior reasoning to
    // ride as `{reasoningContent: {reasoningText: {text, signature}}}`.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("question"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "let me think".into(),
                        signature: Some("sig_abc".into()),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "answer".into(),
                        citations: None,
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

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: the assistant message's first content block is a
    // reasoningContent with a reasoningText carrying the verbatim
    // signature.
    let messages = body["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("missing assistant message");
    let content = assistant["content"].as_array().unwrap();
    assert_eq!(
        content[0]["reasoningContent"]["reasoningText"]["text"],
        "let me think"
    );
    assert_eq!(
        content[0]["reasoningContent"]["reasoningText"]["signature"],
        "sig_abc"
    );
    // And the trailing text block survives intact.
    assert_eq!(content[1]["text"], "answer");
}

#[test]
fn thinking_block_without_signature_returns_err() {
    // Arrange: missing signature must surface as a NormalizeRequest
    // error locally rather than producing a body AWS will 400 with a
    // confusing "invalid reasoning content" error on the second turn.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("q"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Thinking {
                        thinking: "no sig here".into(),
                        signature: None,
                    },
                )]),
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

    // Act
    let result = normalize_request(&cfg, &req);

    // Assert
    let err = result.expect_err("expected error on missing signature");
    let msg = err.to_string();
    assert!(
        msg.contains("missing signature") || msg.contains("cannot replay"),
        "expected normalize_request error about missing signature, got: {msg}"
    );
}

#[test]
fn thinking_block_with_empty_signature_returns_err() {
    // Arrange: an empty-string signature is logically identical to
    // a missing signature -- AWS will reject it with a confusing
    // validation error if we let it through. Surface the same
    // NormalizeRequest error as the None case so the operator sees
    // a clear local message instead of a vague AWS 400.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("q"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Thinking {
                        thinking: "empty sig here".into(),
                        signature: Some(String::new()),
                    },
                )]),
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

    // Act
    let result = normalize_request(&cfg, &req);

    // Assert
    let err = result.expect_err("expected error on empty signature");
    let msg = err.to_string();
    assert!(
        msg.contains("missing signature") || msg.contains("cannot replay"),
        "expected normalize_request error about missing signature, got: {msg}"
    );
}

#[test]
fn redacted_thinking_translates_to_converse_redacted_content() {
    // Arrange: redacted thinking carries safety-redacted reasoning
    // bytes in canonical schema. They round-trip into AWS as
    // `{reasoningContent: {redactedContent: <base64>}}` -- pass-through
    // verbatim, no signature.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("q"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::RedactedThinking {
                        data: "AAECAwQF".into(),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "ok".into(),
                        citations: None,
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

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    let content = assistant["content"].as_array().unwrap();
    assert_eq!(
        content[0]["reasoningContent"]["redactedContent"],
        "AAECAwQF"
    );
    // No reasoningText sibling on the redacted variant.
    assert!(
        content[0]["reasoningContent"]
            .get("reasoningText")
            .is_none()
    );
}

#[test]
fn multi_turn_assistant_replay_with_thinking_round_trips_through_converse() {
    // Arrange: the canonical multi-turn shape that triggers AWS 400s
    // pre-fix -- assistant message carrying [Thinking, Text, ToolUse]
    // followed by a user-role tool_result, then a user follow-up.
    // With the fix, the assistant's reasoningContent block rides
    // verbatim and AWS accepts the second-turn replay.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("what is the weather in Tokyo?"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Thinking {
                        thinking: "user wants weather; I'll call the tool".into(),
                        signature: Some("sig_round1".into()),
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "Let me check.".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "tu_1".into(),
                        name: "get_weather".into(),
                        input: json!({"location": "Tokyo"}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("sunny, 22C".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("tu_1".into()),
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    };

    // Act
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: assistant message carries reasoningContent + text +
    // toolUse in order, and the tool message lands as a synthesized
    // user-role toolResult.
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    let assistant_content = messages[1]["content"].as_array().unwrap();
    assert_eq!(
        assistant_content[0]["reasoningContent"]["reasoningText"]["text"],
        "user wants weather; I'll call the tool"
    );
    assert_eq!(
        assistant_content[0]["reasoningContent"]["reasoningText"]["signature"],
        "sig_round1"
    );
    // strip_text_after_tool_use removes trailing text after a tool_use
    // but the leading text before the tool_use survives.
    assert_eq!(assistant_content[1]["text"], "Let me check.");
    assert_eq!(assistant_content[2]["toolUse"]["toolUseId"], "tu_1");
    // Tool message becomes a synthesized user-role toolResult.
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["toolResult"]["toolUseId"], "tu_1");
}

// ---------------------------------------------------------------------------
// true round-trip: response -> canonical -> request
// ---------------------------------------------------------------------------

#[test]
fn response_to_request_round_trip_preserves_thinking_signature_text_and_tool_use() {
    // Arrange: a synthetic AWS Converse response body that contains a
    // reasoningContent block (with a real signature value), a text block,
    // and a toolUse block. This mirrors the actual shape returned by
    // Claude on Bedrock Converse when interleaved-thinking is on.
    let cfg = fake_cfg();
    let raw_response = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {
                        "reasoningText": {
                            "text": "I should call get_weather to answer",
                            "signature": "rt_sig_roundtrip_abc123"
                        }
                    }},
                    {"text": "Let me check the weather."},
                    {"toolUse": {
                        "toolUseId": "tu_rt_1",
                        "name": "get_weather",
                        "input": {"location": "Osaka"}
                    }}
                ]
            }
        },
        "stopReason": "tool_use"
    });

    // Act (step 1): decode the response into a canonical ChatResponse.
    let chat_response =
        super::super::normalize_response("bedrock:test-converse", raw_response).unwrap();

    // Verify the response decoded correctly before the round-trip.
    let resp_msg = &chat_response.choices[0].message;
    let rd = &resp_msg.reasoning_details;
    assert_eq!(rd.len(), 1, "expected one reasoning_detail from response");
    let sig_from_resp = rd[0].payload["signature"].as_str().unwrap();
    assert_eq!(sig_from_resp, "rt_sig_roundtrip_abc123");

    // Step 2: reconstruct a canonical ChatRequest by promoting the
    // response content + reasoning_details back into a new assistant
    // Message (this mirrors what a multi-turn orchestrator does when
    // replaying the prior turn). The reasoning block must use the
    // signature extracted from reasoning_details.
    let assistant_parts = match &resp_msg.content {
        MessageContent::Parts(p) => {
            // Prepend the Thinking block reconstructed from reasoning_details.
            let mut parts = vec![ContentPart::Known(KnownContentPart::Thinking {
                thinking: rd[0].payload["text"].as_str().unwrap_or("").to_string(),
                signature: Some(sig_from_resp.to_string()),
            })];
            parts.extend_from_slice(p);
            parts
        }
        MessageContent::Text(t) => {
            // Pure-text response (shouldn't happen here, but be safe).
            vec![
                ContentPart::Known(KnownContentPart::Thinking {
                    thinking: rd[0].payload["text"].as_str().unwrap_or("").to_string(),
                    signature: Some(sig_from_resp.to_string()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: t.clone(),
                    citations: None,
                    cache_control: None,
                }),
            ]
        }
        MessageContent::Null => vec![],
    };

    let tool_result_msg = Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text("rainy, 18C".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: Some("tu_rt_1".into()),
        tool_calls: None,
    };

    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("what is the weather in Osaka?"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(assistant_parts),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            tool_result_msg,
        ]
        .into(),
        ..Default::default()
    };

    // Act (step 3): translate the reconstructed request to a Converse body.
    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: assistant message carries:
    //   [0] reasoningContent.reasoningText with the ORIGINAL signature
    //   [1] text block
    //   [2] toolUse block with original toolUseId and name
    let msgs = body["messages"].as_array().unwrap();
    let assistant = msgs
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("expected assistant message in Converse body");
    let content = assistant["content"].as_array().unwrap();

    assert_eq!(
        content[0]["reasoningContent"]["reasoningText"]["signature"], "rt_sig_roundtrip_abc123",
        "signature must survive response -> canonical -> request round-trip"
    );
    assert_eq!(
        content[0]["reasoningContent"]["reasoningText"]["text"],
        "I should call get_weather to answer",
        "reasoning text must survive round-trip"
    );
    // The text block precedes the toolUse (strip_text_after_tool_use only
    // removes text AFTER the last toolUse, not before it).
    assert_eq!(
        content[1]["text"], "Let me check the weather.",
        "text block must survive round-trip"
    );
    assert_eq!(
        content[2]["toolUse"]["toolUseId"], "tu_rt_1",
        "toolUseId must survive round-trip"
    );
    assert_eq!(
        content[2]["toolUse"]["name"], "get_weather",
        "tool name must survive round-trip"
    );
}
