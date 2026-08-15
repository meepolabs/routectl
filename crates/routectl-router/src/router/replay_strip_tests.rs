//! Direct behavior coverage for the raw lane-parameterized reasoning-replay
//! strip: which artifacts each lane keeps or drops, and that everything else
//! about the request survives byte-for-byte so upstream prompt-cache affinity
//! is not disturbed.
//!
//! The raw strip is private to `replay_repair`, so these tests live beside it
//! rather than in `capability_strip`: production dispatch code can only reach
//! it through `strip_replay_artifacts_recalibrating`, which additionally
//! re-stamps the calibration estimate. The wrapper's own re-stamp contract is
//! covered in `replay_strip_calibration_tests`.

use super::strip_replay_artifacts;

use std::sync::Arc;

use routectl_core::{
    BEDROCK_MANTLE, CODEX_OAUTH, ChatRequest, Message, MessageContent, OPENAI_RESPONSES_V1,
    ReasoningDetail, ReasoningDetailKind, ReplayScheme, Role,
};
use serde_json::json;

fn detail(format: Option<&str>, id: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(id.to_string()),
        format: format.map(str::to_string),
        index: None,
        payload: json!({"encrypted_content": "rsn_opaque"}),
    }
}

fn text_detail(id: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some(id.to_string()),
        format: Some(CODEX_OAUTH.to_string()),
        index: None,
        payload: json!({"text": "step one", "signature": "sig"}),
    }
}

fn assistant_with(details: Vec<ReasoningDetail>) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text("answer".to_string()),
        reasoning: Some("legacy thinking text".to_string()),
        reasoning_details: details,
        refusal: None,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn detail_ids(req: &ChatRequest, message_index: usize) -> Vec<String> {
    req.messages[message_index]
        .reasoning_details
        .iter()
        .filter_map(|d| d.id.clone())
        .collect()
}

#[test]
fn reasoning_replay_strip_keeps_carry_details_and_drops_foreign_and_gray() {
    // Arrange -- a MIXED history: a portable codex artifact, a foreign
    // mantle artifact, an ambiguous compatibility-tagged one, and an
    // untagged one, plus legacy thinking TEXT.
    let mut req = ChatRequest {
        messages: Arc::from(vec![assistant_with(vec![
            text_detail("rs_carry_text"),
            detail(Some(CODEX_OAUTH), "rs_carry"),
            detail(Some(BEDROCK_MANTLE), "rs_foreign"),
            detail(Some(OPENAI_RESPONSES_V1), "rs_ambiguous"),
            detail(None, "rs_untagged"),
        ])]),
        ..Default::default()
    };

    // Act -- dispatching onto a codex-family lane.
    let stripped = strip_replay_artifacts(&mut req, ReplayScheme::Codex);

    // Assert -- only the proven-portable artifacts survive; the legacy
    // reasoning text is untouched.
    assert!(stripped);
    assert_eq!(detail_ids(&req, 0), vec!["rs_carry_text", "rs_carry"]);
    assert_eq!(
        req.messages[0].reasoning.as_deref(),
        Some("legacy thinking text")
    );
}

#[test]
fn reasoning_replay_strip_is_lane_directional() {
    // The same history judged against the mantle lane keeps the mantle
    // artifact and drops the codex one -- portability is per-lane.
    let base = ChatRequest {
        messages: Arc::from(vec![assistant_with(vec![
            detail(Some(CODEX_OAUTH), "rs_codex"),
            detail(Some(BEDROCK_MANTLE), "rs_mantle"),
        ])]),
        ..Default::default()
    };

    // Act
    let mut onto_mantle = base.clone();
    let mut onto_codex = base.clone();
    assert!(strip_replay_artifacts(
        &mut onto_mantle,
        ReplayScheme::Mantle
    ));
    assert!(strip_replay_artifacts(&mut onto_codex, ReplayScheme::Codex));

    // Assert
    assert_eq!(detail_ids(&onto_mantle, 0), vec!["rs_mantle"]);
    assert_eq!(detail_ids(&onto_codex, 0), vec!["rs_codex"]);
}

#[test]
fn reasoning_replay_strip_on_gray_lane_removes_every_artifact() {
    // An unestablished lane scheme proves nothing portable, so the
    // rejected-variant request carries no replay artifact at all.
    let mut req = ChatRequest {
        messages: Arc::from(vec![assistant_with(vec![
            detail(Some(CODEX_OAUTH), "rs_codex"),
            detail(Some(BEDROCK_MANTLE), "rs_mantle"),
        ])]),
        ..Default::default()
    };

    // Act
    let stripped = strip_replay_artifacts(&mut req, ReplayScheme::Gray);

    // Assert
    assert!(stripped);
    assert!(req.messages[0].reasoning_details.is_empty());
}

#[test]
fn reasoning_replay_strip_leaves_request_otherwise_byte_identical() {
    // Prompt-cache affinity: everything except the removed artifacts
    // -- message order, every other field, the surviving details'
    // order -- must serialize identically to a hand-built expectation.
    let mut req = ChatRequest {
        model: "m".to_string(),
        messages: Arc::from(vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("q".to_string()),
                reasoning: None,
                reasoning_details: vec![],
                refusal: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            assistant_with(vec![
                detail(Some(CODEX_OAUTH), "rs_a"),
                detail(Some(BEDROCK_MANTLE), "rs_foreign"),
                detail(Some(CODEX_OAUTH), "rs_b"),
            ]),
            Message {
                role: Role::User,
                content: MessageContent::Text("followup".to_string()),
                reasoning: None,
                reasoning_details: vec![],
                refusal: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]),
        anthropic_beta: vec!["prompt-caching-2024-07-31".to_string()],
        ..Default::default()
    };
    let mut expected = req.clone();
    Arc::make_mut(&mut expected.messages)[1]
        .reasoning_details
        .remove(1);

    // Act
    let stripped = strip_replay_artifacts(&mut req, ReplayScheme::Codex);

    // Assert
    assert!(stripped);
    assert_eq!(
        serde_json::to_value(&req).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn reasoning_replay_strip_with_nothing_removable_is_a_no_op() {
    // No removable artifact -> no copy-on-write, no reported change,
    // byte-identical request.
    let mut req = ChatRequest {
        messages: Arc::from(vec![assistant_with(vec![detail(
            Some(CODEX_OAUTH),
            "rs_carry",
        )])]),
        ..Default::default()
    };
    let before = serde_json::to_value(&req).unwrap();

    // Act
    let stripped = strip_replay_artifacts(&mut req, ReplayScheme::Codex);

    // Assert
    assert!(!stripped);
    assert_eq!(serde_json::to_value(&req).unwrap(), before);
}

#[test]
fn reasoning_replay_strip_does_not_disturb_other_request_clones() {
    // The messages buffer is shared behind an Arc; a strip on one
    // clone must copy-on-write rather than mutate the sibling.
    let original = ChatRequest {
        messages: Arc::from(vec![assistant_with(vec![detail(
            Some(BEDROCK_MANTLE),
            "rs_foreign",
        )])]),
        ..Default::default()
    };
    let mut attempt = original.clone();

    // Act
    assert!(strip_replay_artifacts(&mut attempt, ReplayScheme::Codex));

    // Assert
    assert!(attempt.messages[0].reasoning_details.is_empty());
    assert_eq!(original.messages[0].reasoning_details.len(), 1);
}
