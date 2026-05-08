//! Integration tests for the Anthropic Messages ingress
//! (`POST /v1/messages`). End-to-end through the axum server +
//! a wiremock upstream that pretends to be `api.anthropic.com`.
//!
//! What's covered:
//!   - Body translation: cache_control on every position (top-level,
//!     system blocks, tool defs, message content blocks) reaches
//!     upstream byte-for-byte.
//!   - Forward-compat: an unknown content block type passes through
//!     verbatim.
//!   - Anthropic_beta body-level array round-trips.
//!   - Listener auth: `[server.auth].tokens` enforced on both
//!     `x-api-key` and `Authorization: Bearer`.
//!   - Ingress aliases: configured `[ingress.anthropic.aliases]` map
//!     a wire model to a routectl alias; `x-routectl-alias` header
//!     overrides.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{
    AliasEntry, Config, IngressConfig, IngressShape, ProviderEntry, RetryPolicy, ServerAuth,
    ServerConfig,
};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
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

/// Build a config that points an alias to an upstream wiremock acting
/// as `api.anthropic.com`. Optional listener auth + ingress aliases.
fn anthropic_proxy_config(
    upstream_base: &str,
    auth_tokens: Option<Vec<String>>,
    anthropic_alias_map: BTreeMap<String, String>,
) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_base.to_string()),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasEntry::new(vec!["anthropic-mock:claude-haiku-4-5".to_string()]),
    );

    let server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: auth_tokens.map(|tokens| ServerAuth { tokens }),
        strict_translation: false,
    };

    let ingress = IngressConfig {
        anthropic: IngressShape {
            aliases: anthropic_alias_map,
        },
        openai: IngressShape::default(),
    };

    Arc::new(Config {
        server,
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        ingress,
    })
}

// ---------------------------------------------------------------------------
// cache_control round-trip on every position
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_control_on_every_position_reaches_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 1024,
        "anthropic_beta": ["context-1m-2025-08-07"],
        "system": [{
            "type": "text",
            "text": "you are helpful",
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }],
        "tools": [{
            "name": "lookup",
            "description": "look up docs",
            "input_schema": {"type": "object"},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }],
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "look at this",
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            }]
        }]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Inspect what wiremock saw.
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();

    assert_eq!(
        upstream_body["anthropic_beta"],
        json!(["context-1m-2025-08-07"])
    );
    assert_eq!(upstream_body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(upstream_body["tools"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(upstream_body["tools"][0]["name"], "lookup");
    assert_eq!(
        upstream_body["messages"][0]["content"][0]["cache_control"]["ttl"],
        "5m"
    );
    assert_eq!(
        upstream_body["messages"][0]["content"][0]["text"],
        "look at this"
    );
}

// ---------------------------------------------------------------------------
// Forward-compat: unknown block type passes through
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_block_type_round_trips_to_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "server_tool_use",
                "id": "srvtu_01",
                "name": "web_search",
                "input": {"query": "rust"}
            }]
        }]
    });

    reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();

    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    let block = &up["messages"][0]["content"][0];
    assert_eq!(block["type"], "server_tool_use");
    assert_eq!(block["id"], "srvtu_01");
    assert_eq!(block["input"]["query"], "rust");
}

// ---------------------------------------------------------------------------
// 4-breakpoint cap and TTL ordering rejected at ingress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn five_breakpoints_rejected_with_400() {
    let upstream = MockServer::start().await;
    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "d", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "e", "cache_control": {"type": "ephemeral"}}
            ]
        }]
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let envelope: Value = resp.json().await.unwrap();
    assert_eq!(envelope["error"]["type"], "validation_error");
}

// ---------------------------------------------------------------------------
// Listener auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_accepts_x_api_key() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(
        &upstream.uri(),
        Some(vec!["literal:sk-routectl-good".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-routectl-good")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn auth_accepts_authorization_bearer() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(
        &upstream.uri(),
        Some(vec!["literal:sk-routectl-good".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("authorization", "Bearer sk-routectl-good")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn auth_rejects_bogus_token() {
    let config = anthropic_proxy_config(
        "http://127.0.0.1:1",
        Some(vec!["literal:sk-real".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-bad")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let envelope: Value = resp.json().await.unwrap();
    assert_eq!(envelope["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn auth_rejects_missing_credentials_when_required() {
    let config = anthropic_proxy_config(
        "http://127.0.0.1:1",
        Some(vec!["literal:sk-real".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "heavy",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

// ---------------------------------------------------------------------------
// Ingress aliases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ingress_aliases_resolve_wire_model_to_alias() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let mut alias_map = BTreeMap::new();
    alias_map.insert("claude-opus-4-7-20251022".into(), "heavy".into());

    let config = anthropic_proxy_config(&upstream.uri(), None, alias_map);
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "claude-opus-4-7-20251022",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    // 200 means the alias resolved + provider was hit successfully.
    // (If the alias had not resolved we'd get an UnknownAlias 404.)
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn x_routectl_alias_header_overrides_aliases_map() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    // Alias map has a different mapping; the header should win.
    let mut alias_map = BTreeMap::new();
    alias_map.insert("claude-opus-4-7-20251022".into(), "nonexistent".into());

    let config = anthropic_proxy_config(&upstream.uri(), None, alias_map);
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-routectl-alias", "heavy")
        .json(&json!({
            "model": "claude-opus-4-7-20251022",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "header should have overridden the alias map; got status {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// auth + token-via-Authorization-Bearer end-to-end (Claude Code shape)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn header_used_for_listener_auth_does_not_pollute_upstream() {
    // Defense-in-depth: when the listener accepts a token via
    // x-api-key, the upstream provider must be called with the
    // provider-configured key, NOT the listener-side token.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(
        &upstream.uri(),
        Some(vec!["literal:sk-routectl".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-routectl")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "expected upstream to see provider-configured key only"
    );
}
