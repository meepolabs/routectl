//! Unit tests for the Responses-API request orchestrator + sub-modules.
//!
//! Pulled into request.rs via `#[path = "request_tests.rs"] mod tests;`
//! to keep the orchestrator file under the 800-line cap while still
//! letting tests reach the `pub(crate)`-visible API surface.

use serde_json::{Value, from_value, json};

use routectl_core::{
    BEDROCK_MANTLE, CODEX_OAUTH, ChatRequest, ContentPart, CustomTool, KnownContentPart, Message,
    MessageContent, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, RefusalReason,
    ResponsesPassthroughItem, Role, SystemBlock, SystemContent, ToolDef,
    cache_control::CacheControl,
    schema::{ResponsesNamespaceContainer, ResponsesUnnamedMember},
};
use routectl_testkit::CapturedEvent;

use super::dropped_cache_surfaces;
use super::translate;
use crate::openai_responses::{AuthKind, OpenAiResponsesConfig};
use tracing_test::traced_test;

// The shared fixtures and the remaining test groups live in sibling files to
// keep each file under the size ceiling. They compile into THIS module via
// `include!`, so the helpers stay in scope and no test's module path changes.
include!("request_test_support.rs");

// ---------------------------------------------------------------------------
// system.rs
// ---------------------------------------------------------------------------

#[test]
fn system_content_concatenates_text_blocks_to_instructions() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Blocks(vec![
        SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: None,
            citations: None,
            extras: serde_json::Map::new(),
        },
        SystemBlock {
            kind: "text".into(),
            text: "be concise".into(),
            cache_control: None,
            citations: None,
            extras: serde_json::Map::new(),
        },
    ]));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["instructions"], json!("be helpful\n\nbe concise"));
}

/// Pin: a blank canonical system yields an empty `instructions` string on
/// the serialized body -- the Responses server's "no system prompt" -- never
/// a blank instruction. The field is always serialized on this wire, so an
/// empty string is the omission.
#[test]
fn blank_canonical_system_serializes_empty_instructions() {
    let blank = |text: &str| SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
        extras: serde_json::Map::new(),
    };
    for system in [
        SystemContent::Text(String::new()),
        SystemContent::Text("   \n\t ".into()),
        SystemContent::Blocks(vec![blank(""), blank("  \n")]),
    ] {
        // Arrange
        let mut req = req_with(vec![user_text("hi")]);
        req.system = Some(system);

        // Act
        let v = translate_to_json(&cfg(), &req);

        // Assert
        assert_eq!(
            v["instructions"],
            json!(""),
            "a blank canonical system must not produce a blank instruction: {v}"
        );
    }
}

#[test]
fn system_content_with_cache_control_warns_and_strips() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "be helpful".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
        extras: serde_json::Map::new(),
    }]));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: cache_control silently dropped on the wire; instructions
    // still carry the prompt text.
    assert_eq!(v["instructions"], json!("be helpful"));
    assert!(v.get("cache_control").is_none());
}

// ---------------------------------------------------------------------------
// messages.rs -- per-role translation
// ---------------------------------------------------------------------------

#[test]
fn user_text_message_translates_to_input_text() {
    // Arrange
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["input"],
        json!([
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "ping"}
            ]}
        ])
    );
}

