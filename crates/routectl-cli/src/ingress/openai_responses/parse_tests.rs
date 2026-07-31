//! Behavior tests for the OpenAI Responses ingress request parser.
//!
//! Mirrors the egress request_tests density: each Responses input-item
//! kind and top-level field has a behavior-named AAA test, plus the
//! three statefulness branches and the graceful-degradation path.

use super::*;
use axum::http::HeaderMap;
use axum::http::header::HeaderName;
use routectl_core::{
    ContentPart, Error, KnownContentPart, MessageContent, OPENAI_RESPONSES_V1, ReasoningDetailKind,
    Role, SystemContent, ToolDef, is_responses_family,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse(body: serde_json::Value) -> ChatRequest {
    ResponsesIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn parse_with_headers(headers: &HeaderMap, body: serde_json::Value) -> ChatRequest {
    ResponsesIngress
        .parse_request_value(headers, body)
        .expect("request should parse")
}

fn parse_err(body: serde_json::Value) -> Error {
    ResponsesIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect_err("request should be rejected")
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
    assert_eq!(details[0].format.as_deref(), Some(OPENAI_RESPONSES_V1));
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
    assert!(
        details
            .iter()
            .any(|d| matches!(d.kind, ReasoningDetailKind::Text) && d.payload["text"] == "chain")
    );
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
fn tool_choice_flat_named_function_normalizes_to_nested() {
    // Arrange -- the Responses wire forces a named tool with the flat
    // {type:function, name:X} shape.
    let body = json!({
        "model": "m",
        "input": "hi",
        "tool_choice": {"type": "function", "name": "get_weather"},
    });

    // Act
    let req = parse(body);

    // Assert -- ingress normalizes to the nested OpenAI form both egress
    // mappers already consume.
    assert_eq!(
        req.tool_choice,
        Some(json!({"type": "function", "function": {"name": "get_weather"}}))
    );
}

#[test]
fn tool_choice_nested_named_function_passes_through_verbatim() {
    // Arrange -- an already-nested named forcing choice.
    let tc = json!({"type": "function", "function": {"name": "get_weather"}});
    let body = json!({ "model": "m", "input": "hi", "tool_choice": tc });

    // Act
    let req = parse(body);

    // Assert -- normalization is a no-op on the canonical shape.
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
fn reasoning_summary_only_carries_to_provider_extras() {
    // Arrange: a summary-only reasoning object carries no effort, so the
    // canonical ReasoningConfig stays None -- but the summary must survive
    // under provider_extras["reasoning"] for the Responses egress to re-emit
    // (the historical drop was the confirmed bug).
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"summary": "auto"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert!(req.reasoning.is_none());
    assert_eq!(
        req.provider_extras.unwrap()["reasoning"],
        json!({"summary": "auto"})
    );
}

#[test]
fn reasoning_effort_lifts_and_summary_carries_together() {
    // Arrange: effort lifts into canonical config; the summary rides along
    // in the provider_extras remainder (effort is stripped out of it).
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"effort": "high", "summary": "concise"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(
        req.reasoning.as_ref().unwrap().effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        req.provider_extras.unwrap()["reasoning"],
        json!({"summary": "concise"})
    );
}

#[test]
fn reasoning_context_and_mode_carry_to_provider_extras() {
    // Arrange: context (closed enum) and mode (open string) both survive
    // the ingress under the reasoning remainder.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"effort": "medium", "context": "all_turns", "mode": "pro"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(
        req.reasoning.as_ref().unwrap().effort.as_deref(),
        Some("medium")
    );
    assert_eq!(
        req.provider_extras.unwrap()["reasoning"],
        json!({"context": "all_turns", "mode": "pro"})
    );
}

#[test]
fn reasoning_arbitrary_mode_string_passes_through_unvalidated() {
    // Arrange: mode is an intentionally open enum on the Responses schema;
    // an unrecognized value must pass through, not 400.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"mode": "some-future-mode"}
    });

    // Act
    let req = parse(body);

    // Assert
    assert_eq!(
        req.provider_extras.unwrap()["reasoning"],
        json!({"mode": "some-future-mode"})
    );
}

#[test]
fn reasoning_non_string_mode_bool_is_rejected() {
    // Arrange: mode's value is open, but its TYPE must be a string. A bool
    // would forward to a guaranteed upstream 400, so reject it locally.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"mode": false}
    });

    // Act
    let err = parse_err(body);

    // Assert
    assert!(
        matches!(err, Error::Validation(_)),
        "expected local 400 for a non-string mode, got {err:?}"
    );
}

#[test]
fn reasoning_non_string_mode_number_is_rejected() {
    // Arrange: a numeric mode is likewise a type error, not an open-enum
    // value.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"mode": 123}
    });

    // Act
    let err = parse_err(body);

    // Assert
    assert!(
        matches!(err, Error::Validation(_)),
        "expected local 400 for a numeric mode, got {err:?}"
    );
}

