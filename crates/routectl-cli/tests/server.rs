//! Integration tests for the routectl axum server (C6).
//!
//! Each test spins up a wiremock mock upstream and a routectl server bound to
//! 127.0.0.1:0 (OS-assigned port). Tests use `reqwest` to exercise the routes.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
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

    let mut models = BTreeMap::new();
    models.insert(
        "gpt-4o".to_string(),
        ModelEntry::new(provider_name, "gpt-4o"),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert(alias.to_string(), AliasValue::Single("gpt-4o".to_string()));

    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
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

    let mut models = BTreeMap::new();
    models.insert("first-gpt".to_string(), ModelEntry::new("first", "gpt-4o"));
    models.insert(
        "second-gpt".to_string(),
        ModelEntry::new("second", "gpt-4o"),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "multi".to_string(),
        AliasValue::Chain(vec!["first-gpt".into(), "second-gpt".into()]),
    );

    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.fallback_on_status = vec![503, 429, 500, 502, 504, 408];

    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry,
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
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

#[tokio::test]
async fn models_includes_alias_keys_and_nicknames() {
    // /v1/models is the discovery endpoint; without listing the
    // identifiers routing actually accepts, operators get
    // misleading "model unavailable" answers. Pin that the endpoint
    // advertises every routable name in v0.6.0: alias keys (incl.
    // wire-string -> nickname mappings) AND the bare nicknames from
    // [models].
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", "literal:k")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    let mut models = BTreeMap::new();
    models.insert("fast-model".into(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("fast-model".into()));
    aliases.insert(
        "openai-renamed-id".into(),
        AliasValue::Single("fast-model".into()),
    );
    aliases.insert(
        "claude-some-release".into(),
        AliasValue::Single("fast-model".into()),
    );
    aliases.insert(
        "default-catch-all".into(),
        AliasValue::Single("fast-model".into()),
    );

    let config = Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });
    let base = helpers::spawn_test_server(config).await;
    let body: Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"fast"), "alias key missing: {ids:?}");
    assert!(
        ids.contains(&"openai-renamed-id"),
        "wire-string alias missing: {ids:?}"
    );
    assert!(
        ids.contains(&"claude-some-release"),
        "wire-string alias missing: {ids:?}"
    );
    assert!(
        ids.contains(&"default-catch-all"),
        "alias key missing: {ids:?}"
    );
    assert!(
        ids.contains(&"fast-model"),
        "model nickname missing: {ids:?}"
    );
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

// ---------------------------------------------------------------------------
// `default` alias key: end-to-end ingress -> router -> provider
//
// The router unit tests in crates/routectl-router/tests/router.rs cover
// `Router::complete` directly. This test pins the WHOLE pipe: a real HTTP
// request hits the openai ingress, the router doesn't recognize the model,
// the configured `aliases.default` resolves to a known nickname, and the
// upstream actually receives the call. A future ingress refactor that
// consumes `req.model` before reaching the router would silently break
// the catch-all without this test catching it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completions_unknown_model_routes_to_default_alias() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body("gpt-4o", "Hello from default")),
        )
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "mock-provider".to_string(),
        ProviderEntry::openai_compat(upstream.uri(), "literal:test-key")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "fast-model".to_string(),
        ModelEntry::new("mock-provider", "gpt-4o"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".to_string(), AliasValue::Single("fast-model".into()));
    aliases.insert(
        "default".to_string(),
        AliasValue::Single("fast-model".into()),
    );
    let config = Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });

    let base = helpers::spawn_test_server(config).await;

    // Wire `model` here is NOT in [aliases] and NOT a direct nickname.
    // It should land on the `default` alias and reach the mock upstream.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude-some-future-release-2099",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from default"
    );
    assert_eq!(
        body["routectl_provider"], "mock-provider",
        "unknown model must reach the configured default alias destination"
    );
}