#[test]
fn assistant_text_message_translates_to_output_text() {
    // Arrange
    let req = req_with(vec![user_text("ping"), assistant_text("pong")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let assistant = &v["input"][1];
    assert_eq!(assistant["type"], "message");
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"],
        json!([{"type": "output_text", "text": "pong"}])
    );
}

#[test]
fn assistant_thinking_with_unknown_format_emits_no_reasoning_item() {
    // Arrange: assistant turn with a Thinking block carrying a signature.
    // ContentPart::Thinking has no format field, so the egress cannot
    // establish that the signature is a token this lane will accept.
    let parts = vec![
        ContentPart::Known(KnownContentPart::Thinking {
            thinking: "step 1".into(),
            signature: Some("sig-xyz".into()),
            extras: serde_json::Map::new(),
        }),
        ContentPart::Known(KnownContentPart::Text {
            text: "final".into(),
            citations: None,
            cache_control: None,
            extras: serde_json::Map::new(),
        }),
    ];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: no reasoning item at all -- an item with empty
    // encrypted_content replays nothing and its id would dangle. The
    // message item follows the user turn directly.
    let message = &v["input"][1];
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(
        message["content"],
        json!([{"type": "output_text", "text": "final"}])
    );
    assert!(
        !v.to_string().contains("sig-xyz"),
        "an unverifiable signature must not reach the wire"
    );
}

#[test]
fn assistant_thinking_without_signature_emits_no_reasoning_item() {
    // Arrange
    let parts = vec![ContentPart::Known(KnownContentPart::Thinking {
        thinking: "hmm".into(),
        signature: None,
        extras: serde_json::Map::new(),
    })];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: nothing to replay, so nothing is emitted.
    assert!(
        v["input"][1].is_null(),
        "a signature-less thinking block must produce no input item"
    );
}

#[test]
fn assistant_redacted_thinking_does_not_leak_blob_into_encrypted_content() {
    // Arrange: assistant turn with a RedactedThinking part carrying an
    // opaque dialect-native blob. It is not a valid token for this lane's
    // encrypted_content slot and restores no artifact, so it must not be
    // forwarded.
    let secret_blob = "EroBCkYIBxgCKkB_ANTHROPIC_REDACTED_BLOB";
    let parts = vec![ContentPart::Known(KnownContentPart::RedactedThinking {
        data: secret_blob.into(),
        extras: serde_json::Map::new(),
    })];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: no reasoning item, and the raw blob appears nowhere.
    assert!(
        v["input"][1].is_null(),
        "an opaque redacted blob must produce no input item"
    );
    assert!(
        !v.to_string().contains(secret_blob),
        "an opaque foreign blob must not leak onto the Responses wire"
    );
}

#[test]
fn assistant_tool_use_translates_to_function_call() {
    // Arrange: assistant turn carrying a ToolUse part.
    let parts = vec![ContentPart::Known(KnownContentPart::ToolUse {
        id: "call_42".into(),
        name: "calc".into(),
        input: json!({"a": 1}),
        cache_control: None,
        extras: serde_json::Map::new(),
    })];
    let req = req_with(vec![user_text("compute"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: function_call input item follows the user message.
    let fc = &v["input"][1];
    assert_eq!(fc["type"], "function_call");
    assert_eq!(fc["call_id"], "call_42");
    assert_eq!(fc["name"], "calc");
    // Arguments must be a JSON string (Responses API quirk).
    assert!(fc["arguments"].is_string());
    let args: Value = serde_json::from_str(fc["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"a": 1}));
}

#[test]
fn tool_role_translates_to_function_call_output() {
    // Arrange
    let req = req_with(vec![user_text("compute"), tool_message("call_42", "42")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let fco = &v["input"][1];
    assert_eq!(fco["type"], "function_call_output");
    assert_eq!(fco["call_id"], "call_42");
    assert_eq!(fco["output"], "42");
}

// ---------------------------------------------------------------------------
// tools.rs
// ---------------------------------------------------------------------------

#[test]
fn tool_def_function_translates_with_strict_field() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "calc".into(),
        description: Some("do math".into()),
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: Some(true),
        type_tag: None,
        extras: serde_json::Map::new(),
    })]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: flat Responses shape (NOT the nested chat-completions shape).
    // The chatgpt-oauth backend 400s with "Missing required parameter:
    // 'tools[0].name'" on the nested {"type","function":{...}} shape.
    assert_eq!(
        v["tools"],
        json!([
            {
                "type": "function",
                "name": "calc",
                "description": "do math",
                "parameters": {"type": "object"},
                "strict": true
            }
        ])
    );
}

#[test]
fn tool_def_other_passes_through_verbatim() {
    // Arrange: Anthropic builtin shape carried through ToolDef::Other.
    let builtin = json!({
        "type": "bash_20250124",
        "name": "bash"
    });
    let mut req = req_with(vec![user_text("ping")]);
    req.tools = Some(vec![from_value::<ToolDef>(builtin.clone()).unwrap()]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: passthrough verbatim
    assert_eq!(v["tools"], json!([builtin]));
}

#[test]
fn passthrough_item_preserves_original_source_order() {
    // Arrange: inbound was [message, local_shell_call, message]; the
    // Responses ingress captured the unmodeled item with modeled_prefix=1
    // (one modeled item preceded it). The egress must re-emit it BETWEEN
    // the two messages, not shoved after both.
    let mut req = req_with(vec![
        user_text("first"),
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text("second".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        },
    ]);
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 1,
        item: json!({"type": "local_shell_call", "id": "lsc_1"}),
    }];

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: order is message, local_shell_call, message -- NOT
    // message, message, local_shell_call.
    let input = v["input"].as_array().expect("input array");
    assert_eq!(input.len(), 3, "got: {v}");
    assert_eq!(input[0]["role"], "user", "got: {v}");
    assert_eq!(input[1]["type"], "local_shell_call", "got: {v}");
    assert_eq!(input[2]["role"], "assistant", "got: {v}");
}

#[test]
fn tools_declared_by_a_replayed_additional_tools_item_are_not_repeated_top_level() {
    // Arrange: canonical tools = one top-level declaration plus the two an
    // `additional_tools` input item declared; the item itself rides the
    // passthrough for verbatim replay.
    let declared = json!([
        {"type": "function", "name": "shell", "parameters": {"type": "object"}},
        {"type": "web_search"}
    ]);
    let item = json!({"type": "additional_tools", "role": "developer", "tools": declared});
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![
        ToolDef::Custom(from_value(json!({"name": "top_only"})).unwrap()),
        ToolDef::Custom(
            from_value(json!({"name": "shell", "input_schema": {"type": "object"}})).unwrap(),
        ),
        ToolDef::Other(json!({"type": "web_search"})),
    ]);
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 0,
        item: item.clone(),
    }];

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "got: {v}");
    assert_eq!(tools[0]["name"], "top_only");
    assert_eq!(v["input"][0], item, "the declaring item replays verbatim");
}

