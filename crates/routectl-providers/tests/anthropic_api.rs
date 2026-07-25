//! Tests for the Anthropic API provider.
//!
//! Covers:
//!   - Request normalization (system lift, reasoning, tools, multi-turn signature)
//!   - Response normalization (thinking blocks, text, tool_use, stop_reason mapping)
//!   - SSE state machine (full event sequence with signature_delta)
//!   - wiremock integration for complete and stream paths

#![cfg(feature = "anthropic-api")]

use routectl_core::Provider;
use routectl_core::{
    ChatRequest, KnownContentPart, Message, MessageContent, ReasoningConfig, ReasoningDetail,
    ReasoningDetailKind, Role, SystemBlock, ToolDef, cache_control::CacheControl,
    content_part::ContentPart, system_content::SystemContent, tool_def::CustomTool,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn make_provider(base_url: &str) -> AnthropicApiProvider {
    let cfg = AnthropicApiConfig {
        id: "test-anthropic".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.to_string(),
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

fn base_req(model: &str, msgs: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: msgs.into(),
        // 2048 sits above the Anthropic legacy-thinking floor
        // (`max_tokens > 1024`) so tests exercising the
        // `ThinkingConfig::Enabled` arm reach the wire body
        // instead of being dropped at the new gate in
        // `build_thinking`. See `small_max_tokens_drops_legacy_thinking`
        // in `anthropic_api/request.rs::tests` for the dropped
        // case.
        max_tokens: Some(2048),
        ..Default::default()
    }
}

fn make_response_body() -> Value {
    json!({
        "id": "msg_check",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
        "content": [{"type": "text", "text": "ok"}]
    })
}

#[path = "anthropic_api/beta_headers.rs"]
mod beta_headers;
#[path = "anthropic_api/cache_control.rs"]
mod cache_control;
#[path = "anthropic_api/count_tokens.rs"]
mod count_tokens;
#[path = "anthropic_api/integration.rs"]
mod integration;
#[cfg(feature = "bedrock")]
#[path = "anthropic_api/mantle.rs"]
mod mantle;
#[path = "anthropic_api/probe.rs"]
mod probe;
#[path = "anthropic_api/request_normalization.rs"]
mod request_normalization;
#[path = "anthropic_api/response_normalization.rs"]
mod response_normalization;
#[path = "anthropic_api/sse.rs"]
mod sse;
#[path = "anthropic_api/unified_quota.rs"]
mod unified_quota;
