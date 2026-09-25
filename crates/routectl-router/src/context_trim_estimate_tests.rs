//! Golden pins for the whole-request token estimates: the persisted
//! floor [`estimate_total_tokens`] and the client-facing display
//! wrapper [`estimate_meter_tokens`].
//!
//! Each golden case pins BOTH the exact serialized request bytes and the
//! resulting floor, so a serialization drift and an arithmetic drift fail
//! separately. The literals are the values the floor produced before the
//! display wrapper existed; they key persisted `calib_estimated_tokens` and
//! `would_trim_*` evidence and must not move.

use std::sync::Arc;

use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::schema::{ChatRequest, Message, MessageContent, Role};
use routectl_core::{CustomTool, SystemContent, ToolDef};
use serde_json::json;

use super::{
    RequestEstimate, estimate_meter_tokens, estimate_request, estimate_total_tokens,
    meter_tokens_from_bytes,
};

fn message(role: Role, content: MessageContent) -> Message {
    Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: Arc::from(messages),
        ..Default::default()
    }
}

fn text_request() -> ChatRequest {
    request(vec![message(
        Role::User,
        MessageContent::Text("hello there".into()),
    )])
}

fn tools_request() -> ChatRequest {
    ChatRequest {
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "get_weather".into(),
            description: Some("Look up weather".into()),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..text_request()
    }
}

fn image_request() -> ChatRequest {
    request(vec![message(
        Role::User,
        MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({"type": "base64", "media_type": "image/png", "data": "AAAA"}),
            cache_control: None,
        })]),
    )])
}

fn tool_result_request() -> ChatRequest {
    request(vec![message(
        Role::User,
        MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: json!("result body"),
            is_error: None,
            cache_control: None,
        })]),
    )])
}

fn non_ascii_request() -> ChatRequest {
    ChatRequest {
        system: Some(SystemContent::Text("\u{6f22}\u{5b57}".into())),
        ..request(vec![message(
            Role::User,
            MessageContent::Text("caf\u{e9} \u{1f600}".into()),
        )])
    }
}

/// Assert the golden shape of one case: exact serialized bytes, then the
/// floor, then the display wrapper agreeing with the floor.
fn assert_golden(req: &ChatRequest, expected_json: &str, expected_floor: u64) {
    let serialized = serde_json::to_string(req).expect("canonical request serializes");
    assert_eq!(serialized, expected_json, "serialized request drifted");
    assert_eq!(
        expected_json.len() as u64 / 4,
        expected_floor,
        "golden literal must equal the pinned byte length / 4"
    );
    assert_eq!(
        estimate_total_tokens(req),
        expected_floor,
        "persisted floor drifted for {expected_json}"
    );
    assert_eq!(
        estimate_meter_tokens(req),
        expected_floor,
        "display wrapper must equal the floor once the request is >= 4 bytes"
    );
    assert_eq!(
        estimate_request(req),
        RequestEstimate {
            total: expected_floor,
            meter: expected_floor,
        },
        "the one-pass pair must equal both estimators"
    );
}

#[test]
fn golden_default_request_counts_its_envelope() {
    // Arrange: no model, no messages -- only the always-present keys.
    let req = ChatRequest::default();

    // Act + Assert: the envelope alone is 26 bytes, so even an "empty"
    // request estimates a nonzero floor.
    assert_golden(&req, r#"{"model":"","messages":[]}"#, 6);
}

#[test]
fn golden_model_id_bytes_count_toward_the_estimate() {
    // Arrange: identical content, different model id length.
    let short = request(vec![]);
    let long = ChatRequest {
        model: "a-considerably-longer-model-identifier".into(),
        ..request(vec![])
    };

    // Act + Assert
    assert_golden(&short, r#"{"model":"m","messages":[]}"#, 6);
    assert_golden(
        &long,
        r#"{"model":"a-considerably-longer-model-identifier","messages":[]}"#,
        16,
    );
}

#[test]
fn golden_request_flags_count_toward_the_estimate() {
    // Arrange: generation flags ride in the serialized body.
    let req = ChatRequest {
        stream: Some(true),
        max_tokens: Some(1024),
        ..text_request()
    };

    // Act + Assert
    assert_golden(
        &req,
        r#"{"model":"m","messages":[{"role":"user","content":"hello there"}],"max_tokens":1024,"stream":true}"#,
        24,
    );
}

#[test]
fn golden_text_request() {
    assert_golden(
        &text_request(),
        r#"{"model":"m","messages":[{"role":"user","content":"hello there"}]}"#,
        16,
    );
}

#[test]
fn golden_tools_request() {
    assert_golden(
        &tools_request(),
        r#"{"model":"m","messages":[{"role":"user","content":"hello there"}],"tools":[{"name":"get_weather","description":"Look up weather","input_schema":{"type":"object"}}]}"#,
        41,
    );
}

#[test]
fn golden_image_request() {
    assert_golden(
        &image_request(),
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image","source":{"data":"AAAA","media_type":"image/png","type":"base64"}}]}]}"#,
        34,
    );
}

#[test]
fn golden_tool_result_request() {
    assert_golden(
        &tool_result_request(),
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"result body"}]}]}"#,
        31,
    );
}

#[test]
fn golden_non_ascii_request_counts_utf8_bytes_not_chars() {
    // Arrange
    let req = non_ascii_request();
    let serialized = serde_json::to_string(&req).expect("serializes");

    // Act + Assert: the fixture really is multibyte, and the floor follows
    // the byte length.
    assert!(
        serialized.len() > serialized.chars().count(),
        "fixture must be multibyte: {} bytes vs {} chars",
        serialized.len(),
        serialized.chars().count()
    );
    assert_golden(
        &req,
        "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"caf\u{e9} \u{1f600}\"}],\"system\":\"\u{6f22}\u{5b57}\"}",
        20,
    );
}

#[test]
fn meter_rule_is_zero_only_for_zero_bytes() {
    // Arrange + Act + Assert: below one full token, a nonzero byte count
    // shows one token rather than the floor's zero.
    assert_eq!(meter_tokens_from_bytes(0), 0);
    assert_eq!(meter_tokens_from_bytes(1), 1);
    assert_eq!(meter_tokens_from_bytes(3), 1);
}

#[test]
fn meter_rule_matches_the_floor_from_one_full_token_up() {
    // Arrange + Act + Assert: from 4 bytes on, the display value is the
    // floor quotient -- never rounded up.
    assert_eq!(meter_tokens_from_bytes(4), 1);
    assert_eq!(meter_tokens_from_bytes(7), 1);
    assert_eq!(meter_tokens_from_bytes(8), 2);
    assert_eq!(meter_tokens_from_bytes(4099), 1024);
}
