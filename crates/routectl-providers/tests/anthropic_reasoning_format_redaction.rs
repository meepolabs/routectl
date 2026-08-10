//! The foreign-format WARN's `skipped_formats` field must stay readable
//! when the operator turns prompt redaction on: a format tag is
//! caller-chosen protocol vocabulary, and the whole diagnostic value of
//! the field is seeing WHICH foreign dialect arrived. The reasoning body
//! itself never reaches this record.
//!
//! Why a dedicated test binary: the redaction knob is read once per
//! process and frozen, so proving the on-state requires a pristine
//! process with the variable set before the first read. One test per
//! binary also means no sibling test races the process-environment
//! mutation.

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

#[test]
fn foreign_format_tag_survives_prompt_redaction() {
    // Arrange: prompt redaction on for this process, before any reader
    // freezes the knob.
    // SAFETY: this binary declares exactly one test, so no sibling test
    // reads or writes the process environment concurrently.
    unsafe { std::env::set_var("ROUTECTL_LOG_REDACT_PROMPTS", "1") };
    let provider = make_provider();
    let req = ChatRequest {
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
                    format: Some("openai-o-format".to_string()),
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
    };

    // Act
    let events = routectl_testkit::capture_events(|| {
        provider.normalize_request(&req).expect("normalize ok");
    });

    // Assert
    let warn = events
        .iter()
        .find(|e| {
            e.message
                .contains("skipping reasoning blocks on replay: format is not anthropic-claude-v1")
        })
        .unwrap_or_else(|| panic!("expected the foreign-format WARN; got events: {events:?}"));
    let rendered = warn
        .field("skipped_formats")
        .expect("skipped_formats field present");
    assert!(
        rendered.contains("openai-o-format"),
        "the format tag must stay visible under prompt redaction; got: {rendered}"
    );
    assert!(
        !rendered.contains("<redacted"),
        "the format tag must not be collapsed to a redaction marker; got: {rendered}"
    );
}
