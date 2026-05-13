//! Tests for `super::request::translate` and the orchestrator-level
//! contracts: cross-module wiring (system + messages + tools +
//! extras), Role::Tool error propagation, breakpoint validation, and
//! end-to-end shape assertions.
//!
//! Lives in a sibling file so `request.rs` stays under the project's
//! 800-line ceiling. Imported via `#[path = "request_tests.rs"]
//! mod tests;` from `request.rs`.

use super::super::normalize_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use routectl_core::cache_control::CacheControl;
use routectl_core::system_content::SystemBlock;
use routectl_core::{
    ChatRequest, ContentPart, CustomTool, KnownContentPart, Message, MessageContent,
    ReasoningConfig, Role, SystemContent, ToolDef,
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

#[test]
fn plain_user_message_round_trips_through_converse_request() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hello world")],
        max_tokens: Some(1024),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    assert_eq!(
        body["messages"][0]["role"], "user",
        "expected user role in messages, got {body}"
    );
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello world");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 1024);
    // No system, no toolConfig, no additionalModelRequestFields.
    assert!(body.get("system").is_none(), "got: {body}");
    assert!(body.get("toolConfig").is_none(), "got: {body}");
    assert!(
        body.get("additionalModelRequestFields").is_none(),
        "got: {body}"
    );
}

#[test]
fn system_string_serializes_as_array_of_text_blocks() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        system: Some(SystemContent::Text("be helpful".into())),
        messages: vec![user_msg("hi")],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let sys = body["system"].as_array().expect("system must be array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0], json!({"text": "be helpful"}));
}

#[test]
fn system_blocks_with_cache_control_emit_cache_point_after_text() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        system: Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "context block".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }])),
        messages: vec![user_msg("hi")],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let sys = body["system"].as_array().expect("system must be array");
    assert_eq!(sys.len(), 2, "expected text + cachePoint, got {body}");
    assert_eq!(sys[0], json!({"text": "context block"}));
    assert_eq!(
        sys[1],
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    );
}

