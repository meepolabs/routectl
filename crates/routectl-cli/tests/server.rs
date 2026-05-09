//! Integration tests for the routectl axum server (C6).
//!
//! Each test spins up a wiremock mock upstream and a routectl server bound to
//! 127.0.0.1:0 (OS-assigned port). Tests use `reqwest` to exercise the routes.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasEntry, Config, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod helpers {
    use std::sync::Arc;

    use routectl_router::Config;
    use tokio::net::TcpListener;

    /// Bind to 127.0.0.1:0, spawn the server in a background tokio task,
    /// return the bound base URL (e.g. "http://127.0.0.1:54321").
    pub async fn spawn_test_server(config: Arc<Config>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener)
                .await
                .expect("server failed");
        });

        // Give the server a tick to accept connections.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        base_url
    }
}

// ---------------------------------------------------------------------------
// Config builder helpers
// ---------------------------------------------------------------------------

fn openai_compat_config(upstream_base: &str, provider_name: &str, alias: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::openai_compat(upstream_base, "literal:test-key")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert(
        alias.to_string(),
        AliasEntry::new(vec![format!("{provider_name}:gpt-4o")]),
    );

    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        default_model: None,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress: Default::default(),
    })
}

fn two_provider_config(first_base: &str, second_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "first".to_string(),
        ProviderEntry::openai_compat(first_base, "literal:test-key")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    providers.insert(
        "second".to_string(),
        ProviderEntry::openai_compat(second_base, "literal:test-key")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );

    let mut aliases = BTreeMap::new();
    let rp = {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 1;
        rp.fallback_on_status = vec![503, 429, 500, 502, 504, 408];
        rp
    };
    aliases.insert(
        "multi".to_string(),
        AliasEntry::new(vec![
            "first:gpt-4o".to_string(),
            "second:gpt-4o".to_string(),
        ])
        .with_retry(rp),
    );

    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        default_model: None,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress: Default::default(),
    })
}

fn chat_response_body(model: &str, content: &str) -> Value {
    json!({
        "id": "chatcmpl-test-001",
        "object": "chat.completion",
        "model": model,
        "created": 1700000000_i64,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    })
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_ok() {
    let config = openai_compat_config("http://127.0.0.1:1", "unused", "fast");
    let base = helpers::spawn_test_server(config).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
}

// ---------------------------------------------------------------------------
// GET /v1/models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_lists_configured_aliases() {
    let config = openai_compat_config("http://127.0.0.1:1", "provider1", "my-alias");
    let base = helpers::spawn_test_server(config).await;

    let resp = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");

    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();

    assert!(ids.contains(&"my-alias"), "expected my-alias in {ids:?}");
}

// ---------------------------------------------------------------------------
// POST /v1/chat/completions -- non-streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions_non_streaming_returns_response() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body("gpt-4o", "Hello from mock")),
        )
        .mount(&upstream)
        .await;

    let config = openai_compat_config(&upstream.uri(), "mock-provider", "fast");
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "fast",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from mock");
}

// ---------------------------------------------------------------------------
// POST /v1/chat/completions -- streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions_streaming_returns_sse_events() {
    let sse_body = concat!(
        "data: {\"id\":\"s1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&upstream)
        .await;

    let config = openai_compat_config(&upstream.uri(), "mock-provider", "fast");
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "fast",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let text = resp.text().await.unwrap();
    // Must contain at least one data: line with JSON and a final [DONE].
    assert!(text.contains("data: "), "no SSE data lines in:\n{text}");
    assert!(
        text.contains("data: [DONE]"),
        "no [DONE] terminator in:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Bind safety: non-loopback without --unsafe-public must error (no bind)
// ---------------------------------------------------------------------------

#[test]
fn bind_safety_rejects_non_loopback_without_flag() {
    use routectl_cli::server::check_bind_safety;

    let result = check_bind_safety("0.0.0.0", false);
    assert!(
        result.is_err(),
        "expected error for 0.0.0.0 without unsafe_public"
    );

    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("non-loopback") || msg.contains("refusing"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn bind_safety_accepts_loopback_without_flag() {
    use routectl_cli::server::check_bind_safety;

    assert!(check_bind_safety("127.0.0.1", false).is_ok());
    assert!(check_bind_safety("localhost", false).is_ok());
    assert!(check_bind_safety("::1", false).is_ok());
}

#[test]
fn bind_safety_allows_non_loopback_with_unsafe_public() {
    use routectl_cli::server::check_bind_safety;

    // Should not return error (may log a warning).
    assert!(check_bind_safety("0.0.0.0", true).is_ok());
}

// ---------------------------------------------------------------------------
// Fallback test: first provider 503 -> second provider 200
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fallback_to_second_provider_on_503() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
        .mount(&failing)
        .await;

    let working = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body("gpt-4o", "fallback answer")),
        )
        .mount(&working)
        .await;

    let config = two_provider_config(&failing.uri(), &working.uri());
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "multi",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "fallback answer");
    // routectl_provider extension should identify which provider answered.
    assert_eq!(
        body["routectl_provider"], "second",
        "expected second provider but got: {body}"
    );
}

// ---------------------------------------------------------------------------
// Error envelope: unknown alias -> 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_alias_returns_404_error_envelope() {
    let config = openai_compat_config("http://127.0.0.1:1", "p", "fast");
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "does-not-exist",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"].is_string());
    assert_eq!(body["error"]["type"], "unknown_alias");
}

// ---------------------------------------------------------------------------
// Error envelope: malformed JSON body -> 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_json_returns_400() {
    let config = openai_compat_config("http://127.0.0.1:1", "p", "fast");
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{not-valid-json}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "bad_request");
}