#[test]
fn reasoning_null_summary_is_treated_as_unset() {
    // Arrange: a null summary means "unset" -- it must not carry to the
    // egress (where it would block the "auto" default) nor 400.
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"effort": "high", "summary": null, "context": "all_turns"}
    });

    // Act
    let req = parse(body);

    // Assert: effort lifts; only the non-null context remains.
    assert_eq!(
        req.reasoning.as_ref().unwrap().effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        req.provider_extras.unwrap()["reasoning"],
        json!({"context": "all_turns"})
    );
}

#[test]
fn reasoning_invalid_summary_value_is_rejected() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"summary": "verbose"}
    });

    // Act
    let err = parse_err(body);

    // Assert: local 400, not clamped or forwarded.
    assert!(
        matches!(err, Error::Validation(_)),
        "expected 400, got {err:?}"
    );
}

#[test]
fn reasoning_invalid_context_value_is_rejected() {
    // Arrange
    let body = json!({
        "model": "m",
        "input": "hi",
        "reasoning": {"context": "everything"}
    });

    // Act
    let err = parse_err(body);

    // Assert
    assert!(
        matches!(err, Error::Validation(_)),
        "expected 400, got {err:?}"
    );
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
        "text": {"format": format}
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
            "format": format,
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
fn unknown_input_item_kind_preserved_without_error() {
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

    // Assert: the two known items parse as messages; the unmodeled one is
    // captured verbatim for a Responses egress to replay (not dropped).
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "before"));
    assert!(matches!(&req.messages[1].content, MessageContent::Text(t) if t == "after"));
    let passthrough = &req.routectl_internal.responses_input_passthrough;
    assert_eq!(passthrough.len(), 1, "unmodeled item must be preserved");
    assert_eq!(passthrough[0].item["type"], "totally_made_up_item");
    assert_eq!(passthrough[0].item["weird"], "payload");
    // One modeled message preceded it inbound, so its splice index is 1.
    assert_eq!(passthrough[0].modeled_prefix, 1);
}

#[test]
fn non_object_body_returns_validation_error() {
    // Arrange
    let body = json!("not an object");

    // Act
    let err = ResponsesIngress
        .parse_request_value(&HeaderMap::new(), body)
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
        .parse_request_value(&HeaderMap::new(), body)
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

// ---------------------------------------------------------------------------
// unmodeled input-item passthrough (ingress -> Responses egress round-trip)
// ---------------------------------------------------------------------------

/// Drive a canonical request through the Responses EGRESS and return the
/// serialized wire body. Same public entry the contract egress test uses
/// (`Provider::normalize_request`), so this exercises the real
/// ingress -> canonical -> egress path end to end.
fn responses_egress_body(req: &ChatRequest) -> serde_json::Value {
    use routectl_core::Provider;
    use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};

    OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
        "openai-responses:test",
        "literal:test",
    ))
    .normalize_request(req)
    .expect("responses egress normalize")
}

#[test]
fn unmodeled_input_item_kinds_round_trip_through_responses_egress() {
    // Arrange: a codex-shaped Responses request interleaving a modeled
    // message with the codex-native item kinds this hub does not model.
    let body = json!({
        "model": "gpt-5-codex",
        "input": [
            {"type": "message", "role": "user", "content": "run it"},
            {"type": "local_shell_call", "id": "lsc_1",
             "action": {"type": "exec", "command": ["ls", "-la"]}},
            {"type": "custom_tool_call", "call_id": "ctc_1",
             "name": "grep", "input": "needle"},
            {"type": "custom_tool_call_output", "call_id": "ctc_1",
             "output": "haystack"},
            {"type": "tool_search_call", "id": "tsc_1", "query": "docs"},
            {"type": "agent_message", "id": "am_1", "content": "internal note"}
        ]
    });

    // Act: ingress -> canonical -> egress wire body.
    let req = parse(body);
    let out = responses_egress_body(&req);

    // Assert: every unmodeled kind survives verbatim in the egress input[].
    let input = out["input"].as_array().expect("egress input array");
    let kinds: Vec<&str> = input
        .iter()
        .filter_map(|i| i.get("type").and_then(serde_json::Value::as_str))
        .collect();
    for kind in [
        "local_shell_call",
        "custom_tool_call",
        "custom_tool_call_output",
        "tool_search_call",
        "agent_message",
    ] {
        assert!(
            kinds.contains(&kind),
            "kind {kind} must round-trip through the Responses egress; got {kinds:?}"
        );
    }

    // The modeled user turn is still emitted (known kinds unaffected).
    assert!(
        kinds.contains(&"message"),
        "the modeled user message must still be emitted; got {kinds:?}"
    );

    // A preserved item's inner payload is forwarded byte-for-byte.
    let lsc = input
        .iter()
        .find(|i| i["type"] == "local_shell_call")
        .expect("local_shell_call present");
    assert_eq!(lsc["id"], "lsc_1");
    assert_eq!(lsc["action"]["command"][0], "ls");
    assert_eq!(lsc["action"]["command"][1], "-la");
}

