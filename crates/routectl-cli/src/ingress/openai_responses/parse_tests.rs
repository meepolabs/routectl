//! Behavior tests for the OpenAI Responses ingress request parser.
//!
//! Mirrors the egress request_tests density: each Responses input-item
//! kind and top-level field has a behavior-named AAA test, plus the
//! three statefulness branches and the graceful-degradation path.

use super::*;
use axum::http::header::HeaderName;
use axum::http::HeaderMap;
use routectl_core::{
    ContentPart, Error, KnownContentPart, MessageContent, ReasoningDetailKind, Role, SystemContent,
    ToolDef,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse(body: serde_json::Value) -> ChatRequest {
    ResponsesIngress
        .parse_request(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn parse_with_headers(headers: &HeaderMap, body: serde_json::Value) -> ChatRequest {
    ResponsesIngress
        .parse_request(headers, body)
        .expect("request should parse")
}

fn alias_headers(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        HeaderName::from_static(crate::ingress::ALIAS_HEADER),
        value.parse().unwrap(),
    );
    h
}

// ---------------------------------------------------------------------------
// model + alias header
// ---------------------------------------------------------------------------

#[test]
fn maps_model_field_to_canonical_model() {
    // Arrange
    let body = json!({ "model": "gpt-5-codex", "input": "hi" });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.model, "gpt-5-codex");
}

#[test]
fn alias_header_overrides_wire_model() {
    // Arrange
    let headers = alias_headers("fast");
    let body = json!({ "model": "gpt-5-codex", "input": "hi" });

    // Act
    let req = parse_with_headers(&headers, body);

    // Assert
    assert_eq!(req.model, "fast");
}

#[test]
fn stamps_openai_provenance() {
    // Arrange
    let body = json!({ "model": "m", "input": "hi" });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(
        req.routectl_internal.provenance,
        routectl_core::RequestProvenance::OpenaiIngress
    );
}

// ---------------------------------------------------------------------------
// instructions -> system
// ---------------------------------------------------------------------------

#[test]
fn lifts_instructions_into_system() {
    // Arrange
    let body = json!({ "model": "m", "instructions": "be terse", "input": "hi" });

    // Act
    let req = parse(body);

    // Assert
    match req.system {
        Some(SystemContent::Text(s)) => assert_eq!(s, "be terse"),
        other => panic!("expected SystemContent::Text, got {other:?}"),
    }
}

#[test]
fn empty_instructions_yields_no_system() {
    // Arrange: the egress always serializes instructions, even as "".
    let body = json!({ "model": "m", "instructions": "", "input": "hi" });

    // Act
    let req = parse(body);

    // Assert
    assert!(
        req.system.is_none(),
        "empty instructions must not set system"
    );
}

// ---------------------------------------------------------------------------
// input: bare string
// ---------------------------------------------------------------------------

#[test]
fn bare_string_input_becomes_single_user_message() {
    // Arrange
    let body = json!({ "model": "m", "input": "what is 2+2?" });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::User));
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "what is 2+2?"));
}

// ---------------------------------------------------------------------------
// input: message items
// ---------------------------------------------------------------------------

#[test]
fn user_message_item_with_input_text_part() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: single text part collapses to a flat Text content.
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::User));
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "hello"));
}

#[test]
fn assistant_message_item_with_output_text_part() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "the answer"}]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    assert!(matches!(req.messages[0].role, Role::Assistant));
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "the answer"));
}

#[test]
fn message_item_with_bare_string_content() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [{"type": "message", "role": "user", "content": "plain"}]
    });

    // Act
    let req = parse(body);

    // Assert
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "plain"));
}

