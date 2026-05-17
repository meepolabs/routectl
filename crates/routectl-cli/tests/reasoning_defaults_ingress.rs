//! Integration tests for the per-provider `[providers.X] thinking` /
//! `enabled` operator-side reasoning defaults. End-to-end through the
//! axum server + a wiremock upstream that captures the routed request
//! body so we can verify what reaches each provider's wire shape.
//!
//! What's covered:
//!   - Anthropic legacy thinking shape: `thinking = "high"` + the
//!     caller's `max_tokens` produce a proportional `budget_tokens`
//!     on the upstream body.
//!   - Anthropic adaptive thinking shape: `thinking = "xhigh"` +
//!     `adaptive_thinking = true` produces `thinking.type =
//!     "adaptive"` and lifts the effort string into top-level
//!     `output_config.effort`.
//!   - OpenAI Responses: `thinking = "high"` produces `reasoning =
//!     {effort: "high", summary: "auto"}` on the body the chatgpt-oauth
//!     endpoint receives.
//!   - vLLM dialect: `enabled = true` injects
//!     `chat_template_kwargs.enable_thinking = true`.
//!   - Caller precedence: a caller-supplied `thinking.budget_tokens`
//!     wins over the operator's `thinking = "high"` default.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{
    AliasEntry, Config, IngressConfig, IngressShape, ProviderEntry, ReasoningDefaults,
    ReasoningDialect, RetryPolicy, ServerConfig,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod helpers {
    use std::sync::Arc;

    use routectl_router::Config;
    use tokio::net::TcpListener;

    pub async fn spawn(config: Arc<Config>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener)
                .await
                .expect("server failed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        base_url
    }
}

// ---------------------------------------------------------------------------
// Test fixtures shared across cases
// ---------------------------------------------------------------------------

fn anthropic_response_body() -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

fn openai_compat_response_body() -> Value {
    json!({
        "id": "chatcmpl-01",
        "object": "chat.completion",
        "created": 0,
        "model": "qwen3-30b",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

fn empty_server() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
    }
}

fn empty_ingress() -> IngressConfig {
    IngressConfig {
        anthropic: IngressShape::default(),
        openai: IngressShape::default(),
    }
}

/// Build a config with one Anthropic-API provider whose `[providers.X]
/// thinking` is set. `adaptive` toggles the Opus 4.7+ wire shape.
fn anthropic_config_with_defaults(
    upstream_base: &str,
    thinking: &str,
    adaptive: bool,
) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let mut entry =
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_base.to_string());
    if let ProviderEntry::AnthropicApi {
        adaptive_thinking,
        reasoning_defaults,
        ..
    } = &mut entry
    {
        *adaptive_thinking = Some(adaptive);
        *reasoning_defaults = ReasoningDefaults::new(Some(thinking.to_string()), None);
    }
    providers.insert("anthropic-mock".to_string(), entry);

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasEntry::new(vec!["anthropic-mock:claude-haiku-4-5".to_string()]),
    );

    Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        default_model: None,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress: empty_ingress(),
        ..Default::default()
    })
}

/// Build a config with one openai-compat provider on the vllm dialect
/// whose `[providers.X] enabled` is set.
fn vllm_config_with_enabled(upstream_base: &str, enabled: bool) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let mut entry = ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key")
        .with_reasoning_dialect(ReasoningDialect::Vllm);
    if let ProviderEntry::OpenaiCompat {
        reasoning_defaults, ..
    } = &mut entry
    {
        *reasoning_defaults = ReasoningDefaults::new(None, Some(enabled));
    }
    providers.insert("vllm-mock".to_string(), entry);

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasEntry::new(vec!["vllm-mock:qwen3-30b".to_string()]),
    );

    Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        default_model: None,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress: empty_ingress(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Case (a): Anthropic legacy thinking shape derives budget from default
// ---------------------------------------------------------------------------

/// Pin: `[providers.X] thinking = "high"` on a non-adaptive Anthropic
/// provider produces the legacy wire shape `thinking.type = "enabled"`
/// with `budget_tokens = floor(max_tokens * 0.80)`. Caller sent no
/// reasoning fields at all.
#[tokio::test]
async fn anthropic_default_thinking_high_reaches_upstream_legacy_shape() {
    // Arrange
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "high", false);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Assert
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "enabled",
        "expected legacy thinking shape; got body: {upstream_body}"
    );
    // 1024 * 0.80 = 819.2 -> u32 truncation -> 819.
    assert_eq!(
        upstream_body["thinking"]["budget_tokens"], 819,
        "operator default thinking=high must derive budget from caller max_tokens"
    );
}

// ---------------------------------------------------------------------------
// Case (b): Anthropic adaptive thinking shape lifts effort to output_config
// ---------------------------------------------------------------------------