#[test]
fn legacy_system_message_is_lifted_into_top_level_system() {
    // Arrange: a direct caller (no ingress) sends `[{role:"system",
    // ...}, {role:"user", ...}]` with no top-level `req.system`.
    // Without lifting, the Converse egress would silently drop the
    // system prompt because Role::System messages skip the
    // build_messages loop.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            Message {
                role: Role::System,
                content: MessageContent::Text("be helpful".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("hi"),
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let sys = body["system"].as_array().expect("system must be array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0], json!({"text": "be helpful"}));
    // The user message is still present; system isn't duplicated
    // into the messages array.
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn custom_tool_translates_to_converse_tool_def() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("weather?")],
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "get_weather".into(),
            description: Some("look up weather".into()),
            input_schema: json!({"type": "object", "properties": {"loc": {"type": "string"}}}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let tools = body["toolConfig"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let spec = &tools[0]["toolSpec"];
    assert_eq!(spec["name"], "get_weather");
    assert_eq!(spec["description"], "look up weather");
    assert_eq!(spec["inputSchema"]["json"]["type"], "object");
}

#[test]
fn tool_with_cache_control_emits_sibling_cache_point_in_tools_array() {
    // Arrange: AWS toolConfig.tools is a union of {toolSpec} and
    // {cachePoint} entries. A cached tool must produce two adjacent
    // entries -- the spec, then the cache marker.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")],
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "calc".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_1h()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let tools = body["toolConfig"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "expected spec + cachePoint, got {body}");
    assert_eq!(tools[0]["toolSpec"]["name"], "calc");
    assert_eq!(
        tools[1]["cachePoint"],
        json!({"type": "default", "ttl": "1h"})
    );
}

#[test]
fn anthropic_builtin_tool_dropped_under_non_strict() {
    // Arrange: a builtin tool (web_search) arrives via ToolDef::Other
    // -- canonical considers it an Anthropic builtin, no Converse
    // equivalent, drop with warn.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("search?")],
        tools: Some(vec![
            ToolDef::Other(json!({
                "type": "web_search_20250901",
                "name": "web_search",
            })),
            ToolDef::Custom(CustomTool {
                name: "ok_tool".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            }),
        ]),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: only the Custom tool survives.
    let tools = body["toolConfig"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "got: {body}");
    assert_eq!(tools[0]["toolSpec"]["name"], "ok_tool");
}

#[test]
fn tool_choice_string_auto_translates_to_aws_auto_object() {
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
        tool_choice: Some(json!("auto")),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    assert_eq!(body["toolConfig"]["toolChoice"], json!({"auto": {}}));
}

#[test]
fn tool_choice_anthropic_object_translates_to_specific_tool() {
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
        tool_choice: Some(json!({"type":"tool","name":"calc"})),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    assert_eq!(
        body["toolConfig"]["toolChoice"],
        json!({"tool": {"name": "calc"}})
    );
}

#[test]
fn tool_choice_with_empty_name_drops_field() {
    // Arrange: each name extraction site (Anthropic-shape `tool`,
    // OpenAI-shape `function`, Converse-shape `tool` passthrough)
    // must drop the field rather than emit `{tool:{name:""}}` --
    // AWS rejects the empty-name shape with a 400.
    for tc in [
        json!({"type":"tool","name":""}),
        json!({"type":"tool"}),
        json!({"type":"function","function":{"name":""}}),
        json!({"type":"function","function":{}}),
        json!({"tool":{"name":""}}),
        json!({"tool":{}}),
    ] {
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
            tool_choice: Some(tc.clone()),
            ..Default::default()
        };
        let body = normalize_request(&cfg, &req).unwrap();
        assert!(
            body["toolConfig"].get("toolChoice").is_none(),
            "expected toolChoice dropped for {tc:?}, got {body}"
        );
    }
}

#[test]
fn thinking_config_lands_in_additional_model_request_fields() {
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![user_msg("hi")],
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: legacy thinking shape with budget_tokens.
    let bag = body
        .get("additionalModelRequestFields")
        .expect("expected additionalModelRequestFields, got {body}");
    let thinking = bag.get("thinking").expect("expected thinking in bag");
    assert_eq!(thinking["type"], "enabled");
    // budget_tokens = max_tokens (1024) * effort_ratio("high")=0.80 = 819
    assert_eq!(thinking["budget_tokens"], 819);
}

#[test]
fn tool_use_block_in_assistant_content_translates_to_aws_tool_use_block() {
    // Arrange: an assistant turn with a tool_use Parts block, as
    // would arrive on a multi-turn replay.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("calculate 2+2"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "computing".into(),
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "tu_abc".into(),
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
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    let blocks = assistant["content"].as_array().unwrap();
    // First text, then toolUse
    assert_eq!(blocks[0]["text"], "computing");
    assert_eq!(blocks[1]["toolUse"]["toolUseId"], "tu_abc");
    assert_eq!(blocks[1]["toolUse"]["name"], "calc");
    assert_eq!(blocks[1]["toolUse"]["input"], json!({"expr": "2+2"}));
}

#[test]
fn assistant_text_after_tool_use_is_stripped() {
    // Arrange: claude 4 occasionally emits a transition text
    // block after `tool_use`. Bedrock + Anthropic both reject
    // that shape on echo with "tool_use ids were found without
    // tool_result blocks immediately after". Mirror the
    // anthropic_api egress's strip behavior here.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("calc 2+2"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "computing".into(),
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "tu_x".into(),
                        name: "calc".into(),
                        input: json!({"expr": "2+2"}),
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::Text {
                        text: "Sure! On it.".into(),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    // Assert: the trailing transition text MUST be dropped.
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message present");
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(
        blocks.len(),
        2,
        "expected text + toolUse, trailing text dropped; got {body}"
    );
    assert_eq!(blocks[0]["text"], "computing");
    assert!(blocks[1].get("toolUse").is_some(), "got: {body}");
}

#[test]
fn tool_message_with_id_emits_user_role_with_tool_result() {
    // Arrange: canonical Role::Tool message with a tool_call_id
    // produces a synthesized user-role message carrying a
    // toolResult block; the toolUseId on the wire matches the
    // canonical tool_call_id verbatim.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("calc 2+2"),
            Message {
                role: Role::Tool,
                content: MessageContent::Text("4".into()),
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
    // Find the tool-result-bearing message (it lands as role: user).
    let tool_msg = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .map(|c| c.iter().any(|b| b.get("toolResult").is_some()))
                .unwrap_or(false)
        })
        .expect("expected synthesized tool_result message");
    assert_eq!(tool_msg["role"], "user");
    let tool_result = &tool_msg["content"][0]["toolResult"];
    assert_eq!(tool_result["toolUseId"], "toolu_X");
    assert_eq!(tool_result["content"][0]["text"], "4");
}

#[test]
fn tool_message_without_id_returns_err() {
    // Arrange: Role::Tool with `tool_call_id: None` MUST surface
    // as a NormalizeRequest error rather than emit
    // `toolResult.toolUseId == ""` upstream (AWS 400).
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![
            user_msg("calc 2+2"),
            Message {
                role: Role::Tool,
                content: MessageContent::Text("result".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        ..Default::default()
    };

    let err = normalize_request(&cfg, &req).unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("tool_call_id"),
        "error must mention tool_call_id; got: {msg}"
    );
}

#[test]
fn document_content_block_translates_to_aws_document_block() {
    // Arrange: canonical Document part in a user turn produces an
    // AWS `{document: {format, name, source: {bytes}}}` block. AWS
    // also requires a companion {text} block in the same message;
    // when canonical doesn't supply one, the egress prepends an
    // empty-string Text so AWS accepts the shape. Forward-compat
    // over rejection.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
                source: json!({
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "JVBERi0xLjQK",
                }),
                title: Some("report.pdf".into()),
                citations: None,
                cache_control: None,
            })]),
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
    // 2 blocks: synthetic empty text sibling + document.
    assert_eq!(blocks.len(), 2, "got {body}");
    assert_eq!(blocks[0]["text"], "", "got {body}");
    let doc = &blocks[1]["document"];
    assert_eq!(doc["format"], "pdf", "got {body}");
    // AWS document.name validates against [a-zA-Z0-9-()[]_ ]{1,200};
    // dots are sanitized to underscores so `report.pdf` -> `report_pdf`.
    assert_eq!(doc["name"], "report_pdf", "got {body}");
    assert_eq!(doc["source"]["bytes"], "JVBERi0xLjQK", "got {body}");
}