#[test]
fn message_item_with_multiple_text_parts_stays_parts() {
    // Arrange: two text parts must not collapse (only a lone part does).
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "first"},
                {"type": "input_text", "text": "second"}
            ]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    match &req.messages[0].content {
        MessageContent::Parts(parts) => assert_eq!(parts.len(), 2),
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn message_item_with_input_image_part_preserved() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_image", "image_url": "https://example.com/x.png", "detail": "high"}
            ]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: image survives as a canonical OpenAI-shape ImageUrl part.
    match &req.messages[0].content {
        MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 1);
            match &parts[0] {
                ContentPart::Known(KnownContentPart::ImageUrl { image_url, .. }) => {
                    assert_eq!(image_url["url"], "https://example.com/x.png");
                    assert_eq!(image_url["detail"], "high");
                }
                other => panic!("expected ImageUrl part, got {other:?}"),
            }
        }
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn message_item_with_unknown_part_kind_preserved_as_other() {
    // Arrange: an unknown content block type must survive as Other, not
    // be dropped.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_audio", "audio": {"data": "AAAA"}}
            ]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    match &req.messages[0].content {
        MessageContent::Parts(parts) => match &parts[0] {
            ContentPart::Other {
                type_tag, extras, ..
            } => {
                assert_eq!(type_tag, "input_audio");
                assert_eq!(extras["audio"]["data"], "AAAA");
            }
            other => panic!("expected Other part, got {other:?}"),
        },
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn message_item_without_type_but_with_role_is_treated_as_message() {
    // Arrange: the Responses API tolerates a `{role, content}` object
    // with no explicit type tag as an implicit message item.
    let body = json!({
        "model": "m",
        "input": [{"role": "user", "content": "implicit"}]
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "implicit"));
}

#[test]
fn system_role_message_item_maps_to_role_system() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [{"type": "message", "role": "system", "content": "sys"}]
    });

    // Act
    let req = parse(body);

    // Assert: in-array system items are lifted into req.system (matching
    // lift_system_messages semantics), not left as loose Role::System
    // messages in the messages array.
    match req.system {
        Some(SystemContent::Text(s)) => assert_eq!(s, "sys"),
        other => panic!("expected SystemContent::Text, got {other:?}"),
    }
    assert!(
        req.messages.is_empty(),
        "system message must be removed from messages array"
    );
}

#[test]
fn developer_role_message_item_maps_to_role_system() {
    // Arrange: the Responses `developer` role is a privileged-instruction
    // role; canonical has no distinct developer role, so it folds into
    // System (same target as `system`). Pin it so a later edit can't
    // silently demote it to user.
    let body = json!({
        "model": "m",
        "input": [{"type": "message", "role": "developer", "content": "dev"}]
    });

    // Act
    let req = parse(body);

    // Assert: developer role items are lifted into req.system, not left
    // as loose Role::System messages in the messages array.
    match req.system {
        Some(SystemContent::Text(s)) => assert_eq!(s, "dev"),
        other => panic!("expected SystemContent::Text, got {other:?}"),
    }
    assert!(
        req.messages.is_empty(),
        "developer message must be removed from messages array"
    );
}

// ---------------------------------------------------------------------------
// function_call -> tool_calls
// ---------------------------------------------------------------------------

#[test]
fn function_call_item_attaches_to_assistant_tool_calls() {
    // Arrange: an assistant message followed by its function_call item.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "calling tool"}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather",
             "arguments": "{\"city\":\"SF\"}"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: the call attaches to the existing assistant turn.
    assert_eq!(req.messages.len(), 1);
    let calls = req.messages[0]
        .tool_calls
        .as_ref()
        .expect("tool_calls present");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "call_1");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"SF\"}");
}

#[test]
fn function_call_item_opens_assistant_turn_when_none_trailing() {
    // Arrange: a function_call with no preceding assistant message.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "call_9", "name": "f", "arguments": "{}"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: a fresh assistant turn carries the call.
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(req.messages[1].role, Role::Assistant));
    let calls = req.messages[1]
        .tool_calls
        .as_ref()
        .expect("tool_calls present");
    assert_eq!(calls[0]["id"], "call_9");
}

#[test]
fn function_call_item_falls_back_to_id_when_call_id_absent() {
    // Arrange: some clients ship `id` instead of `call_id`.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "function_call", "id": "fc_42", "name": "f", "arguments": "{}"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    let calls = req.messages[0].tool_calls.as_ref().unwrap();
    assert_eq!(calls[0]["id"], "fc_42");
}