#[test]
fn a_malformed_inline_function_does_not_suppress_a_same_name_top_level_tool() {
    for malformed in [
        json!({"type": "function", "name": "shell", "description": 42}),
        json!({"type": "function", "name": "shell", "strict": "yes"}),
    ] {
        // Arrange: the inline declaration names `shell` but a mistyped field
        // keeps it from normalizing, so the canonical `shell` is the valid
        // top-level one and nothing else declares it upstream.
        let item = json!({"type": "additional_tools", "role": "developer", "tools": [malformed]});
        let mut req = req_with(vec![user_text("hi")]);
        req.tools = Some(vec![ToolDef::Custom(
            from_value(json!({"name": "shell", "description": "top-level"})).unwrap(),
        )]);
        req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
            modeled_prefix: 0,
            item: item.clone(),
        }];

        // Act
        let v = translate_to_json(&cfg(), &req);

        // Assert
        let tools = v["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1, "got: {v}");
        assert_eq!(tools[0]["name"], "shell", "got: {v}");
        assert_eq!(tools[0]["description"], "top-level", "got: {v}");
        assert_eq!(
            v["input"][0], item,
            "the inline item still replays verbatim"
        );
    }
}

#[test]
fn a_valid_inline_function_still_suppresses_the_same_name_top_level_tool() {
    // Arrange: positive control for the test above -- a normalizable inline
    // declaration of the same name does suppress the top-level one.
    let valid = json!({"type": "function", "name": "shell", "description": "inline"});
    let item = json!({"type": "additional_tools", "role": "developer", "tools": [valid]});
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![
        ToolDef::Custom(from_value(json!({"name": "shell", "description": "inline"})).unwrap()),
        ToolDef::Custom(from_value(json!({"name": "top_only"})).unwrap()),
    ]);
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 0,
        item,
    }];

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let names: Vec<&str> = v["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, vec!["top_only"], "got: {v}");
}

fn ns_function(name: &str) -> Value {
    json!({"type": "function", "name": name, "parameters": {"type": "object"}})
}

fn ns_custom(name: &str) -> ToolDef {
    ToolDef::Custom(CustomTool::from_responses_function(&ns_function(name)).expect("normalizes"))
}

