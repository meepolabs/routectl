//! Integration tests for the routectl axum server.
//!
//! Each test spins up a wiremock mock upstream and a routectl server bound to
//! 127.0.0.1:0 (OS-assigned port). Tests use `reqwest` to exercise the routes.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerAuth, ServerConfig,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

mod helpers {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use routectl_router::Config;
    use tokio::net::TcpListener;

    /// Bind to 127.0.0.1:0, spawn the server in a background tokio task,
    /// return the bound base URL (e.g. "http://127.0.0.1:54321") once
    /// `/health` responds.
    ///
    /// The config's usage DB is redirected to a unique per-process path
    /// (via `common::isolate_usage_db`) before serving so the now-live
    /// usage writer never touches the real `~/.config/routectl/usage.db`
    /// (the `UsageConfig` default). See `common::isolate_usage_db` for why
    /// that path is persistent / leaked rather than guarded.
    ///
    /// Readiness is decided by a successful `/health` response, not by a
    /// fixed sleep: the listener is bound before the serve task starts, so
    /// a bare TCP connect succeeds from the OS backlog before the router
    /// can answer. Polling the live endpoint against a deadline instead
    /// keeps a slow boot on a loaded CI box from flaking the suite.
    pub async fn spawn_test_server(config: Arc<Config>) -> String {
        let config = crate::common::isolate_usage_db(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener, None)
                .await
                .expect("server failed");
        });

        await_health(&base_url).await;
        base_url
    }

    /// Poll `GET {base_url}/health` until it returns success or a 5s
    /// deadline elapses. The 20ms inter-attempt pause is a poll cadence,
    /// not a readiness wait -- readiness is the 200 response.
    async fn await_health(base_url: &str) {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(resp) = client.get(format!("{base_url}/health")).send().await
                && resp.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("test server did not become healthy at {base_url}");
    }
}

// ---------------------------------------------------------------------------
// Config builder helpers
// ---------------------------------------------------------------------------

fn openai_compat_config(upstream_base: &str, provider_name: &str, alias: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::openai_compat(upstream_base, common::file_ref("test-key")),
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
        models,
        ..Default::default()
    })
}

fn two_provider_config(first_base: &str, second_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "first".to_string(),
        ProviderEntry::openai_compat(first_base, common::file_ref("test-key")),
    );
    providers.insert(
        "second".to_string(),
        ProviderEntry::openai_compat(second_base, common::file_ref("test-key")),
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

    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry,
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
        ProviderEntry::openai_compat("http://127.0.0.1:1", common::file_ref("k")),
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
        ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
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

/// Set an env var for the test's duration and restore the prior value on
/// drop so an assertion failure cannot leak `XDG_CONFIG_HOME` into sibling
/// tests.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvGuard {
    // SAFETY: process-env mutation is unsynchronized, so every test that
    // constructs an EnvGuard MUST be #[serial_test::serial]; sibling tests
    // in this binary pass explicit config paths and do not read
    // XDG_CONFIG_HOME, so no non-serial reader races the mutation.
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvGuard::set -- restore runs under the same
        // #[serial_test::serial] test that created the guard.
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn activated_inventory_never_synthesizes_routes() {
    // A fully-activated OAuth inventory (a seeded, unexpired anthropic
    // credential) must not leak into routing: with an EMPTY config (no
    // providers / models / aliases), /v1/models stays empty and a direct
    // dispatch still 404s UnknownAlias. Activation state lives on AppState
    // as a sibling of the router swap -- physically outside the Router the
    // dispatch path reads -- so it is unreachable from routing by
    // construction. This pins that guarantee end-to-end over the HTTP
    // surface.
    let xdg = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());

    // Seed <xdg>/routectl/credentials.json with a Present anthropic token so
    // the startup activation compute marks anthropic Activated.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let creds = json!({
        "schema_version": 1,
        "providers": {
            "anthropic": {
                "access_token": "activated-inventory-token",
                "refresh_token": "activated-inventory-refresh",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:inference"],
                "obtained_at_unix": now
            }
        }
    });
    let creds_path = xdg.path().join("routectl").join("credentials.json");
    std::fs::create_dir_all(creds_path.parent().unwrap()).unwrap();
    std::fs::write(&creds_path, serde_json::to_vec_pretty(&creds).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // Empty config: no providers, models, or aliases at all.
    let base = helpers::spawn_test_server(Arc::new(Config::default())).await;

    let client = reqwest::Client::new();

    // /v1/models lists ZERO entries -- activation synthesized no models.
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        models["data"].as_array().map(Vec::len),
        Some(0),
        "activated inventory must not add any /v1/models entries: {models}"
    );

    // A direct dispatch still 404s UnknownAlias -- no ResolvedModel was
    // synthesized from the activated provider.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "an activated but unconfigured provider must not become routable"
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
        ),
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
        models,
        ..Default::default()
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = routectl_cli::server::serve_on_listener(config, listener, None).await;
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
    let mut providers = BTreeMap::new();
    providers.insert(
        "broken".into(),
        ProviderEntry::openai_compat(
            "http://127.0.0.1:1",
            "env://ROUTECTL_TEST_THIS_VAR_IS_NEVER_SET_F3",
        ),
    );
    providers.insert(
        "working".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:2", common::file_ref("test")),
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
        models,
        ..Default::default()
    });

    // Route through spawn_test_server so the (booted) usage writer is
    // isolated to a tempdir DB, not the real on-disk path.
    let base = helpers::spawn_test_server(config).await;

    // /health returns 200 -> server actually started despite the
    // unreferenced broken provider.
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
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