// ---------------------------------------------------------------------------
// function_call_output -> tool result message
// ---------------------------------------------------------------------------

#[test]
fn function_call_output_item_becomes_tool_message() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "function_call_output", "call_id": "call_1", "output": "72F and sunny"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::Tool));
    assert_eq!(req.messages[0].tool_call_id.as_deref(), Some("call_1"));
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "72F and sunny"));
}

#[test]
fn function_call_output_item_with_array_output_collapses_text() {
    // Arrange: the output body may be an array of typed content items.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_text", "text": "result"}
            ]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "result"));
}

// ---------------------------------------------------------------------------
// reasoning item -> reasoning_details
// ---------------------------------------------------------------------------

#[test]
fn reasoning_item_maps_summary_to_reasoning_details() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "reasoning", "id": "rs_1",
             "summary": [{"type": "summary_text", "text": "step one"}],
             "encrypted_content": ""}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: details attach to the assistant turn with the v1 format tag.
    let details = &req.messages[0].reasoning_details;
    assert_eq!(details.len(), 1);
    assert!(matches!(details[0].kind, ReasoningDetailKind::Summary));
    assert_eq!(details[0].id.as_deref(), Some("rs_1"));
    assert_eq!(details[0].format.as_deref(), Some(OPENAI_RESPONSES_FORMAT));
    assert_eq!(details[0].payload["text"], "step one");
}

#[test]
fn reasoning_item_preserves_encrypted_content_signature() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": [
            {"type": "reasoning", "id": "rs_2",
             "summary": [],
             "content": [{"type": "reasoning_text", "text": "chain"}],
             "encrypted_content": "SIG-XYZ"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: text detail + encrypted-signature detail are both present.
    let details = &req.messages[0].reasoning_details;
    assert!(details
        .iter()
        .any(|d| matches!(d.kind, ReasoningDetailKind::Text) && d.payload["text"] == "chain"));
    let enc = details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .expect("encrypted detail present");
    assert_eq!(enc.payload["encrypted_content"], "SIG-XYZ");
}

#[test]
fn reasoning_item_with_inner_encrypted_content_block() {
    // Arrange: a reasoning_encrypted entry inside the content array.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "reasoning", "id": "rs_3", "summary": [],
             "content": [{"type": "reasoning_encrypted", "encrypted_content": "INNER"}],
             "encrypted_content": ""}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    let details = &req.messages[0].reasoning_details;
    let enc = details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Encrypted))
        .expect("encrypted detail present");
    assert_eq!(enc.payload["encrypted_content"], "INNER");
}

// ---------------------------------------------------------------------------
// tools + tool_choice
// ---------------------------------------------------------------------------

#[test]
fn flat_function_tool_becomes_custom_tooldef() {
    // Arrange: the Responses flat function shape (no nested `function`).
    let body = json!({
        "model": "m",
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "current weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            "strict": true
        }]
    });

    // Act
    let req = parse(body);

    // Assert
    let tools = req.tools.expect("tools present");
    assert_eq!(tools.len(), 1);
    match &tools[0] {
        ToolDef::Custom(c) => {
            assert_eq!(c.name, "get_weather");
            assert_eq!(c.description.as_deref(), Some("current weather"));
            assert_eq!(c.input_schema["properties"]["city"]["type"], "string");
            assert_eq!(c.strict, Some(true));
        }
        other => panic!("expected ToolDef::Custom, got {other:?}"),
    }
}

#[test]
fn unknown_tool_shape_passes_through_as_other() {
    // Arrange: a builtin / unknown tool kind must not be coerced.
    let body = json!({
        "model": "m",
        "input": "hi",
        "tools": [{"type": "web_search_preview"}]
    });

    // Act
    let req = parse(body);

    // Assert
    let tools = req.tools.expect("tools present");
    assert!(matches!(&tools[0], ToolDef::Other(_)));
}