#[test]
fn thinking_block_with_cache_control_does_not_emit_orphan_cache_point() {
    // Arrange: thinking blocks drop on the Converse egress (no
    // reasoningContent surface yet). When a thinking block carries
    // cache_control, the loop must NOT emit a stray {cachePoint}
    // entry -- AWS rejects a cachePoint without a preceding content
    // block.
    //
    // Note: KnownContentPart::Thinking has no cache_control field
    // by design, so this test exercises ContentPart::Other -- a
    // forward-compat block that translate_content_part rejects --
    // with cache_control set. Same orphan-cachePoint risk.
    use serde_json::Map;
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Other {
                    type_tag: "future_block".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                    extras: Map::new(),
                },
                ContentPart::Known(KnownContentPart::Text {
                    text: "anchor".into(),
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

    // Assert: the dropped Other block must not leave behind a
    // cachePoint sibling. Only the surviving Text block is emitted.
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "expected only text, got {body}");
    assert_eq!(blocks[0]["text"], "anchor");
}

#[test]
fn tool_result_image_content_uses_image_variant_not_json_wrap() {
    // Arrange: canonical tool_result with an Anthropic-shape image
    // block in its content array. AWS expects the multimodal
    // variant `{image: {format, source: {bytes}}}`, NOT a {json:
    // ...} wrap (Claude 3+ rejects the latter).
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "tu_img".into(),
                    content: json!([
                        {"type": "text", "text": "see attached"},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "AAAA"
                            }
                        }
                    ]),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        ..Default::default()
    };

    let body = normalize_request(&cfg, &req).unwrap();

    let arr = body["messages"][0]["content"][0]["toolResult"]["content"]
        .as_array()
        .unwrap();
    assert_eq!(arr.len(), 2, "got {body}");
    assert_eq!(arr[0]["text"], "see attached", "got {body}");
    let img = &arr[1]["image"];
    assert_eq!(img["format"], "png", "got {body}");
    assert_eq!(img["source"]["bytes"], "AAAA", "got {body}");
}

#[test]
fn cache_control_breakpoint_validation_runs_in_converse_path() {
    // Arrange: 5 breakpoints exceeds the cap; the
    // cache_control::validate call must surface a clean
    // Validation error rather than letting the body ship upstream.
    let cfg = fake_cfg();
    let req = ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "a".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "b".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "c".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "d".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "e".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
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

    let err = normalize_request(&cfg, &req).unwrap_err();

    assert!(
        err.to_string().contains("breakpoints"),
        "expected breakpoint cap error; got: {err}"
    );
}
