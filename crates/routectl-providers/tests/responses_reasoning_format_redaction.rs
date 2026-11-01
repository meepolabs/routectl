//! The OpenAI Responses egress's foreign-format DEBUG `formats` field under
//! prompt redaction. A detail whose format tag is not Responses-family is
//! skipped, and its tag is reported. A RECOGNIZED tag (Anthropic's own, for
//! instance) is closed protocol vocabulary and keeps echoing; an
//! UNRECOGNIZED tag is a caller-chosen free-text string, so with
//! `ROUTECTL_LOG_REDACT_PROMPTS=1` it collapses to the `<unrecognized>`
//! placeholder. The knob-OFF literal echo is pinned in the lane's own unit
//! tests, which run with the knob unset.
//!
//! Why a dedicated test binary: the redaction knob is read once per
//! process and frozen, so proving the on-state requires a pristine
//! process with the variable set before the first read. Both tests here
//! share the SAME knob state (on), and no sibling test races the
//! process-environment mutation.

#![cfg(feature = "openai-responses")]

use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, ReasoningDetail, ReasoningDetailKind, Role,
};
use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};

const KNOWN_NON_RESPONSES_TAG: &str = "anthropic-claude-v1";
const UNKNOWN_TAG: &str = "openai-o-format";

fn make_provider() -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
        "test-openai-responses",
        "test-key",
    ))
}

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

/// A request whose assistant turn carries one reasoning detail in `format`,
/// which the Responses translator cannot replay and therefore skips.
fn request_with_format(format: &str) -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        messages: vec![
            user_msg("think then reply"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("I thought about it.".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: None,
                    format: Some(format.to_string()),
                    index: Some(0),
                    payload: serde_json::json!({"text": "some reasoning"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("and again"),
        ]
        .into(),
        ..Default::default()
    }
}

/// Normalize `req` and return the `formats` field of the foreign-format
/// DEBUG record.
fn skipped_formats_for(req: &ChatRequest) -> String {
    let provider = make_provider();
    let events = routectl_testkit::capture_events(|| {
        provider.normalize_request(req).expect("normalize ok");
    });
    let record = events
        .iter()
        .find(|e| {
            e.message.contains(
                "openai-responses: skipped reasoning_details entries with a non-Responses-family format",
            )
        })
        .unwrap_or_else(|| panic!("expected the foreign-format record; got events: {events:?}"));
    record
        .field("formats")
        .expect("formats field present")
        .to_string()
}

/// Turn prompt redaction on for this process, before any reader freezes the
/// knob.
///
/// SAFETY: every test in this binary wants the knob ON and sets it to the
/// same value, so a concurrent sibling can only write the value already
/// there; no test reads a different state.
fn enable_prompt_redaction() {
    unsafe { std::env::set_var("ROUTECTL_LOG_REDACT_PROMPTS", "1") };
}

#[test]
fn known_non_responses_format_tag_survives_prompt_redaction() {
    // Arrange: a recognized tag from another dialect is closed vocabulary,
    // so redaction has nothing to protect and the diagnostic keeps its value.
    enable_prompt_redaction();
    let req = request_with_format(KNOWN_NON_RESPONSES_TAG);

    // Act
    let rendered = skipped_formats_for(&req);

    // Assert
    assert_eq!(
        rendered,
        format!("[\"{KNOWN_NON_RESPONSES_TAG}\"]"),
        "a known format tag must stay visible under prompt redaction"
    );
}

#[test]
fn unrecognized_format_tag_collapses_to_a_placeholder_under_prompt_redaction() {
    // Arrange: a tag outside the vocabulary is caller-chosen free text, and
    // the operator opted into keeping caller content out of the logs.
    enable_prompt_redaction();
    let req = request_with_format(UNKNOWN_TAG);

    // Act
    let rendered = skipped_formats_for(&req);

    // Assert
    assert_eq!(
        rendered, "[\"<unrecognized>\"]",
        "an unknown tag must render as the placeholder under redaction"
    );
    assert!(
        !rendered.contains(UNKNOWN_TAG),
        "the caller-chosen tag must not reach the log line under redaction; got: {rendered}"
    );
}
