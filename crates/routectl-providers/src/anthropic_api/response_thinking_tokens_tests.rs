//! `usage.output_tokens_details.thinking_tokens` on a buffered Messages
//! response, lifted onto the canonical `Usage.reasoning_tokens`.
//!
//! The thinking fixture's usage block is the one a live
//! claude-sonnet-4-5 response with thinking enabled returned.

use super::normalize;
use serde_json::{Value, json};

fn response_with_usage(usage: Value) -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "text", "text": "391"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage
    })
}

fn thinking_usage() -> Value {
    json!({
        "input_tokens": 69,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0},
        "output_tokens": 124,
        "output_tokens_details": {"thinking_tokens": 118},
        "service_tier": "standard"
    })
}

#[test]
fn nested_thinking_tokens_populate_reasoning_tokens() {
    // Act
    let usage = normalize("test", response_with_usage(thinking_usage()))
        .unwrap()
        .usage
        .expect("usage present");

    // Assert
    assert_eq!(usage.reasoning_tokens, Some(118));
    assert_eq!(
        usage.completion_tokens, 124,
        "thinking tokens are a subset of output_tokens, never added to it"
    );
    assert_eq!(
        usage.extras.get("output_tokens_details"),
        Some(&json!({"thinking_tokens": 118})),
        "the wire object must still ride extras for a verbatim Anthropic render"
    );
}

#[test]
fn a_response_without_thinking_details_reports_no_reasoning_tokens() {
    // Act
    let usage = normalize(
        "test",
        response_with_usage(json!({"input_tokens": 36, "output_tokens": 5})),
    )
    .unwrap()
    .usage
    .expect("usage present");

    // Assert
    assert_eq!(usage.reasoning_tokens, None);
}

#[test]
fn a_wire_stated_zero_is_reported_as_zero() {
    // Act
    let usage = normalize(
        "test",
        response_with_usage(json!({
            "input_tokens": 36,
            "output_tokens": 5,
            "output_tokens_details": {"thinking_tokens": 0}
        })),
    )
    .unwrap()
    .usage
    .expect("usage present");

    // Assert
    assert_eq!(usage.reasoning_tokens, Some(0));
}

/// Bedrock Invoke speaks the Messages wire and parses through the same
/// normalizer, so it gets the same lift.
#[cfg(feature = "bedrock")]
#[test]
fn bedrock_invoke_lifts_the_same_nested_field() {
    // Act
    let usage =
        crate::bedrock::invoke::normalize_response("test", response_with_usage(thinking_usage()))
            .unwrap()
            .usage
            .expect("usage present");

    // Assert
    assert_eq!(usage.reasoning_tokens, Some(118));
}