#[test]
fn tool_choice_string_passes_through_verbatim() {
    for tc in ["auto", "required", "none"] {
        // Arrange
        let body = json!({ "model": "m", "input": "hi", "tool_choice": tc });

        // Act
        let req = parse(body);

        // Assert: ingress is shape-agnostic; egress translates per-upstream.
        assert_eq!(req.tool_choice, Some(json!(tc)));
    }
}

#[test]
fn tool_choice_named_function_object_passes_through_verbatim() {
    // Arrange
    let tc = json!({"type": "function", "name": "get_weather"});
    let body = json!({ "model": "m", "input": "hi", "tool_choice": tc.clone() });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.tool_choice, Some(tc));
}

// ---------------------------------------------------------------------------
// reasoning effort + max_output_tokens + text.format
// ---------------------------------------------------------------------------

#[test]
fn reasoning_object_maps_effort_to_reasoning_config() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"effort": "high", "summary": "auto"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.reasoning.unwrap().effort.as_deref(), Some("high"));
}

#[test]
fn reasoning_object_without_effort_yields_no_config() {
    // Arrange: a summary-only reasoning object carries nothing canonical
    // models, so reasoning stays None.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"summary": "auto"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert!(req.reasoning.is_none());
}

#[test]
fn max_output_tokens_maps_to_max_tokens() {
    // Arrange
    let body = json!({ "model": "m", "input": "hi", "max_output_tokens": 4096 });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.max_tokens, Some(4096));
}

