//! The foreign-format WARN's `skipped_formats` field under prompt
//! redaction: a RECOGNIZED format tag is closed protocol vocabulary and
//! keeps echoing, because naming which known dialect arrived is the whole
//! diagnostic value of the field and the tag carries no caller bytes. An
//! UNRECOGNIZED tag is a caller-chosen free-text string, and an operator
//! who set `ROUTECTL_LOG_REDACT_PROMPTS=1` asked for exactly that class of
//! content to stay out of the log line -- so it collapses to the
//! `<unrecognized>` placeholder. The forward-compat discovery loop (seeing
//! the literal of a tag routectl does not know yet) survives one
//! knob-toggle away; that knob-OFF cell is pinned deterministically by the
//! `render_skipped_format` unit tests in
//! `anthropic_api/messages_reasoning_warn_tests.rs`, which take the flag as
//! an argument instead of the frozen process-wide read. The reasoning body
//! itself never reaches this record in either mode.
//!
//! Why a dedicated test binary: the redaction knob is read once per
//! process and frozen, so proving the on-state requires a pristine
//! process with the variable set before the first read. Both tests here
//! share the SAME knob state (on), and no sibling test races the
//! process-environment mutation.

#![cfg(feature = "anthropic-api")]

use routectl_core::{
    ChatRequest, Message, MessageContent, Provider, ReasoningDetail, ReasoningDetailKind, Role,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};

fn make_provider() -> AnthropicApiProvider {
    let cfg = AnthropicApiConfig {
        id: "test-anthropic".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    AnthropicApiProvider::new(cfg)
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
/// which the Anthropic translator cannot echo and therefore skips.
fn request_with_format(format: &str) -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-7".into(),
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
                    payload: serde_json::json!({"text": "some reasoning", "signature": "sig"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        max_tokens: Some(2048),
        ..Default::default()
    }
}

/// Normalize `req` and return the `skipped_formats` field of the
/// foreign-format WARN.
fn skipped_formats_for(req: &ChatRequest) -> String {
    let provider = make_provider();
    let events = routectl_testkit::capture_events(|| {
        provider.normalize_request(req).expect("normalize ok");
    });
    let warn = events
        .iter()
        .find(|e| {
            e.message
                .contains("skipping reasoning blocks on replay: format is not anthropic-claude-v1")
        })
        .unwrap_or_else(|| panic!("expected the foreign-format WARN; got events: {events:?}"));
    warn.field("skipped_formats")
        .expect("skipped_formats field present")
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
fn known_format_tag_survives_prompt_redaction() {
    // Arrange: a recognized Responses-family tag -- closed vocabulary, so
    // redaction has nothing to protect and the diagnostic keeps its value.
    enable_prompt_redaction();
    let req = request_with_format(routectl_core::CODEX_OAUTH);

    // Act
    let rendered = skipped_formats_for(&req);

    // Assert
    assert!(
        rendered.contains(routectl_core::CODEX_OAUTH),
        "a known format tag must stay visible under prompt redaction; got: {rendered}"
    );
    assert!(
        !rendered.contains("<unrecognized>"),
        "a known tag must not be collapsed to the unrecognized placeholder; got: {rendered}"
    );
}

#[test]
fn unrecognized_format_tag_collapses_to_a_placeholder_under_prompt_redaction() {
    // Arrange: a tag outside the vocabulary is caller-chosen free text, and
    // the operator opted into keeping caller content out of the logs.
    enable_prompt_redaction();
    let req = request_with_format("openai-o-format");

    // Act
    let rendered = skipped_formats_for(&req);

    // Assert
    assert!(
        rendered.contains("<unrecognized>"),
        "an unknown tag must render as the placeholder under redaction; got: {rendered}"
    );
    assert!(
        !rendered.contains("openai-o-format"),
        "the caller-chosen tag must not reach the log line under redaction; got: {rendered}"
    );
}
