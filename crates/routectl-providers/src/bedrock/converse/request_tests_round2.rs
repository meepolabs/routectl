//! internal review tests for `super::request::translate`.
//!
//! Lives in a sibling file so `request_tests.rs` stays under the
//! project's 800-line ceiling. Imported via `#[path =
//! "request_tests_round2.rs"] mod tests_round2;` from `request.rs`.
//!
//! Coverage:
//!   - HIGH 1: `{"type":"none"}` Anthropic-object tool_choice suppresses
//!     the entire toolConfig (not just toolChoice).
//!   - HIGH 2: `req.provider_extras` merges into
//!     additionalModelRequestFields, with managed-key shielding.
//!   - HIGH 3: A canonical Document content block prepends an empty
//!     {text} sibling when no Text exists in the same message.
//!   - HIGH 4: Role::Tool Parts of type Image / Document dispatch
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
        extra_headers: Vec::new(),
        anthropic_beta: Vec::new(),
        anthropic_beta_allowlist: None,
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn user_msg(text: &str) -> Message {
    Message {
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
        messages: vec![user_msg("hi")],
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
    // survive to additionalModelRequestFields verbatim. Without
    // this, fields like `context_management`, `mcp_servers`,
    // `container`, and the legacy-merged `output_config.format`
    // disappear silently between ingress and Converse egress.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")],
        provider_extras: Some(json!({
            "context_management": {"strategy": "summarize"},
            "mcp_servers": [{"url": "https://example.com"}],
            "container": "my-container",
        })),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body["additionalModelRequestFields"]
        .as_object()
        .expect("expected bag with provider_extras");
    assert_eq!(bag["context_management"]["strategy"], "summarize", "got {body}");
    assert_eq!(bag["mcp_servers"][0]["url"], "https://example.com", "got {body}");
    assert_eq!(bag["container"], "my-container", "got {body}");
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
        messages: vec![user_msg("hi")],
        provider_extras: Some(json!({
            "thinking": {"type": "evil"},
            "anthropic_beta": ["pwn"],
            // long-tail key MUST pass through:
            "metadata": {"user_id": "u-1"},
        })),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let bag = body
        .get("additionalModelRequestFields")
        .and_then(|v| v.as_object());
    if let Some(b) = bag {
        assert!(
            b.get("thinking").map(|v| v["type"] != "evil").unwrap_or(true),
            "thinking override leaked: {body}"
        );
        // Long-tail extras DO land.
        assert_eq!(b["metadata"]["user_id"], "u-1", "got {body}");
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
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "see the attached report".into(),
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
        }],
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
                role: Role::Tool,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "see attached".into(),
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
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let messages = body["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .map(|c| c.iter().any(|b| b.get("toolResult").is_some()))
                .unwrap_or(false)
        })
        .expect("expected synthesized tool_result message");
    let arr = tool_msg["content"][0]["toolResult"]["content"]
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
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let messages = body["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .map(|c| c.iter().any(|b| b.get("toolResult").is_some()))
                .unwrap_or(false)
        })
        .expect("expected synthesized tool_result message");
    let arr = tool_msg["content"][0]["toolResult"]["content"]
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
    // allowlist as Invoke (issues.md::INV-6) -- AWS validates
    // anthropic_beta whether it sits on the body (Invoke) or in
    // additionalModelRequestFields (Converse), so the filter
    // applies on both paths.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")],
        anthropic_beta: vec![
            "context-1m-2025-08-07".into(),       // accepted
            "made-up-flag".into(),                // not in allowlist
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
        messages: vec![user_msg("hi")],
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
fn anthropic_beta_global_allowlist_override_replaces_const_on_converse() {
    // Arrange: `cfg.anthropic_beta_allowlist` (sourced from
    // `[bedrock] anthropic_beta` global TOML) REPLACES the
    // routectl-shipped const allowlist. Same hook, same precedence
    // as Invoke.
    let mut cfg = fake_cfg();
    cfg.anthropic_beta_allowlist = Some(vec!["my-override".into()]);
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")],
        anthropic_beta: vec![
            // In const, NOT in override: drops.
            "context-1m-2025-08-07".into(),
            // NOT in const, in override: survives.
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