#[test]
fn text_format_maps_to_response_format() {
    // Arrange
    let format = json!({"type": "json_schema", "name": "out", "schema": {"type": "object"}});
    let body = json!({
        "model": "m",
        "input": "hi",
        "text": {"format": format.clone()}
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.response_format, Some(format));
}

#[test]
fn instructions_and_in_array_system_item_both_preserved_in_system() {
    // Arrange: both the top-level `instructions` field and an in-array
    // system message carry content. The lift must merge both into
    // req.system so neither is silently dropped.
    let body = json!({
        "model": "m",
        "instructions": "from instructions",
        "input": [
            {"type": "message", "role": "system", "content": "from in-array system"},
            {"type": "message", "role": "user", "content": "hello"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: both texts survive concatenated in req.system.
    match &req.system {
        Some(SystemContent::Text(s)) => {
            assert!(
                s.contains("from instructions"),
                "instructions text missing from system: {s:?}"
            );
            assert!(
                s.contains("from in-array system"),
                "in-array system text missing from system: {s:?}"
            );
        }
        other => panic!("expected SystemContent::Text, got {other:?}"),
    }
    // The system item is removed from the messages array; only the user
    // message remains.
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::User));
}

#[test]
fn instructions_and_in_array_developer_item_both_preserved_in_system() {
    // Arrange: same as above but with role "developer" (the o-series /
    // Responses privileged-instruction variant of "system").
    let body = json!({
        "model": "m",
        "instructions": "from instructions",
        "input": [
            {"type": "message", "role": "developer", "content": "from developer"},
            {"type": "message", "role": "user", "content": "hello"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert
    match &req.system {
        Some(SystemContent::Text(s)) => {
            assert!(
                s.contains("from instructions"),
                "instructions missing: {s:?}"
            );
            assert!(
                s.contains("from developer"),
                "developer text missing: {s:?}"
            );
        }
        other => panic!("expected SystemContent::Text, got {other:?}"),
    }
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::User));
}

#[test]
fn text_verbosity_survives_into_provider_extras() {
    // Arrange: a `text` object carrying both `format` (which is lifted
    // into response_format) and a sibling field `verbosity`. The sibling
    // must survive into provider_extras.text for forward-compat, since
    // "text" is a handled top-level field and the extras sweep never sees
    // its contents.
    let format = json!({"type": "text"});
    let body = json!({
        "model": "m",
        "input": "hi",
        "text": {
            "format": format.clone(),
            "verbosity": "detailed"
        }
    });

    // Act
    let req = parse(body);

    // Assert: format lifted into response_format.
    assert_eq!(req.response_format.as_ref(), Some(&format));

    // Assert: verbosity survives in provider_extras["text"].
    let extras = req
        .provider_extras
        .as_ref()
        .expect("provider_extras present");
    let text_extras = extras
        .get("text")
        .expect("text key present in provider_extras");
    assert_eq!(
        text_extras["verbosity"], "detailed",
        "verbosity must survive in provider_extras.text: {text_extras:?}"
    );
    // format must NOT be re-emitted in extras (it was lifted).
    assert!(
        text_extras.get("format").is_none(),
        "format must not appear in provider_extras.text: {text_extras:?}"
    );
}

// ---------------------------------------------------------------------------
// scalar passthroughs
// ---------------------------------------------------------------------------

#[test]
fn maps_stream_temperature_top_p() {
    // Arrange
    let body = json!({
        "model": "m", "input": "hi",
        "stream": true, "temperature": 0.3, "top_p": 0.9
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.stream, Some(true));
    assert_eq!(req.temperature, Some(0.3));
    assert_eq!(req.top_p, Some(0.9));
}

// ---------------------------------------------------------------------------
// provider_extras forward-compat sweep
// ---------------------------------------------------------------------------

#[test]
fn unknown_top_level_field_swept_into_provider_extras() {
    // Arrange: a long-tail Responses knob this ingress does not model.
    let body = json!({
        "model": "m",
        "input": "hi",
        "prompt_cache_key": "abc123",
        "service_tier": "flex"
    });

    // Act
    let req = parse(body);

    // Assert: nothing is silently dropped.
    let extras = req.provider_extras.expect("provider_extras present");
    assert_eq!(extras["prompt_cache_key"], "abc123");
    assert_eq!(extras["service_tier"], "flex");
}

#[test]
fn handled_fields_do_not_leak_into_provider_extras() {
    // Arrange
    let body = json!({
        "model": "m",
        "instructions": "sys",
        "input": "hi",
        "max_output_tokens": 10,
        "reasoning": {"effort": "low"}
    });

    // Act
    let req = parse(body);

    // Assert: a body with only handled fields produces no extras.
    assert!(
        req.provider_extras.is_none(),
        "no provider_extras expected, got {:?}",
        req.provider_extras
    );
}

// ---------------------------------------------------------------------------
// graceful degradation (Acceptance C3)
// ---------------------------------------------------------------------------

#[test]
fn unknown_input_item_kind_skipped_without_error() {
    // Arrange: a bogus item kind sits between two valid items.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": "before"},
            {"type": "totally_made_up_item", "weird": "payload"},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "after"}]}
        ]
    });

    // Act: must not error, must not panic.
    let req = parse(body);

    // Assert: the two known items parse; the unknown one is dropped.
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "before"));
    assert!(matches!(&req.messages[1].content, MessageContent::Text(t) if t == "after"));
}

#[test]
fn non_object_body_returns_validation_error() {
    // Arrange
    let body = json!("not an object");

    // Act
    let err = ResponsesIngress
        .parse_request(&HeaderMap::new(), body)
        .unwrap_err();

    // Assert
    assert!(matches!(err, Error::Validation(_)));
}

// ---------------------------------------------------------------------------
// statefulness contract (Acceptance C4)
// ---------------------------------------------------------------------------

#[test]
fn previous_response_id_present_returns_400_validation_error() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": "continue",
        "previous_response_id": "resp_abc"
    });

    // Act
    let err = ResponsesIngress
        .parse_request(&HeaderMap::new(), body)
        .unwrap_err();

    // Assert: a stateless proxy must reject server-side state rather than
    // answer with the wrong context. Error::Validation maps to 400.
    match err {
        Error::Validation(msg) => {
            assert!(
                msg.contains("previous_response_id"),
                "message should name the rejected field: {msg}"
            );
        }
        other => panic!("expected Error::Validation, got {other:?}"),
    }
}

