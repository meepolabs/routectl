//! A blank canonical `req.system` must never reach the Anthropic wire as
//! `system: ""`. Blank means an empty or whitespace-only `Text`, or `Blocks`
//! whose every text is blank. Blank reads as "no canonical system supplied"
//! (the same as `None`), so it falls through to the Role::System lift
//! instead of suppressing it.
//!
//! Pulled into request.rs via `#[path = ...] mod ...;` so the orchestrator
//! file stays under the project's line ceiling.

use super::*;
use routectl_core::{ChatRequest, Message, Role, SystemBlock, SystemContent};

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

fn system_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn req_with_system(system: Option<SystemContent>, messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: messages.into(),
        max_tokens: Some(64),
        system,
        ..Default::default()
    }
}

fn blank_block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

#[test]
fn empty_canonical_system_text_emits_no_system_field() {
    // Arrange
    let req = req_with_system(
        Some(SystemContent::Text(String::new())),
        vec![user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert
    assert!(
        body.get("system").is_none(),
        "an empty canonical system must not serialize a system field: {body}"
    );
}

#[test]
fn whitespace_only_canonical_system_text_emits_no_system_field() {
    // Arrange
    let req = req_with_system(
        Some(SystemContent::Text("   \n\t ".into())),
        vec![user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert
    assert!(body.get("system").is_none(), "{body}");
}

#[test]
fn all_blank_canonical_system_blocks_emit_no_system_field() {
    // Arrange
    let req = req_with_system(
        Some(SystemContent::Blocks(vec![
            blank_block(""),
            blank_block("  \n"),
        ])),
        vec![user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert
    assert!(body.get("system").is_none(), "{body}");
}

#[test]
fn blank_canonical_system_falls_through_to_the_system_message_lift() {
    // Arrange: blank canonical system alongside a direct caller's system
    // message. Blank must not discard the message-array prompt.
    let req = req_with_system(
        Some(SystemContent::Text(String::new())),
        vec![system_msg("you are helpful"), user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert
    assert_eq!(
        body["system"], "you are helpful",
        "the lifted system message must survive a blank canonical system: {body}"
    );
}

#[test]
fn blank_block_beside_real_block_keeps_only_the_real_block() {
    // Arrange
    let req = req_with_system(
        Some(SystemContent::Blocks(vec![
            blank_block("  "),
            blank_block("be helpful"),
        ])),
        vec![user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert: a partially-blank Blocks system is NOT blank, so it is
    // forwarded verbatim (per-block cache_control/citations preservation is
    // the point of the Blocks shape).
    let blocks = body["system"].as_array().expect("blocks shape preserved");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1]["text"], "be helpful");
}

#[test]
fn non_blank_canonical_system_still_reaches_the_wire() {
    // Arrange
    let req = req_with_system(
        Some(SystemContent::Text("you are helpful".into())),
        vec![user_msg("hi")],
    );

    // Act
    let body = normalize("p", &req, false, &[], false, None).unwrap();

    // Assert
    assert_eq!(body["system"], "you are helpful");
}
