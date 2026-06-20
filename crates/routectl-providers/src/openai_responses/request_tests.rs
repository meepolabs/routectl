//! Unit tests for the Responses-API request orchestrator + sub-modules.
//!
//! Pulled into request.rs via `#[path = "request_tests.rs"] mod tests;`
//! to keep the orchestrator file under the 800-line cap while still
//! letting tests reach the `pub(crate)`-visible API surface.

use serde_json::{from_value, json, Value};

use routectl_core::{
    cache_control::CacheControl, ChatRequest, ContentPart, CustomTool, KnownContentPart, Message,
    MessageContent, ReasoningConfig, Role, SystemBlock, SystemContent, ToolDef,
};

use super::translate;
use crate::openai_responses::{AuthKind, OpenAiResponsesConfig};
// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

fn cfg() -> OpenAiResponsesConfig {
    let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
    c.account_id = Some("acct-uuid".into());
    c.auth_kind = AuthKind::ChatgptOauth;
    c
}

fn req_with(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        messages,
        ..Default::default()
    }
}

fn user_text(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_parts(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_text(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn tool_message(call_id: &str, output: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text(output.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some(call_id.into()),
        tool_calls: None,
    }
}

/// Convert the request to a JSON Value -- avoids over-tying tests to
/// the struct internals when serde wire shape is what we care about.
fn translate_to_json(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> Value {
    let r = translate(cfg, req).expect("translate");
    serde_json::to_value(&r).expect("serialize")
}

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
        },
        SystemBlock {
            kind: "text".into(),
            text: "be concise".into(),
            cache_control: None,
            citations: None,
        },
    ]));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["instructions"], json!("be helpful\n\nbe concise"));
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
fn assistant_thinking_with_unknown_format_emits_empty_encrypted_content() {
    // Arrange: assistant turn with a Thinking block carrying a signature.
    // ContentPart::Thinking has no format field, so the egress cannot
    // know whether the signature is an Anthropic or OpenAI token.
    // Correct behavior: emit a Reasoning input item with EMPTY
    // encrypted_content (Anthropic signatures are not valid OpenAI
    // encrypted_content tokens and must not be forwarded).
    let parts = vec![
        ContentPart::Known(KnownContentPart::Thinking {
            thinking: "step 1".into(),
            signature: Some("sig-xyz".into()),
        }),
        ContentPart::Known(KnownContentPart::Text {
            text: "final".into(),
            cache_control: None,
        }),
    ];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: reasoning item emitted FIRST, then the message item.
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    // Signature must NOT be forwarded: KnownContentPart::Thinking carries
    // no format tag so the egress cannot verify it is a valid OpenAI
    // encrypted_content token. Empty string is the documented "no prior
    // signature" shape (codex arc_monitor.rs:325-336 treats it as no-op).
    assert_eq!(reasoning["encrypted_content"], "");
    assert_eq!(
        reasoning["summary"],
        json!([{"type": "summary_text", "text": "step 1"}])
    );
    let message = &v["input"][2];
    assert_eq!(message["type"], "message");
    assert_eq!(message["role"], "assistant");
    assert_eq!(
        message["content"],
        json!([{"type": "output_text", "text": "final"}])
    );
}

#[test]
fn assistant_thinking_without_signature_emits_empty_encrypted_content() {
    // Arrange
    let parts = vec![ContentPart::Known(KnownContentPart::Thinking {
        thinking: "hmm".into(),
        signature: None,
    })];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: encrypted_content emitted as empty string, not null.
    // codex's arc_monitor.rs:325-336 treats empty as "no replay" so
    // this is safe.
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["encrypted_content"], "");
    assert!(reasoning["encrypted_content"].is_string());
}