#[test]
fn preserved_items_keep_relative_order_on_egress() {
    // Arrange: two unmodeled kinds in a fixed order.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "custom_tool_call", "call_id": "c1", "name": "a", "input": "x"},
            {"type": "custom_tool_call_output", "call_id": "c1", "output": "y"}
        ]
    });

    // Act
    let req = parse(body);
    let out = responses_egress_body(&req);

    // Assert: the call precedes its output on the wire.
    let input = out["input"].as_array().expect("egress input array");
    let call_idx = input
        .iter()
        .position(|i| i["type"] == "custom_tool_call")
        .expect("custom_tool_call present");
    let out_idx = input
        .iter()
        .position(|i| i["type"] == "custom_tool_call_output")
        .expect("custom_tool_call_output present");
    assert!(
        call_idx < out_idx,
        "preserved items must keep inbound relative order"
    );
}

#[test]
fn known_kinds_produce_no_passthrough() {
    // Arrange: only modeled kinds -- nothing to preserve.
    let body = json!({
        "model": "m",
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]
    });

    // Act
    let req = parse(body);

    // Assert: no items diverted to passthrough.
    assert!(
        req.routectl_internal.responses_input_passthrough.is_empty(),
        "modeled kinds must not populate the passthrough carrier"
    );
}

// ---------------------------------------------------------------------------
// reasoning sub-key fidelity (ingress -> Responses egress wire)
//
// These drive each accepted summary/context value, a mode-only request, and
// a full effort+summary+context+mode request THROUGH the ingress into the
// canonical request and then out the real Responses egress, asserting the
// emitted wire `reasoning` object -- not by hand-constructing
// provider_extras. This closes the passthrough loop end to end.
// ---------------------------------------------------------------------------

/// Drive a Responses `reasoning` object through the ingress and back out the
/// Responses egress, returning the emitted wire `reasoning` object.
fn ingress_to_egress_reasoning(reasoning: serde_json::Value) -> serde_json::Value {
    let body = json!({ "model": "gpt-5-codex", "input": "hi", "reasoning": reasoning });
    let req = parse(body);
    let out = responses_egress_body(&req);
    out["reasoning"].clone()
}

#[test]
fn each_accepted_summary_value_reaches_the_wire_from_ingress() {
    for summary in ["auto", "concise", "detailed"] {
        // Act
        let wire = ingress_to_egress_reasoning(json!({"effort": "high", "summary": summary}));

        // Assert: the caller-set summary survives verbatim onto the wire.
        assert_eq!(
            wire,
            json!({"effort": "high", "summary": summary}),
            "summary {summary} must reach the wire from ingress; got: {wire}"
        );
    }
}

#[test]
fn each_accepted_context_value_reaches_the_wire_from_ingress() {
    for context in ["auto", "current_turn", "all_turns"] {
        // Act
        let wire = ingress_to_egress_reasoning(json!({"effort": "high", "context": context}));

        // Assert: context rides onto the wire alongside the defaulted summary.
        assert_eq!(
            wire,
            json!({"effort": "high", "summary": "auto", "context": context}),
            "context {context} must reach the wire from ingress; got: {wire}"
        );
    }
}

#[test]
fn mode_only_request_still_emits_reasoning_object_on_the_wire() {
    // Arrange/Act: a mode-only reasoning object (no effort/summary/context)
    // driven ingress -> egress.
    let wire = ingress_to_egress_reasoning(json!({"mode": "pro"}));

    // Assert: a reasoning object is emitted carrying the mode (summary
    // defaults to auto since the caller set none).
    assert_eq!(
        wire,
        json!({"summary": "auto", "mode": "pro"}),
        "a mode-only request must still emit a reasoning object; got: {wire}"
    );
}

#[test]
fn full_reasoning_request_round_trips_ingress_to_egress_byte_for_byte() {
    // Arrange/Act: a full reasoning request exercises every knob at once --
    // effort lifts into canonical, summary/context/mode ride the remainder,
    // and the egress must reassemble the exact wire object.
    let wire = ingress_to_egress_reasoning(json!({
        "effort": "high",
        "summary": "detailed",
        "context": "all_turns",
        "mode": "pro"
    }));

    // Assert: the emitted wire object matches the inbound shape byte-for-byte.
    assert_eq!(
        wire,
        json!({
            "effort": "high",
            "summary": "detailed",
            "context": "all_turns",
            "mode": "pro"
        }),
        "the full reasoning request must round-trip byte-for-byte; got: {wire}"
    );
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
        .parse_request_value(&HeaderMap::new(), body)
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
fn stamped_ingress_tag_is_recognized_by_the_family_predicate() {
    // The ingress stamps inbound reasoning items with the compatibility
    // tag; every downstream reader gates on the family predicate, so the
    // stamp must land inside the family or reasoning replay breaks.
    assert!(is_responses_family(Some(OPENAI_RESPONSES_V1)));
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
    let v = ResponsesIngress.render_response_value(resp).unwrap();

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
    let mut state = ResponsesIngress.new_stream_state(&StreamRequestContext::default());

    // Act
    let events = ResponsesIngress
        .render_chunk(chunk, state.as_mut())
        .expect("render_chunk should succeed");

    // Assert
    assert!(!events.is_empty());
    assert_eq!(events[0].event.as_deref(), Some("response.created"));
}