#[test]
fn previous_response_id_null_is_accepted() {
    // Arrange: an explicit null is the "not set" shape and must not 400.
    let body = json!({
        "model": "m",
        "input": "hi",
        "previous_response_id": null
    });

    // Act / Assert: parses cleanly.
    let req = parse(body);
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn store_true_is_accepted_and_ignored() {
    // Arrange: store=true without previous_response_id is self-contained.
    let body = json!({ "model": "m", "input": "hi", "store": true });

    // Act
    let req = parse(body);

    // Assert: request parses, and store is not forwarded to extras.
    assert_eq!(req.messages.len(), 1);
    let store_leaked = req
        .provider_extras
        .as_ref()
        .and_then(|v| v.get("store"))
        .is_some();
    assert!(!store_leaked, "store must not leak into provider_extras");
}

#[test]
fn store_false_is_normal_stateless_path() {
    // Arrange
    let body = json!({ "model": "m", "input": "hi", "store": false });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 1);
    assert!(req.provider_extras.is_none());
}

#[test]
fn store_absent_is_normal_stateless_path() {
    // Arrange
    let body = json!({ "model": "m", "input": "hi" });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn store_true_with_previous_response_id_returns_400_not_warn() {
    // Arrange: the two flags together must still hard-400 on
    // previous_response_id (the reject runs before the store warn), never
    // a store warn + silent wrong answer.
    let body = json!({
        "model": "m",
        "input": "hi",
        "previous_response_id": "resp_x",
        "store": true
    });

    // Act
    let err = ResponsesIngress
        .parse_request(&HeaderMap::new(), body)
        .unwrap_err();

    // Assert
    assert!(matches!(err, Error::Validation(_)));
}

#[test]
fn reasoning_item_opens_assistant_turn_when_none_trailing() {
    // Arrange: a reasoning item with no preceding assistant message must
    // open a fresh assistant turn to carry its details (same open-turn
    // logic as function_call; pin it independently).
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "reasoning", "id": "rs_1",
             "summary": [{"type": "summary_text", "text": "thinking"}]}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: a fresh assistant turn carries the reasoning details.
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(req.messages[1].role, Role::Assistant));
    assert!(!req.messages[1].reasoning_details.is_empty());
}

#[test]
fn openai_responses_format_constant_matches_egress_spelling() {
    // The ingress redefines this tag rather than importing the egress's
    // pub(crate) copy (hub-and-spoke forbids the cross-crate import). The
    // two must stay byte-identical or reasoning replay silently breaks at
    // the integration boundary. Machine-check the documented invariant;
    // update BOTH constants together if the format tag ever changes.
    assert_eq!(OPENAI_RESPONSES_FORMAT, "openai-responses-v1");
}

// ---------------------------------------------------------------------------
// stub surfaces (SLICE 3 not yet implemented)
// ---------------------------------------------------------------------------

#[test]
fn render_response_emits_response_envelope() {
    // SLICE 2 implements the non-stream renderer: a default
    // ChatResponse renders a well-formed Responses `response` object
    // (object/status/output present) rather than erroring.
    // Arrange
    let resp = ChatResponse::default();

    // Act
    let v = ResponsesIngress.render_response(resp).unwrap();

    // Assert
    assert_eq!(v["object"], "response");
    assert_eq!(v["status"], "completed");
    assert!(v["output"].is_array());
}

#[test]
fn render_chunk_emits_response_created_on_first_chunk() {
    // SLICE 3 implements the streaming render path: the first chunk opens
    // the stream with a response.created event rather than erroring. Full
    // lifecycle coverage lives in stream_tests.rs.
    // Arrange
    let chunk = ChatChunk::default();
    let mut state = ResponsesIngress.new_stream_state();

    // Act
    let events = ResponsesIngress
        .render_chunk(chunk, state.as_mut())
        .expect("render_chunk should succeed");

    // Assert
    assert!(!events.is_empty());
    assert_eq!(events[0].event.as_deref(), Some("response.created"));
}
