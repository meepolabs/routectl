//! Only the Anthropic ingress fills the native thinking carrier; the other
//! dialects leave it uncaptured so readers use the canonical config.

use axum::http::HeaderMap;
use routectl_core::AnthropicThinking;
use serde_json::json;

use crate::ingress::IngressAdapter;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;

#[test]
fn openai_chat_ingress_leaves_thinking_uncaptured() {
    let req = OpenAiIngress
        .parse_request_value(
            &HeaderMap::new(),
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning_effort": "high",
            }),
        )
        .expect("parses");

    assert!(
        req.reasoning.is_some(),
        "the fixture carries reasoning intent"
    );
    assert_eq!(
        req.routectl_internal.anthropic_thinking,
        AnthropicThinking::NotCaptured
    );
}

#[test]
fn responses_ingress_leaves_thinking_uncaptured() {
    let req = ResponsesIngress
        .parse_request_value(
            &HeaderMap::new(),
            json!({
                "model": "m",
                "input": "hi",
                "reasoning": {"effort": "high"},
            }),
        )
        .expect("parses");

    assert!(
        req.reasoning.is_some(),
        "the fixture carries reasoning intent"
    );
    assert_eq!(
        req.routectl_internal.anthropic_thinking,
        AnthropicThinking::NotCaptured
    );
}