#[test]
fn assistant_redacted_thinking_does_not_leak_blob_into_encrypted_content() {
    // Arrange: assistant turn with a RedactedThinking part. The opaque
    // Anthropic base64 blob is NOT a valid OpenAI encrypted_content
    // token and must not be forwarded into that slot.
    let secret_blob = "EroBCkYIBxgCKkB_ANTHROPIC_REDACTED_BLOB";
    let parts = vec![ContentPart::Known(KnownContentPart::RedactedThinking {
        data: secret_blob.into(),
    })];
    let req = req_with(vec![user_text("ping"), assistant_parts(parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: a reasoning item is emitted with EMPTY encrypted_content;
    // the raw blob does not appear anywhere on the wire.
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["encrypted_content"], "");
    assert!(
        !v.to_string().contains(secret_blob),
        "redacted Anthropic blob must not leak onto the Responses wire"
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

// ---------------------------------------------------------------------------
// extras.rs -- reasoning + provider_extras
// ---------------------------------------------------------------------------

#[test]
fn reasoning_effort_maps_to_responses_reasoning() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: None,
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["reasoning"], json!({"effort": "high", "summary": "auto"}));
}

#[test]
fn reasoning_max_tokens_warns_and_drops() {
    // Arrange: caller supplied a budget. Effort still flows through.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("medium".into()),
        max_tokens: Some(2048),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: no budget field on the wire; effort survives.
    let r = &v["reasoning"];
    assert_eq!(r["effort"], "medium");
    assert!(r.get("max_tokens").is_none());
    assert!(r.get("budget_tokens").is_none());
}

#[test]
fn reasoning_budget_only_maps_to_effort_band() {
    // Arrange: caller supplied only a budget (no explicit effort).
    // 8192 sits in the medium band (1025..=8192) per the reverse table.
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(8192),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: budget is mapped to "medium" rather than dropped.
    assert_eq!(
        v["reasoning"],
        json!({"effort": "medium", "summary": "auto"})
    );
}

#[test]
fn reasoning_explicit_effort_wins_over_budget() {
    // Arrange: both set. Explicit effort must win; budget is ignored
    // (it would map to the medium band but "high" takes precedence).
    let mut req = req_with(vec![user_text("ping")]);
    req.reasoning = Some(ReasoningConfig {
        effort: Some("high".into()),
        max_tokens: Some(8192),
        exclude: None,
        enabled: None,
    });

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["reasoning"]["effort"], "high");
}

#[test]
fn provider_extras_prompt_cache_key_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"prompt_cache_key": "user-42"}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["prompt_cache_key"], "user-42");
}

#[test]
fn provider_extras_service_tier_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"service_tier": "priority"}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["service_tier"], "priority");
}

#[test]
fn provider_extras_text_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"text": {"verbosity": "high"}}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: text passthrough preserves the operator-supplied shape.
    assert_eq!(v["text"], json!({"verbosity": "high"}));
}

#[test]
fn provider_extras_include_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"include": ["reasoning.encrypted_content"]}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn provider_extras_unknown_key_does_not_forward() {
    // Arrange: long-tail key the egress doesn't recognize.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"frequency_penalty_v2": 0.5}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(v.get("frequency_penalty_v2").is_none());
}

#[test]
fn store_false_hardcoded_for_chatgpt_oauth() {
    // Arrange: default cfg uses ChatgptOauth.
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["store"], json!(false));
}

#[test]
fn store_provider_extras_override_ignored_for_chatgpt_oauth() {
    // Arrange: operator tries to flip store on -- must be ignored.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"store": true}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["store"], json!(false));
}

/// A non-chatgpt-oauth config (api-key path) where store stays false by
/// default. Used to exercise the store override + include-forcing logic.
fn cfg_api_key() -> OpenAiResponsesConfig {
    let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
    c.auth_kind = AuthKind::ApiKey;
    c
}

#[test]
fn store_false_forces_encrypted_reasoning_include() {
    // Arrange: default chatgpt-oauth, store false, no operator include.
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: include carries the encrypted-reasoning carrier so the
    // upstream returns a non-empty encrypted_content for later replay.
    assert_eq!(v["store"], json!(false));
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn store_true_does_not_force_encrypted_reasoning_include() {
    // Arrange: api-key path with an explicit store=true override (server
    // retains reasoning, so no include is needed).
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"store": true}));

    // Act
    let v = translate_to_json(&cfg_api_key(), &req);

    // Assert: store honored, include NOT force-added.
    assert_eq!(v["store"], json!(true));
    assert!(
        v.get("include").is_none(),
        "include must not be forced when store is true; got: {v}"
    );
}

#[test]
fn explicit_operator_include_is_respected_not_overwritten() {
    // Arrange: operator pins include to a custom value; store false.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({"include": ["message.output_text.logprobs"]}));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: the operator value is honored verbatim (NOT augmented with
    // the encrypted-reasoning carrier).
    assert_eq!(v["include"], json!(["message.output_text.logprobs"]));
}

// ---------------------------------------------------------------------------
// user image content
// ---------------------------------------------------------------------------

fn user_image_base64(media_type: &str, data: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "base64",
                "media_type": media_type,
                "data": data
            }),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_image_url(url: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "url",
                "url": url
            }),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