#[tokio::test]
async fn chat_completions_unknown_model_without_default_returns_error() {
    // No `default` alias configured. The router must still error
    // cleanly with UnknownAlias (not crash, not silently route
    // somewhere unexpected).
    let config = openai_compat_config("http://127.0.0.1:1", "mock-provider", "fast");
    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "totally-unknown-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    // UnknownAlias maps to 404 in handlers/ingress_handle.rs. Pin the
    // exact code so a future change that makes this 503 (network) or
    // 500 (internal error) trips this test instead of silently passing.
    assert_eq!(
        resp.status(),
        404,
        "unknown model with no default alias must produce 404 (UnknownAlias)"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "unknown_alias");
}

#[tokio::test]
async fn server_fails_startup_when_referenced_provider_cannot_build() {
    // A provider whose creds can't resolve is fine if no model
    // references it (the operator may have an unused-but-declared
    // provider in the TOML for environment variation). But once a
    // model entry points at it AND an alias chain references that
    // model, the misconfiguration must surface at startup -- not as
    // an `UnknownAlias` at first request time, hours after the
    // operator thought everything was healthy.
    use tokio::net::TcpListener;
    let mut providers = BTreeMap::new();
    providers.insert(
        "broken".into(),
        ProviderEntry::openai_compat(
            "http://127.0.0.1:1",
            "env://ROUTECTL_TEST_THIS_VAR_IS_NEVER_SET_F3",
        )
        .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    let mut models = BTreeMap::new();
    models.insert("broken-gpt".into(), ModelEntry::new("broken", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("broken-gpt".into()));
    let config = Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = routectl_cli::server::serve_on_listener(config, listener).await;
    match result {
        Err(routectl_core::Error::Config(msg)) => {
            assert!(
                msg.contains("broken"),
                "error must name the failed provider; got: {msg}"
            );
            assert!(
                msg.contains("alias `fast`"),
                "error must name the affected alias; got: {msg}"
            );
        }
        Ok(()) => panic!("server must error when an alias references an unbuildable provider"),
        Err(other) => panic!("expected Error::Config, got: {other:?}"),
    }
}

#[tokio::test]
async fn server_starts_when_unbuildable_provider_is_unreferenced() {
    // Counterpart: an unused-but-declared provider whose creds
    // can't resolve must NOT block startup. This is the intended
    // partial-config workflow (multiple providers in TOML, only
    // some active in the current env).
    use tokio::net::TcpListener;
    let mut providers = BTreeMap::new();
    providers.insert(
        "broken".into(),
        ProviderEntry::openai_compat(
            "http://127.0.0.1:1",
            "env://ROUTECTL_TEST_THIS_VAR_IS_NEVER_SET_F3",
        )
        .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    providers.insert(
        "working".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:2", "literal:test")
            .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai),
    );
    let mut models = BTreeMap::new();
    // Only define a model on `working`; `broken` is unused.
    models.insert("working-gpt".into(), ModelEntry::new("working", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("working-gpt".into()));
    let config = Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = routectl_cli::server::serve_on_listener(config, listener).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // /health returns 200 -> server actually started despite the
    // unreferenced broken provider.
    let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

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

/// Pin the serde_json default 128-deep recursion limit at the ingress
/// boundary. A maliciously deep JSON body must produce a 400, NOT
/// stack-overflow the tokio worker. Regression guard for the
/// security-review observation that axum::Json -> serde_json::from_slice
/// has no explicit depth cap on routectl's side; if a future change
/// switches to a custom `Deserializer` config, this test fires.
#[tokio::test]
async fn deeply_nested_json_returns_400_not_panic() {
    let config = openai_compat_config("http://127.0.0.1:1", "p", "fast");
    let base = helpers::spawn_test_server(config).await;

    // 1000 nested arrays, well past serde_json's 128-deep default.
    let depth = 1000;
    let mut body = String::with_capacity(depth * 2 + 4);
    for _ in 0..depth {
        body.push('[');
    }
    body.push('1');
    for _ in 0..depth {
        body.push(']');
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("request did not panic the worker -- depth limit honored");

    // serde_json's recursion limit returns a parse error; axum wraps
    // that as 400.
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "bad_request");
}