// ---------------------------------------------------------------------------
// x-routectl-alias header overrides wire model on /v1/chat/completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn x_routectl_alias_header_overrides_model_on_chat_completions() {
    // Mirror of `x_routectl_alias_header_overrides_aliases_map` from the
    // Anthropic-ingress tests. Two model entries on the same wiremock,
    // distinguished by upstream wire-model id. The wire-string alias maps
    // `claude-opus-4-7-20251022` -> `opus-oc` (which resolves to
    // `gpt-4o-NEVER-CALLED`). The `x-routectl-alias: heavy` header MUST
    // take precedence and route to `haiku-oc` -> `gpt-4o-haiku`.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_response_body("gpt-4o-haiku", "alias answer")),
        )
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "mock-oc".to_string(),
        ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
    );

    let mut models = BTreeMap::new();
    models.insert(
        "haiku-oc".to_string(),
        ModelEntry::new("mock-oc", "gpt-4o-haiku"),
    );
    models.insert(
        "opus-oc".to_string(),
        ModelEntry::new("mock-oc", "gpt-4o-NEVER-CALLED"),
    );

    let mut aliases = BTreeMap::new();
    // Wire-string alias: body `model` value points here and would route
    // to `gpt-4o-NEVER-CALLED` without the header override.
    aliases.insert(
        "claude-opus-4-7-20251022".to_string(),
        AliasValue::Single("opus-oc".into()),
    );
    // Named alias: the `x-routectl-alias` header points here and must win.
    aliases.insert("heavy".to_string(), AliasValue::Single("haiku-oc".into()));

    let config = Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    });

    let base = helpers::spawn_test_server(config).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-routectl-alias", "heavy")
        .json(&json!({
            "model": "claude-opus-4-7-20251022",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.expect("requests");
    let body: Value =
        serde_json::from_slice(&received[0].body).expect("upstream body parses as JSON");
    assert_eq!(
        body["model"].as_str(),
        Some("gpt-4o-haiku"),
        "header alias `heavy` -> `haiku-oc` should set upstream model to gpt-4o-haiku; \
         got {:?}",
        body["model"],
    );
}

// ---------------------------------------------------------------------------
// MITM anti-drift guard: `proxy::split::ANTHROPIC_INFERENCE_PATHS` is the
// single source of truth for which routes the MITM front-proxy classifies
// as Anthropic-dialect inference traffic. These two tests pin (a) every
// path in the const is actually served by `build_axum_router` (no 404s),
// and (b) the const equals its exact expected literal set -- so a change to
// either side is a deliberate, reviewed edit rather than silent drift.
// ---------------------------------------------------------------------------

#[test]
fn anthropic_inference_paths_matches_expected_literal_set() {
    let expected: &[&str] = &["/v1/messages", "/v1/messages/count_tokens", "/v1/models"];
    assert_eq!(
        routectl_cli::proxy::split::anthropic_inference_paths(),
        expected,
        "ANTHROPIC_INFERENCE_PATHS changed -- this must be a deliberate, reviewed edit \
         (the MITM split classifier depends on this exact set)"
    );
}

#[tokio::test]
async fn anthropic_inference_paths_are_all_served_by_build_axum_router() {
    let config = openai_compat_config("http://127.0.0.1:1", "provider1", "any-alias");
    let base = helpers::spawn_test_server(config).await;
    let client = reqwest::Client::new();

    for inference_path in routectl_cli::proxy::split::anthropic_inference_paths() {
        let url = format!("{base}{inference_path}");
        let resp = if *inference_path == "/v1/models" {
            client.get(&url).send().await.unwrap()
        } else {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await
                .unwrap()
        };
        assert_ne!(
            resp.status(),
            404,
            "path {inference_path} must be served by build_axum_router -- a 404 here means \
             ANTHROPIC_INFERENCE_PATHS has drifted from the routes routectl actually serves"
        );
    }
}

// ---------------------------------------------------------------------------
// Status subtree wiring: auth-exemption + host-allowlist scoping (ui.f1.10).
// ---------------------------------------------------------------------------

/// A config whose ingress `/v1/*` surface requires a listener token, so the
/// auth-exemption of `/status*` is observable against a live auth wall.
fn config_with_listener_auth(token: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", common::file_ref("test-key")),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));

    Arc::new(Config {
        server: ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec![common::file_ref(token)],
            }),
            ..ServerConfig::default()
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

const STATUS_PATHS: &[&str] = &[
    "/status",
    "/status/usage",
    "/status/health",
    "/status/config",
    "/status/doctor",
];

#[tokio::test]
async fn status_subtree_is_auth_exempt_while_v1_still_requires_a_token() {
    let base = helpers::spawn_test_server(config_with_listener_auth("secret-token")).await;
    let client = reqwest::Client::new();

    // Every /status path is reachable WITHOUT a token even though auth is on.
    for path in STATUS_PATHS {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(
            resp.status(),
            200,
            "{path} must be reachable without a token (status is auth-exempt)"
        );
    }

    // The dashboard shell at `GET /` shares the status surface's auth-exemption
    // (public-like-/health): served token-less even with auth configured.
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "GET / (dashboard shell) must be reachable without a token"
    );

    // A /v1 route still rejects an unauthenticated request.
    let resp = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "/v1/* must still require a token when auth is configured"
    );
}

#[tokio::test]
async fn host_allowlist_rejects_status_but_not_v1() {
    let base = helpers::spawn_test_server(openai_compat_config(
        "http://127.0.0.1:1",
        "provider1",
        "my-alias",
    ))
    .await;
    let client = reqwest::Client::new();

    // A disallowed Host to a status path is rejected by the subtree-only guard.
    let resp = client
        .get(format!("{base}/status/health"))
        .header(reqwest::header::HOST, "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "a disallowed Host to /status must be rejected"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden_host");

    // The SAME disallowed Host to a /v1 route is NOT rejected by this layer
    // (the proxy lane does not carry the host allowlist).
    let resp = client
        .get(format!("{base}/v1/models"))
        .header(reqwest::header::HOST, "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        403,
        "the host allowlist must not apply to /v1/*"
    );

    // The default (loopback) Host is allowed on the status subtree.
    let resp = client
        .get(format!("{base}/status/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "loopback Host must be allowed on /status"
    );

    // The dashboard shell at `GET /` inherits the SAME host allowlist as the
    // JSON: an off-allowlist Host is turned away at page load, while `/v1/*`
    // (which never carries the guard) is unaffected, and the default loopback
    // Host serves the shell.
    let resp = client
        .get(format!("{base}/"))
        .header(reqwest::header::HOST, "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "a disallowed Host to GET / must be rejected"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden_host");

    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "loopback Host must be allowed on GET /");
}