#[test]
fn user_image_base64_translates_to_input_image_data_url() {
    // Arrange: user turn containing a base64 PNG image.
    let req = req_with(vec![user_image_base64("image/png", "AAAA")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: content block becomes {type:"input_image",
    // image_url:"data:image/png;base64,AAAA"}.
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_image");
    assert_eq!(content["image_url"], "data:image/png;base64,AAAA");
    // detail is absent (None -> omitted).
    assert!(content.get("detail").is_none());
}

#[test]
fn user_image_url_translates_to_input_image_url() {
    // Arrange: user turn carrying an https URL image source.
    let req = req_with(vec![user_image_url("https://example.com/cat.jpg")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_image");
    assert_eq!(content["image_url"], "https://example.com/cat.jpg");
}

#[test]
fn user_image_unknown_source_kind_warns_and_drops() {
    // Arrange: source.type is an unsupported kind (forward-compat
    // extension). The part should be dropped; the message item should
    // still be emitted but with no content blocks (empty -> skipped).
    let msg = Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({"type": "s3", "bucket": "my-bucket", "key": "img.png"}),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![msg]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: the single unknown-source image was dropped so the user
    // message has no content and was skipped entirely.
    assert_eq!(v["input"], json!([]));
}

// ---------------------------------------------------------------------------
// user file content (OpenAI-shape File -> Responses input_file)
// ---------------------------------------------------------------------------

fn user_file(file: Value) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::File {
            file,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

#[test]
fn user_file_data_translates_to_input_file_with_filename() {
    // Arrange: OpenAI-shape file part carrying inline base64 + filename.
    let req = req_with(vec![user_file(json!({
        "filename": "draft.pdf",
        "file_data": "data:application/pdf;base64,JVBER"
    }))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: an input_file item carries file_data + filename (no drop).
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_file");
    assert_eq!(content["file_data"], "data:application/pdf;base64,JVBER");
    assert_eq!(content["filename"], "draft.pdf");
    assert!(content.get("file_id").is_none());
}

#[test]
fn user_file_id_only_translates_to_input_file_with_file_id() {
    // Arrange: OpenAI-shape file part referencing a prior upload.
    let req = req_with(vec![user_file(json!({"file_id": "file-abc123"}))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: input_file item carries file_id; file_data/filename absent.
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_file");
    assert_eq!(content["file_id"], "file-abc123");
    assert!(content.get("file_data").is_none());
    assert!(content.get("filename").is_none());
}

#[test]
fn user_file_with_no_carrier_is_dropped() {
    // Arrange: a file part with neither file_data nor file_id.
    let req = req_with(vec![user_file(json!({"filename": "empty.pdf"}))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: nothing to forward; the user message is skipped entirely.
    assert_eq!(v["input"], json!([]));
}

#[test]
fn user_document_anthropic_shape_still_drops() {
    // Arrange: Anthropic-shape Document part (out of scope for the
    // codex target; remains dropped at parity with the reference).
    let msg = Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBER"
            }),
            title: Some("spec.pdf".into()),
            citations: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![msg]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: Document is dropped; no content -> message skipped.
    assert_eq!(v["input"], json!([]));
}

// ---------------------------------------------------------------------------
// tool result with image parts
// ---------------------------------------------------------------------------

fn tool_message_parts(call_id: &str, parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some(call_id.into()),
        tool_calls: None,
    }
}

#[test]
fn tool_role_text_only_translates_to_string_output() {
    // Arrange: single text part -- common path.
    let req = req_with(vec![user_text("run"), tool_message("call_1", "result")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: output is a flat string.
    let fco = &v["input"][1];
    assert_eq!(fco["type"], "function_call_output");
    assert_eq!(fco["call_id"], "call_1");
    assert_eq!(fco["output"], json!("result"));
}

#[test]
fn tool_role_with_image_part_translates_to_items_array() {
    // Arrange: tool result contains only an image (e.g. screenshot tool).
    let parts = vec![ContentPart::Known(KnownContentPart::Image {
        source: json!({
            "type": "base64",
            "media_type": "image/png",
            "data": "iVBORw"
        }),
        cache_control: None,
    })];
    let req = req_with(vec![
        user_text("screenshot"),
        tool_message_parts("call_9", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: output is an items array with one input_image entry.
    let fco = &v["input"][1];
    assert_eq!(fco["type"], "function_call_output");
    assert_eq!(
        fco["output"],
        json!([
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw"}
        ])
    );
}

#[test]
fn tool_role_mixed_text_and_image_emits_items_array() {
    // Arrange: tool result has both text and an image.
    let parts = vec![
        ContentPart::Known(KnownContentPart::Text {
            text: "here is the screenshot".into(),
            cache_control: None,
        }),
        ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "url",
                "url": "https://example.com/shot.png"
            }),
            cache_control: None,
        }),
    ];
    let req = req_with(vec![
        user_text("screenshot"),
        tool_message_parts("call_7", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: mixed -> items array with both kinds present.
    let fco = &v["input"][1];
    assert_eq!(
        fco["output"],
        json!([
            {"type": "input_text", "text": "here is the screenshot"},
            {"type": "input_image", "image_url": "https://example.com/shot.png"}
        ])
    );
}

// ---------------------------------------------------------------------------
// client_metadata passthrough
// ---------------------------------------------------------------------------

#[test]
fn provider_extras_client_metadata_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({
        "client_metadata": {"user_id": "u-123", "session": "s-abc"}
    }));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: client_metadata forwarded verbatim.
    assert_eq!(
        v["client_metadata"],
        json!({"user_id": "u-123", "session": "s-abc"})
    );
}

// ---------------------------------------------------------------------------
// multi-turn reasoning replay round-trip
//
// These tests prove that an assistant turn carrying response-side
// reasoning_details (with the openai-responses-v1 format tag) survives
// the second-turn request translation: the encrypted_content signature
// the upstream issued must reach the next /responses POST verbatim or
// Anthropic/OpenAI reasoning-enabled models return 400.
// ---------------------------------------------------------------------------

#[test]
fn response_reasoning_round_trips_through_canonical_to_replay_request() {
    use crate::openai_responses::response;
    use crate::openai_responses::response_types::ResponsesResponse;

    // Arrange: a fake upstream response carrying a Reasoning output
    // item with a signature + summary + inner text. Drive it through
    // the response translator to a canonical ChatResponse, then build
    // a new request whose message[1] is the translated assistant turn
    // and assert the egress emits a Reasoning input item carrying the
    // original encrypted_content + id.
    let upstream_body = json!({
        "id": "resp_01",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "step"}],
                "content": [{"type": "reasoning_text", "text": "detail"}],
                "encrypted_content": "sig_xyz"
            },
            {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        ]
    });
    let typed: ResponsesResponse = from_value(upstream_body).unwrap();
    let chat_response = response::translate("test", typed).unwrap();
    let assistant_msg = chat_response.choices[0].message.clone();

    // Act: build a fresh request whose second message is the assistant
    // turn from the upstream response. The egress must lift
    // reasoning_details back into a Reasoning input item.
    let req = req_with(vec![user_text("ping"), assistant_msg]);
    let v = translate_to_json(&cfg(), &req);

    // Assert: input[1] is a Reasoning item carrying the original
    // signature, id, and summary/content surfaces.
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "sig_xyz");
    assert_eq!(
        reasoning["summary"],
        json!([{"type": "summary_text", "text": "step"}])
    );
    assert_eq!(
        reasoning["content"],
        json!([{"type": "reasoning_text", "text": "detail"}])
    );
    // input[2] is the assistant message text (no Thinking duplication
    // because reasoning_details produced the Reasoning item).
    let msg = &v["input"][2];
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["role"], "assistant");
    assert_eq!(
        msg["content"],
        json!([{"type": "output_text", "text": "answer"}])
    );
}

#[test]
fn sse_reasoning_round_trips_through_canonical_to_replay_request() {
    use crate::openai_responses::sse::ResponsesStreamState;
    use routectl_core::{ReasoningDetail, ReasoningDetailKind};

    // Arrange: synthesize a streaming session by feeding events
    // through parse_event, then collect the emitted reasoning_details
    // into a synthetic assistant message. The replay request must then
    // carry the same encrypted_content signature.
    let events = vec![
        json!({"type": "response.created", "response": {"id": "r", "model": "m"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": []}
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "step"
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": [],
                     "encrypted_content": "sig_xyz"}
        }),
    ];
    let mut state = ResponsesStreamState::default();
    let mut all_details: Vec<ReasoningDetail> = Vec::new();
    for ev in events {
        let typed = serde_json::from_value(ev).unwrap();
        for chunk in state.parse_event("test", typed).unwrap() {
            all_details.extend(chunk.choices[0].delta.reasoning_details.clone());
        }
    }
    // Sanity: at least one Encrypted detail with the upstream id.
    let enc = all_details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .expect("Encrypted detail emitted");
    assert_eq!(enc.id.as_deref(), Some("rs_1"));
    assert_eq!(enc.payload["encrypted_content"], "sig_xyz");

    // Promote the accumulated details onto a synthetic assistant
    // message and drive translate_request to assert the encrypted_content
    // reaches the egress wire body.
    let assistant = Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("answer".into()),
        reasoning: None,
        reasoning_details: all_details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![user_text("ping"), assistant]);
    let v = translate_to_json(&cfg(), &req);
    let reasoning = &v["input"][1];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["id"], "rs_1");
    assert_eq!(reasoning["encrypted_content"], "sig_xyz");
}

// ---------------------------------------------------------------------------
// v0.8 max_tokens injection contract: openai-responses MUST NOT inject
// ---------------------------------------------------------------------------

/// The openai-responses egress MUST NOT inject `max_tokens` when the
/// caller omits it. Mirrors the openai-compat negative-injection test
/// (`openai_compat_does_not_inject_max_tokens_when_caller_omitted`).
/// The good-translator principle: only inject where the upstream
/// demands it (Anthropic-shape egresses). The
/// `routectl_internal.max_output_tokens` carrier is Anthropic-shape
/// territory and must NOT leak onto the openai-responses wire body
/// regardless of its value.
#[test]
fn openai_responses_does_not_inject_max_tokens_when_caller_omitted() {
    let mut req = req_with(vec![user_text("hi")]);
    req.max_tokens = None;
    // Pin: even when the router carrier carries a non-zero value
    // (e.g. when a `[models.X].max_output_tokens` override sits on a
    // model that happens to route through openai-responses), the
    // egress must NOT lift it onto the wire body.
    req.routectl_internal.max_output_tokens = 8000;
    let v = translate_to_json(&cfg(), &req);
    assert!(
        v.get("max_tokens").is_none(),
        "openai-responses egress must not inject max_tokens; got: {v}"
    );
    assert!(
        v.get("max_output_tokens").is_none(),
        "openai-responses egress must not inject max_output_tokens either; got: {v}"
    );
}

// ---------------------------------------------------------------------------
// dropped cache_control observability
//
// The Responses API has no prompt-cache breakpoint surface, so dropping
// caller `cache_control` markers is CORRECT; these tests pin that the
// drop is OBSERVABLE (a single WARN naming the surfaces) and that the
// wire body is UNCHANGED by the diagnostic. `system`-level markers are
// excluded here -- system.rs already logs that drop at DEBUG.
// ---------------------------------------------------------------------------

use super::dropped_cache_surfaces;
use tracing_test::traced_test;

fn user_text_part_with_cc(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
            text: text.into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn custom_tool_with_cc(name: &str) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: Some(CacheControl::ephemeral_5m()),
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

#[test]
fn dropped_surfaces_detects_top_level_marker() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.cache_control = Some(CacheControl::ephemeral_5m());

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["top-level"]);
}

#[test]
fn dropped_surfaces_detects_per_part_marker() {
    // Arrange
    let req = req_with(vec![user_text_part_with_cc("hi")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["messages"]);
}

#[test]
fn dropped_surfaces_detects_per_tool_marker() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.tools = Some(vec![custom_tool_with_cc("calc")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert_eq!(surfaces, vec!["tools"]);
}

#[test]
fn dropped_surfaces_excludes_system_already_logged_at_debug() {
    // Arrange: only a system-block marker. system.rs owns that DEBUG log,
    // so this helper must NOT re-report it (avoids a double-log).
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "sys".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
    }]));

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert!(
        surfaces.is_empty(),
        "system marker must not be re-reported: {surfaces:?}"
    );
}

#[test]
fn dropped_surfaces_empty_for_clean_request() {
    // Arrange: no markers anywhere.
    let req = req_with(vec![user_text("hi")]);

    // Act
    let surfaces = dropped_cache_surfaces(&req);

    // Assert
    assert!(surfaces.is_empty());
}

#[traced_test]
#[test]
fn warn_fires_for_top_level_marker_and_wire_is_unchanged() {
    // Arrange: identical requests, one with a top-level marker.
    let clean = req_with(vec![user_text("hi")]);
    let mut hinted = req_with(vec![user_text("hi")]);
    hinted.cache_control = Some(CacheControl::ephemeral_5m());

    // Act
    let clean_wire = translate_to_json(&cfg(), &clean);
    let hinted_wire = translate_to_json(&cfg(), &hinted);

    // Assert: the diagnostic fired, names the surface, and the wire body
    // is byte-identical to the unhinted request (cache_control never rode
    // the Responses wire to begin with).
    assert!(
        logs_contain("cache_control dropped"),
        "drop diagnostic must fire for a top-level marker"
    );
    assert_eq!(clean_wire, hinted_wire);
    assert!(hinted_wire.get("cache_control").is_none());
}

#[traced_test]
#[test]
fn no_warn_for_clean_request() {
    // Arrange
    let req = req_with(vec![user_text("hi")]);

    // Act
    let _ = translate(&cfg(), &req).expect("translate");

    // Assert: a request with no caller markers emits no drop diagnostic.
    assert!(
        !logs_contain("cache_control dropped"),
        "no drop diagnostic should fire when no caller marker is present"
    );
}