/// A request shaped the way the Responses ingress leaves one with a
/// top-level `[flat, namespace, flat]` tools array: the members flattened in
/// place, the container recorded at index 1.
fn req_with_top_level_namespace() -> (ChatRequest, Value) {
    let container = json!({
        "type": "namespace",
        "name": "ns_alpha",
        "tools": [
            ns_function("alpha_wait"),
            {"type": "custom", "name": "exec", "format": {"type": "text"}},
            {"type": "web_search"}
        ]
    });
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![
        ns_custom("flat_one"),
        ns_custom("alpha_wait"),
        ToolDef::Other(container["tools"][1].clone()),
        ToolDef::Other(container["tools"][2].clone()),
        ns_custom("flat_two"),
    ]);
    let ns = &mut req.routectl_internal.responses_namespaces;
    ns.members = [("alpha_wait", "ns_alpha"), ("exec", "ns_alpha")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    ns.unnamed_members = vec![ResponsesUnnamedMember {
        occurrence: 0,
        raw: container["tools"][2].clone(),
    }];
    ns.containers = vec![ResponsesNamespaceContainer {
        index: 1,
        raw: container.clone(),
    }];
    (req, container)
}

#[test]
fn a_top_level_namespace_is_spliced_back_and_its_members_are_not_redeclared() {
    // Arrange
    let (req, container) = req_with_top_level_namespace();

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["tools"],
        json!([ns_function("flat_one"), container, ns_function("flat_two")]),
        "got: {v}"
    );
}

#[test]
fn members_of_an_inline_namespace_are_declared_only_by_the_replayed_item() {
    // Arrange: the container rides the additional_tools item, so nothing is
    // spliced and no member is declared top-level.
    let item = json!({
        "type": "additional_tools",
        "role": "developer",
        "tools": [{"type": "namespace", "name": "ns_beta", "tools": [ns_function("beta_read")]}]
    });
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![ns_custom("beta_read")]);
    req.routectl_internal
        .responses_namespaces
        .members
        .insert("beta_read".into(), "ns_beta".into());
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 0,
        item: item.clone(),
    }];

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(v.get("tools").is_none(), "got: {v}");
    assert_eq!(v["input"][0], item, "the declaring item replays verbatim");
}

#[test]
fn a_flat_tools_array_without_namespaces_serializes_as_before() {
    // Arrange: positive control; nothing recorded, nothing suppressed.
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![
        ns_custom("flat_one"),
        ToolDef::Other(json!({"type": "web_search"})),
    ]);

    // Act
    let body = serde_json::to_vec(&translate(&cfg(), &req).expect("translate")).unwrap();

    // Assert
    let tools = r#""tools":[{"type":"function","name":"flat_one","parameters":{"type":"object"}},{"type":"web_search"}]"#;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains(tools), "got: {body}");
}

#[test]
fn only_additional_tools_items_suppress_top_level_tools() {
    // Arrange: positive control -- an unrelated passthrough kind that
    // happens to carry a `tools` key must not hide anything.
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![ToolDef::Custom(
        from_value(json!({"name": "shell"})).unwrap(),
    )]);
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 0,
        item: json!({"type": "tool_search_output", "tools": [{"type": "function", "name": "shell"}]}),
    }];

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["tools"][0]["name"], "shell", "got: {v}");
}

#[test]
fn tool_choice_auto_serializes_as_string() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.tool_choice = Some(json!("auto"));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["tool_choice"], json!("auto"));
}

#[test]
fn tool_choice_required_serializes_as_string() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.tool_choice = Some(json!("required"));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["tool_choice"], json!("required"));
}

#[test]
fn tool_choice_none_serializes_as_string() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.tool_choice = Some(json!("none"));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["tool_choice"], json!("none"));
}

#[test]
fn tool_choice_named_function_uses_flat_shape() {
    // Arrange: OpenAI-shape input (nested {"type","function":{"name":...}}).
    let mut req = req_with(vec![user_text("ping")]);
    req.tool_choice = Some(json!({
        "type": "function",
        "function": {"name": "calc"}
    }));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: flat Responses shape on the wire (NOT nested chat-completions).
    // The chatgpt-oauth backend 400s with "Unknown parameter:
    // 'tool_choice.function'" on the nested shape (smoke 2026-05-12).
    assert_eq!(
        v["tool_choice"],
        json!({"type": "function", "name": "calc"})
    );
}

include!("request_extras_tests.rs");
include!("request_content_tests.rs");
include!("request_lane_observability_tests.rs");
include!("request_drop_policy_tests.rs");
include!("request_tool_output_tests.rs");

include!("request_system_role_tests.rs");
