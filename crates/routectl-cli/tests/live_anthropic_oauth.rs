//! Live OAuth-bearer integration test against api.anthropic.com /v1/messages.
//!
//! Skipped unless `ROUTECTL_TEST_CLAUDE_OAUTH_TOKEN_FILE` is set to a
//! path containing a raw Anthropic OAuth bearer access token (one
//! line; trailing whitespace OK). The developer running the test is
//! responsible for sourcing a token they're permitted to use this way.
//!
//! Run with:
//!
//!   cargo test -p routectl-cli --features live-integration --release \
//!     --test live_anthropic_oauth -- --nocapture

#![cfg(feature = "live-integration")]

use std::time::Duration;

use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};

const ENV_TOKEN_FILE: &str = "ROUTECTL_TEST_CLAUDE_OAUTH_TOKEN_FILE";
const MODEL: &str = "claude-haiku-4-5";
const PROMPT: &str = "Reply with just the word: pong";
const TIMEOUT_SECS: u64 = 60;

fn read_token_file() -> Option<String> {
    let path = std::env::var(ENV_TOKEN_FILE).ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn make_request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(PROMPT.to_string()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(40),
        stream: Some(stream),
        temperature: None,
        top_p: None,
        stop: None,
        n: None,
        seed: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        reasoning: None,
        chat_template_kwargs: None,
        provider_extras: None,
    }
}

fn build_oauth_provider(token: String) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new("anthropic-oauth-test", token);
    cfg.auth_kind = AuthKind::OauthBearer;
    cfg.extra_headers =
        vec![("anthropic-beta".into(), "oauth-2025-04-20".into())];
    AnthropicApiProvider::new(cfg)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_oauth_complete_with_bearer_token() {
    let Some(token) = read_token_file() else {
        eprintln!(
            "skip: {ENV_TOKEN_FILE} not set or file empty. \
             Set to a file containing a raw Anthropic OAuth bearer access token."
        );
        return;
    };

    let provider = build_oauth_provider(token);
    let req = make_request(false);

    let result = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), provider.complete(req))
        .await
        .expect("oauth completion timed out");

    let resp = result.expect("oauth completion failed");
    let preview = resp
        .choices
        .first()
        .map(|c| match &c.message.content {
            MessageContent::Text(t) => t.clone(),
            _ => "<non-text>".into(),
        })
        .unwrap_or_default();
    let tokens = resp.usage.as_ref().map(|u| u.total_tokens).unwrap_or(0);
    eprintln!("PASS anthropic-oauth complete model={MODEL} tokens={tokens} content={preview:?}");
    assert!(!preview.is_empty(), "expected non-empty completion text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_oauth_stream_with_bearer_token() {
    use futures::StreamExt;

    let Some(token) = read_token_file() else {
        eprintln!("skip: {ENV_TOKEN_FILE} not set or file empty");
        return;
    };

    let provider = build_oauth_provider(token);
    let req = make_request(true);

    let mut stream = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), provider.stream(req))
        .await
        .expect("oauth stream open timed out")
        .expect("oauth stream open failed");

    let mut text = String::new();
    let mut chunks = 0u32;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), stream.next())
        .await
        .expect("oauth stream chunk timed out")
    {
        let chunk = item.expect("oauth stream chunk error");
        chunks += 1;
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = choice.delta.content.as_ref() {
                text.push_str(content);
            }
        }
    }

    eprintln!("PASS anthropic-oauth stream model={MODEL} chunks={chunks} content={text:?}");
    assert!(chunks > 0, "expected at least one streamed chunk");
    assert!(!text.is_empty(), "expected non-empty streamed text");
}