/// Pin: `[providers.X] thinking = "xhigh"` on an adaptive provider
/// (Opus 4.7+) produces the adaptive wire shape (`thinking.type =
/// "adaptive"` with no budget field) AND lifts the effort string into
/// top-level `output_config.effort`.
#[tokio::test]
async fn anthropic_adaptive_default_thinking_xhigh_emits_output_config() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "xhigh", true);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 4096,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "adaptive",
        "expected adaptive thinking wire shape; got body: {upstream_body}"
    );
    assert!(
        upstream_body["thinking"].get("budget_tokens").is_none(),
        "adaptive shape must NOT carry budget_tokens; got: {}",
        upstream_body["thinking"]
    );
    assert_eq!(
        upstream_body["output_config"]["effort"], "xhigh",
        "operator default thinking=xhigh must lift to output_config.effort on adaptive"
    );
}

// ---------------------------------------------------------------------------
// Case (c): OpenAI Responses default thinking emits reasoning block
// ---------------------------------------------------------------------------

/// Pin: `[providers.X] thinking = "high"` on an OpenAI Responses
/// provider emits `reasoning = {effort: "high", summary: "auto"}` on
/// the upstream body. The chatgpt-oauth endpoint is stream-only so the
/// mock returns an SSE `response.completed` event.
///
/// Note: routectl-cli's dependency on `routectl-providers` enables the
/// `openai-responses` feature unconditionally, so this test is not
/// cfg-gated. The unit test in `routectl-router::config` IS cfg-gated
/// because that crate's openai-responses feature is optional.
#[tokio::test]
async fn openai_responses_default_thinking_high_emits_reasoning_block() {
    let upstream = MockServer::start().await;
    let completed_body = json!({
        "id": "resp_01",
        "object": "response",
        "status": "completed",
        "model": "gpt-5.3-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}],
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
    });
    let event_body = format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed_body).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event_body),
        )
        .mount(&upstream)
        .await;

    // Build the OpenaiResponses provider via TOML deserialization
    // because the variant is `#[non_exhaustive]` and the routectl-cli
    // test crate (external to routectl-router) cannot use struct-
    // literal syntax. The factory rejects an absent `account_id_ref`
    // for `auth_kind = "chatgpt-oauth"`, so supply a placeholder UUID.
    let toml_text = format!(
        r#"
type = "openai-responses"
api_key_ref = "literal:test-jwt"
account_id_ref = "literal:00000000-0000-0000-0000-000000000000"
base_url = "{upstream_uri}"
auth_kind = "chatgpt-oauth"
thinking = "high"
"#,
        upstream_uri = upstream.uri()
    );
    let entry: ProviderEntry =
        toml::from_str(&toml_text).expect("openai-responses entry should parse");

    let mut providers = BTreeMap::new();
    providers.insert("gpt-mock".to_string(), entry);

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasEntry::new(vec!["gpt-mock:gpt-5.3-codex".to_string()]),
    );

    let config = Arc::new(Config {
        server: empty_server(),
        providers,
        aliases,
        default_model: None,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress: empty_ingress(),
        ..Default::default()
    });

    let base = helpers::spawn(config).await;

    // Use the OpenAI Chat Completions ingress so we can drive the
    // openai-responses egress without the Anthropic ingress in the
    // path. The body is a plain chat completions request -- routectl
    // translates internally to the Responses wire shape.
    let body = json!({
        "model": "heavy",
        "messages": [{"role": "user", "content": "hello"}],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "ingress rejected: {} body: {}",
        resp.status(),
        resp.text().await.unwrap_or_default(),
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["reasoning"]["effort"], "high",
        "operator default thinking=high must emit reasoning.effort=high; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["reasoning"]["summary"], "auto",
        "openai-responses egress always pairs effort with summary=auto"
    );
}

// ---------------------------------------------------------------------------
// Case (d): vLLM default enabled=true emits chat_template_kwargs
// ---------------------------------------------------------------------------

/// Pin: `[providers.X] enabled = true` on a vLLM-dialect openai-compat
/// provider injects `chat_template_kwargs.enable_thinking = true` on
/// the upstream body. Caller sent no reasoning fields.
#[tokio::test]
async fn vllm_default_enabled_true_emits_chat_template_kwargs() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = vllm_config_with_enabled(&upstream.uri(), true);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "messages": [{"role": "user", "content": "hello"}],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["chat_template_kwargs"]["enable_thinking"],
        json!(true),
        "operator default enabled=true must inject enable_thinking; got body: {upstream_body}"
    );
}

// ---------------------------------------------------------------------------
// Case (e): caller reasoning overrides operator default
// ---------------------------------------------------------------------------

/// Pin: caller-supplied `thinking.budget_tokens` on the wire wins over
/// the operator's `thinking = "high"` default. The egress must emit
/// the caller's exact budget (256), NOT a budget derived from the
/// operator default (which would be ~3276 for `high` at max_tokens=
/// 4096).
#[tokio::test]
async fn caller_reasoning_overrides_provider_default_high() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_config_with_defaults(&upstream.uri(), "high", false);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 256},
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        }],
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        upstream_body["thinking"]["type"], "enabled",
        "caller asked for legacy enabled shape; got body: {upstream_body}"
    );
    assert_eq!(
        upstream_body["thinking"]["budget_tokens"], 256,
        "caller's budget_tokens=256 must win over operator default thinking=high"
    );
}
