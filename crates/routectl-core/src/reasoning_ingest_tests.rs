use serde_json::json;

use super::*;
use crate::schema::{MessageContent, ReasoningDetail, Role};

fn request_with_details(details: Vec<ReasoningDetail>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: std::sync::Arc::from(vec![assistant_with_details(details)]),
        ..ChatRequest::default()
    }
}

fn assistant_with_details(details: Vec<ReasoningDetail>) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text("answer".into()),
        reasoning: None,
        reasoning_details: details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn detail(kind: ReasoningDetailKind, payload: serde_json::Value) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: None,
        format: Some("anthropic-claude-v1".into()),
        index: Some(0),
        payload,
    }
}

#[test]
fn anthropic_thinking_payload_key_moves_to_canonical_text() {
    // Arrange: a Text detail spelled the Anthropic way.
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Text,
        json!({"thinking": "the trace", "signature": "sig"}),
    )]);

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert: content readable under `text`, signature untouched.
    let payload = &req.messages[0].reasoning_details[0].payload;
    assert_eq!(payload["text"], "the trace");
    assert_eq!(payload["signature"], "sig");
    assert!(
        payload.get("thinking").is_none(),
        "the Anthropic spelling is consumed, not duplicated"
    );
}

#[test]
fn explicit_canonical_text_wins_over_anthropic_spelling() {
    // Arrange: both keys present -- a client sending canonical AND
    // Anthropic vocabulary is taken at its canonical word.
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Text,
        json!({"text": "canonical", "thinking": "anthropic"}),
    )]);

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert
    assert_eq!(
        req.messages[0].reasoning_details[0].payload["text"],
        "canonical"
    );
}

#[test]
fn encrypted_detail_payload_is_left_untouched() {
    // Arrange: the Anthropic `data` spelling is read directly downstream,
    // so normalization must not rewrite it.
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Encrypted,
        json!({"data": "opaque-blob"}),
    )]);

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert
    let payload = &req.messages[0].reasoning_details[0].payload;
    assert_eq!(payload["data"], "opaque-blob");
    assert!(payload.get("text").is_none());
}

#[test]
fn summary_detail_payload_is_left_untouched() {
    // Arrange
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Summary,
        json!({"text": "a summary"}),
    )]);

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert
    assert_eq!(
        req.messages[0].reasoning_details[0].payload["text"],
        "a summary"
    );
}

#[test]
fn redacted_thinking_discriminator_deserializes_as_encrypted() {
    // Arrange + Act: the Anthropic block name on the wire.
    let d: ReasoningDetail =
        serde_json::from_value(json!({"type": "redacted_thinking", "data": "blob"}))
            .expect("redacted_thinking must deserialize");

    // Assert
    assert!(matches!(d.kind, ReasoningDetailKind::Encrypted));
    assert_eq!(d.payload["data"], "blob");
}

#[test]
fn thinking_discriminator_deserializes_as_text() {
    // Arrange + Act
    let d: ReasoningDetail =
        serde_json::from_value(json!({"type": "thinking", "thinking": "t", "signature": "s"}))
            .expect("thinking must deserialize");

    // Assert
    assert!(matches!(d.kind, ReasoningDetailKind::Text));
}

#[test]
fn canonical_spellings_still_deserialize_unchanged() {
    // Arrange + Act + Assert: the aliases widen, never replace.
    for (wire, expect_encrypted) in [
        ("reasoning.summary", false),
        ("reasoning.encrypted", true),
        ("reasoning.text", false),
    ] {
        let d: ReasoningDetail = serde_json::from_value(json!({"type": wire}))
            .unwrap_or_else(|e| panic!("{wire} must deserialize: {e}"));
        assert_eq!(
            matches!(d.kind, ReasoningDetailKind::Encrypted),
            expect_encrypted,
            "{wire} mapped to the wrong variant"
        );
    }
}

#[test]
fn aliased_kinds_serialize_back_to_canonical_spelling() {
    // Arrange: inbound Anthropic vocabulary.
    let d: ReasoningDetail =
        serde_json::from_value(json!({"type": "redacted_thinking", "data": "blob"})).unwrap();

    // Act
    let back = serde_json::to_value(&d).unwrap();

    // Assert: outbound wire contract is unchanged by the alias.
    assert_eq!(back["type"], "reasoning.encrypted");
}

/// The CoW seam on `ChatRequest::messages` must not be paid by a request
/// that carries no Anthropic vocabulary -- which is nearly every request.
#[test]
fn request_needing_no_rewrite_does_not_break_the_message_buffer_sharing() {
    // Arrange: canonical vocabulary only, buffer shared with a clone.
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Text,
        json!({"text": "canonical"}),
    )]);
    let shared = req.clone();

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert: still the same allocation -- no make_mut copy happened.
    assert!(
        std::sync::Arc::ptr_eq(&req.messages, &shared.messages),
        "a no-op normalization must not copy the message buffer"
    );
}

/// When a rewrite IS due, the copy-on-write must leave other clones
/// pristine rather than mutating through a shared buffer.
#[test]
fn rewrite_copies_on_write_and_leaves_other_clones_untouched() {
    // Arrange
    let mut req = request_with_details(vec![detail(
        ReasoningDetailKind::Text,
        json!({"thinking": "the trace"}),
    )]);
    let shared = req.clone();

    // Act
    normalize_reasoning_detail_payloads(&mut req);

    // Assert
    assert_eq!(
        req.messages[0].reasoning_details[0].payload["text"],
        "the trace"
    );
    assert_eq!(
        shared.messages[0].reasoning_details[0].payload["thinking"], "the trace",
        "the pre-existing clone must be unaffected by the rewrite"
    );
}
