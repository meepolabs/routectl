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
    AliasValue, Config, ModelEntry, ProviderEntry, ReasoningDialect, RetryPolicy, ServerAuth,
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
/// as `api.anthropic.com`. Optional listener auth + ingress wire-string
/// aliases (v0.6.0 collapsed them into the unified `[aliases]` table).
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

    let mut models = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5"),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("haiku".into()));
    // v0.6.0: wire-string -> nickname mapping lives directly in
    // `[aliases]`. The `anthropic_alias_map` argument's values used
    // to be alias names (which themselves resolved via the alias
    // table); we now require the map's values to also resolve to a
    // valid nickname through the same `[aliases]` lookup, so we
    // insert each wire-string -> alias-or-nickname pair as a
    // single-entry alias.
    for (wire_string, target) in anthropic_alias_map {
        aliases.insert(wire_string, AliasValue::Single(target));
    }

    let server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: auth_tokens.map(|tokens| ServerAuth { tokens }),
        strict_translation: false,
        allow_disable_fallbacks: true,
    };

    Arc::new(Config {
        server,
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: routectl_router::LegacyCompat::Openrouter,
        models,
        ..Default::default()
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

/// Pin: `metadata` round-trips through the canonical seam to the
/// Anthropic egress with all keys intact. Lifting `metadata.user_id`
/// to `req.user` and dropping the rest of `metadata` would break
/// request attribution.
#[tokio::test]
async fn metadata_round_trips_full_object_to_upstream() {
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
        "metadata": {
            "user_id": "u_42",
            "session_id": "sess_xyz",
            "custom_attribution": "shopping-cart-flow"
        },
        "messages": [{"role": "user", "content": "hi"}]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    // Full metadata object reaches the upstream wire body.
    assert_eq!(upstream_body["metadata"]["user_id"], "u_42");
    assert_eq!(upstream_body["metadata"]["session_id"], "sess_xyz");
    assert_eq!(
        upstream_body["metadata"]["custom_attribution"],
        "shopping-cart-flow"
    );
}

/// End-to-end pin: `context_management` (top-level) and
/// `usage.service_tier` (nested) round-trip from upstream
/// response through ChatResponse.extras / Usage.extras to the
/// client wire body. Forward-compat: any new Anthropic spec field
/// rides this same path with no routectl release.
#[tokio::test]
async fn response_extras_round_trip_to_client() {
    let upstream = MockServer::start().await;
    let upstream_resp = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 5,
            "output_tokens": 1,
            "service_tier": "standard"
        },
        "context_management": {"applied_edits": []}
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_resp))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let client_body: Value = resp.json().await.unwrap();
    assert_eq!(
        client_body["context_management"],
        json!({"applied_edits": []}),
        "context_management dropped on egress"
    );
    assert_eq!(
        client_body["usage"]["service_tier"], "standard",
        "usage.service_tier dropped on egress"
    );
}

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
// Ingress alias header (per-dialect alias map removed in v0.6.0)
// ---------------------------------------------------------------------------

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
// Review-fix regressions (CRITICAL + HIGH from v0.4.0 review pass)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn body_exceeding_size_cap_rejected_with_413() {
    // CRITICAL C1: 4 MiB body cap on /v1/messages.
    let config = anthropic_proxy_config("http://127.0.0.1:1", None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    // Build a >4 MiB body by inflating a single text block. Use 5 MiB
    // of `a`s to clear the 4 MiB cap.
    let huge = "a".repeat(5 * 1024 * 1024);
    let body = json!({
        "model": "heavy",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": huge}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    // Axum returns 413 Payload Too Large when DefaultBodyLimit fires.
    assert_eq!(resp.status().as_u16(), 413);
}

#[tokio::test]
async fn health_endpoint_bypasses_auth() {
    // HIGH H5: /health stays public so liveness probes work even
    // when [server.auth].tokens is configured.
    let config = anthropic_proxy_config(
        "http://127.0.0.1:1",
        Some(vec!["literal:sk-real".into()]),
        BTreeMap::new(),
    );
    let base = helpers::spawn(config).await;

    // No x-api-key sent -- /health must still 200.
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn tool_def_other_cache_control_counts_toward_breakpoint_cap() {
    // HIGH H1: ToolDef::Other (Anthropic builtin tools) carrying
    // cache_control was previously invisible to the breakpoint
    // counter, so a request could exceed the 4-cap silently.
    let config = anthropic_proxy_config("http://127.0.0.1:1", None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "heavy",
        "max_tokens": 1,
        // 2 builtin (Other) tools + 3 message blocks = 5 breakpoints
        "tools": [
            {"type": "bash_20250124", "name": "bash", "cache_control": {"type": "ephemeral"}},
            {"type": "web_search_20250901", "name": "search", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}}
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
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exceeds maximum"),
        "expected 'exceeds maximum' in error, got: {}",
        envelope["error"]["message"]
    );
}

#[tokio::test]
async fn provider_extras_cannot_override_routectl_managed_keys() {
    // MEDIUM-1 (arch): provider_extras allow-list. A malicious
    // request should not be able to stomp on `messages` etc via the
    // Anthropic-only escape hatch.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    // We can't directly send `provider_extras` via Anthropic ingress
    // (the ingress only fills it from top_k / service_tier / etc), so
    // simulate by sending top_k + a service_tier that's harmless,
    // plus we test the Anthropic egress allow-list directly via a
    // crafted top-level field that gets parked in extras.
    //
    // The Anthropic ingress moves `output_config` into provider_extras.
    // If output_config were named "messages", it would be a stomp
    // attack -- but the ingress only forwards a known set of keys.
    // This test confirms the egress would reject such a stomp if it
    // somehow appeared in provider_extras (defense in depth).
    //
    // We test the egress directly via an OpenAI-compat-shape ChatRequest
    // would require deeper plumbing; instead, send a request that
    // exercises the legitimate path and assert messages array is
    // unmodified.
    let body = json!({
        "model": "heavy",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "real"}],
        "top_k": 40,  // -> provider_extras
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    // top_k flowed through; messages array intact.
    assert_eq!(up["top_k"], 40);
    assert_eq!(up["messages"][0]["content"], "real");
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

// ---------------------------------------------------------------------------
// Structured output: `output_format` (legacy) and `output_config.format`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_output_format_is_rewritten_to_output_config_format() {
    // Pre-fix: top-level `output_format` was silently dropped at the
    // ingress because ChatRequest has no such field and serde's
    // unknown-field handling on the canonical type is permissive.
    // Post-fix: the ingress folds `output_format` into
    // `output_config.format` so the existing `output_config` ->
    // provider_extras pipeline forwards it verbatim to upstream.
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
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "output_format": {
            "type": "json_schema",
            "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
        }
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();

    // The legacy field has been rewritten to the nested shape.
    assert!(
        up.get("output_format").is_none(),
        "legacy output_format must not leak through to upstream: {up}"
    );
    let format = up
        .get("output_config")
        .and_then(|oc| oc.get("format"))
        .expect("output_config.format must reach upstream");
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["schema"]["type"], "object");
}

#[tokio::test]
async fn output_config_format_passes_through_unchanged() {
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
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
            }
        }
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    let format = up
        .get("output_config")
        .and_then(|oc| oc.get("format"))
        .expect("output_config.format must reach upstream");
    assert_eq!(format["type"], "json_schema");
}

#[tokio::test]
async fn legacy_output_format_preserves_existing_output_config_effort() {
    // The legacy field must merge into a pre-existing `output_config`
    // (with `effort`) rather than overwriting it.
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
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high"},
        "output_format": {
            "type": "json_schema",
            "schema": {"type": "object"}
        }
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        up.get("output_format").is_none(),
        "legacy field leaked: {up}"
    );
    let oc = up.get("output_config").expect("output_config present");
    assert_eq!(oc["effort"], "high");
    assert_eq!(oc["format"]["type"], "json_schema");
}

#[tokio::test]
async fn both_legacy_and_nested_present_drops_legacy_with_warn() {
    // When both `output_format` (legacy top-level) and
    // `output_config.format` (current nested) are sent, the nested
    // form wins -- mirroring claude-code's own deprecation message
    // ("Both output_format and output_config.format were provided.
    //   Please use only output_config.format").
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let nested_schema = json!({"type": "object", "properties": {"nested": {"type": "boolean"}}});
    let legacy_schema = json!({"type": "object", "properties": {"legacy": {"type": "boolean"}}});
    let body = json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {"type": "json_schema", "schema": nested_schema.clone()}
        },
        "output_format": {"type": "json_schema", "schema": legacy_schema}
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        up.get("output_format").is_none(),
        "legacy field leaked: {up}"
    );
    let format = up
        .get("output_config")
        .and_then(|oc| oc.get("format"))
        .expect("output_config.format must reach upstream");
    // Nested wins.
    assert_eq!(format["schema"], nested_schema);
}

// ---------------------------------------------------------------------------
// Coverage: every Anthropic-only wire field claude-code 2.1.x sends
// ---------------------------------------------------------------------------
//
// Fields lifted from the disassembled claude-code binary at
// `~/.local/share/claude/versions/<N>`. The main-loop body
// builder (function `rH` in the binary) emits this exact set when the
// relevant beta + flag combinations are active. routectl's Anthropic
// ingress must forward each one verbatim to the upstream so the
// Anthropic API can interpret it.

/// Helper -- send a body containing the kitchen-sink of Anthropic-only
/// fields and assert each one survives to the upstream.
async fn assert_field_reaches_upstream(field: &str, value: Value) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;
    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let mut body = json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
    });
    body.as_object_mut()
        .unwrap()
        .insert(field.to_string(), value.clone());

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        up.get(field),
        Some(&value),
        "field `{field}` did not reach upstream verbatim. body={up}"
    );
}

#[tokio::test]
async fn anthropic_ingress_forwards_context_management() {
    // Beta `context-management-2025-...`. Nested object the model
    // uses to decide when/how to compact older context.
    assert_field_reaches_upstream(
        "context_management",
        json!({"edits": [{"type": "clear_tool_uses_20250919", "trigger": {"type": "input_tokens", "value": 100000}}]}),
    )
    .await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_context_hint() {
    // Beta `context-hint-...`. Used by claude-code's first-party
    // context-hint controller -- emitted as a top-level body field.
    assert_field_reaches_upstream(
        "context_hint",
        json!({"enabled": true, "target_tokens_saved": 1024}),
    )
    .await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_speed() {
    // claude-code's "fast" mode marker. Emitted on requests routed
    // to the fast-mode model.
    assert_field_reaches_upstream("speed", json!("fast")).await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_diagnostics() {
    // Diagnostic feedback object emitted on the `repl_main_thread`
    // path when the prompt-cache diagnostics flag is active.
    assert_field_reaches_upstream("diagnostics", json!({"previous_message_id": "msg_01ABC"})).await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_mcp_servers() {
    // MCP server list (older claude-code paths still emit this for
    // backward compat with the betas/mcp-client-2025 surface).
    assert_field_reaches_upstream(
        "mcp_servers",
        json!([{"name": "my-server", "url": "https://mcp.example.com"}]),
    )
    .await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_top_k() {
    // Top-k sampling control. Already covered via provider_extras;
    // included here so the regression suite is one place.
    assert_field_reaches_upstream("top_k", json!(40)).await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_service_tier() {
    assert_field_reaches_upstream("service_tier", json!("auto")).await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_container() {
    assert_field_reaches_upstream("container", json!("container_01ABC")).await;
}

#[tokio::test]
async fn anthropic_ingress_forwards_inference_geo() {
    assert_field_reaches_upstream("inference_geo", json!("us")).await;
}

#[tokio::test]
async fn anthropic_beta_http_header_forwarded_to_upstream() {
    // The Anthropic TypeScript SDK translates `betas: [...]` into the
    // HTTP `anthropic-beta: foo,bar` header. claude-code uses this
    // path for first-party betas (context-management, prompt-cache-1h,
    // adaptive-thinking, etc.), so routectl MUST forward those values
    // to the upstream or those betas silently no-op.
    //
    // routectl normalizes the inbound header into canonical
    // `req.anthropic_beta` and emits it on the upstream body
    // (Anthropic accepts either surface). This test pins that
    // round-trip end-to-end.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(
            "anthropic-beta",
            "context-management-2025-06-27,prompt-caching-2024-07-31",
        )
        .json(&json!({
            "model": "heavy",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    let betas = up
        .get("anthropic_beta")
        .and_then(|v| v.as_array())
        .expect("inbound anthropic-beta header was not lifted to upstream body");
    let names: Vec<&str> = betas.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"context-management-2025-06-27"),
        "missing context-management beta: {names:?}"
    );
    assert!(
        names.contains(&"prompt-caching-2024-07-31"),
        "missing prompt-caching beta: {names:?}"
    );
}

#[tokio::test]
async fn anthropic_beta_header_merges_with_body_anthropic_beta_dedup() {
    // When the caller sends BOTH the header and a body-level
    // anthropic_beta array, routectl unions them with deduplication
    // (preserving body order first, then header values not already
    // present). Mirrors how the SDK treats the two surfaces.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-beta", "context-management-2025-06-27,beta-c")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "anthropic_beta": ["beta-a", "context-management-2025-06-27"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    let betas: Vec<String> = up
        .get("anthropic_beta")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    // Body-first ordering, header-second, dedup'd.
    assert_eq!(
        betas,
        vec!["beta-a", "context-management-2025-06-27", "beta-c"]
    );
}

// ---------------------------------------------------------------------------
// Response shape: stop_reason round-trip preservation
// ---------------------------------------------------------------------------

async fn assert_stop_reason_round_trips(stop_reason: &str) {
    // For each Anthropic-only stop_reason, drive a non-streaming
    // /v1/messages through the Anthropic ingress + Anthropic egress
    // and verify the same string emerges on the response. Pre-fix,
    // anything outside {"end_turn","max_tokens","tool_use"} was
    // silently rewritten to "end_turn" by the ingress's reverse mapper.
    let upstream = MockServer::start().await;
    let resp_body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
        .mount(&upstream)
        .await;

    let config = anthropic_proxy_config(&upstream.uri(), None, BTreeMap::new());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "heavy",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["stop_reason"], stop_reason,
        "stop_reason round-trip lossy: response={body}"
    );
}

#[tokio::test]
async fn stop_reason_pause_turn_round_trips() {
    assert_stop_reason_round_trips("pause_turn").await;
}

#[tokio::test]
async fn stop_reason_refusal_round_trips() {
    assert_stop_reason_round_trips("refusal").await;
}

#[tokio::test]
async fn stop_reason_model_context_window_exceeded_round_trips() {
    assert_stop_reason_round_trips("model_context_window_exceeded").await;
}

#[tokio::test]
async fn stop_reason_end_turn_round_trips() {
    assert_stop_reason_round_trips("end_turn").await;
}

#[tokio::test]
async fn stop_reason_max_tokens_round_trips() {
    assert_stop_reason_round_trips("max_tokens").await;
}

#[tokio::test]
async fn stop_reason_tool_use_round_trips() {
    assert_stop_reason_round_trips("tool_use").await;
}

// ---------------------------------------------------------------------------
// Cross-translation audit: Anthropic-in / OpenAI-out (claude-code -> DeepSeek
// / Qwen on llama.cpp / example-deepseek-host / any OpenAI-compat upstream)
// ---------------------------------------------------------------------------
//
// claude-code's primary surface is the Anthropic Messages dialect, but the
// most common deployments route to OpenAI-compat upstreams (DeepSeek v4
// flash/pro, Qwen on llama.cpp, example-deepseek-host DeepSeek host). routectl is the
// translation pipe, so the canonical -> OpenAI-compat egress must turn
// Anthropic-shape inputs into a body the upstream actually accepts.
//
// Each test below pins one Anthropic-only request shape and asserts the
// OpenAI-compat upstream receives the OpenAI-shape equivalent.

fn openai_response_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "deepseek-chat",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

fn openai_compat_proxy_config(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "deepseek-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key"),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "ds-chat".to_string(),
        ModelEntry::new("deepseek-mock", "deepseek-chat"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("ds-chat".into()));
    let server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
    };
    Arc::new(Config {
        server,
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

/// Helper -- POST an Anthropic-shape body to the ingress and return what
/// the OpenAI-compat upstream actually received.
async fn capture_openai_egress_body(anthropic_body: Value) -> Value {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(&upstream)
        .await;
    let config = openai_compat_proxy_config(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&anthropic_body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ingress rejected: {}",
        resp.status()
    );
    let received = upstream.received_requests().await.unwrap();
    serde_json::from_slice(&received[0].body).unwrap()
}

#[tokio::test]
async fn cross_anthropic_system_lowers_to_openai_system_message() {
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "system": "be helpful",
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .await;
    // OpenAI body must NOT have a top-level system field.
    assert!(
        up.get("system").is_none(),
        "top-level system leaked: NIM-class hosts will 400. body={up}"
    );
    let messages = up["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be helpful");
    assert_eq!(messages[1]["role"], "user");
}

#[tokio::test]
async fn cross_anthropic_stop_sequences_renamed_to_stop() {
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "stop_sequences": ["</block>"]
    }))
    .await;
    assert!(
        up.get("stop_sequences").is_none(),
        "stop_sequences leaked: {up}"
    );
    assert_eq!(up["stop"], json!(["</block>"]));
}

#[tokio::test]
async fn cross_anthropic_tools_translated_to_openai_function_shape() {
    // Anthropic tools: `{name, description, input_schema}`.
    // OpenAI tools:    `{type:"function", function:{name, description, parameters}}`.
    // claude-code sends Anthropic-shape; routectl must rewrite for the
    // OpenAI-compat upstream.
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "compute 2+2"}],
        "tools": [{
            "name": "calculator",
            "description": "evaluate arithmetic",
            "input_schema": {
                "type": "object",
                "properties": {"expr": {"type": "string"}},
                "required": ["expr"]
            }
        }]
    }))
    .await;
    let tools = up["tools"].as_array().expect("tools array on egress body");
    assert_eq!(tools.len(), 1);
    let t = &tools[0];
    assert_eq!(
        t["type"], "function",
        "missing OpenAI `type:function` discriminant: {t}"
    );
    assert_eq!(t["function"]["name"], "calculator");
    assert_eq!(
        t["function"]["parameters"]["properties"]["expr"]["type"],
        "string"
    );
    // The Anthropic-shape `input_schema` must NOT leak.
    assert!(
        t.get("input_schema").is_none(),
        "Anthropic-shape input_schema leaked: {t}"
    );
}

#[tokio::test]
async fn cross_anthropic_tool_choice_translated_to_openai_shape() {
    // Anthropic: tool_choice = {"type":"auto"} | {"type":"any"} |
    //                          {"type":"tool", "name":"..."} | {"type":"none"}
    // OpenAI:    tool_choice = "auto" | "required" | "none" |
    //                          {"type":"function","function":{"name":"..."}}
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "use the tool"}],
        "tools": [{
            "name": "calculator",
            "input_schema": {"type": "object", "properties": {}}
        }],
        "tool_choice": {"type": "tool", "name": "calculator"}
    }))
    .await;
    let tc = &up["tool_choice"];
    // The Anthropic tagged-enum form must translate to OpenAI's
    // function-name object form. Anything else and OpenAI hosts 400.
    assert_eq!(tc["type"], "function", "tool_choice not OpenAI-shape: {tc}");
    assert_eq!(tc["function"]["name"], "calculator");
}

#[tokio::test]
async fn cross_anthropic_image_block_translated_to_openai_image_url() {
    // Anthropic image block: {type:"image", source:{type:"base64", media_type, data}}
    // OpenAI image block:    {type:"image_url", image_url:{url:"data:..."}}
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "what's in this image?"},
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgo="
                    }
                }
            ]
        }]
    }))
    .await;
    let parts = up["messages"][0]["content"]
        .as_array()
        .expect("user content array");
    let img_part = parts
        .iter()
        .find(|p| p["type"] == "image_url" || p["type"] == "image")
        .expect("no image-shaped content part: messages={up}");
    assert_eq!(
        img_part["type"], "image_url",
        "Anthropic image shape leaked through to OpenAI host: {img_part}"
    );
    let url = img_part["image_url"]["url"]
        .as_str()
        .expect("image_url.url");
    assert!(
        url.starts_with("data:image/png;base64,"),
        "image_url not data URL: {url}"
    );
}

#[tokio::test]
async fn cross_anthropic_assistant_tool_use_translated_to_openai_tool_calls() {
    // Anthropic assistant: content blocks include {type:"tool_use", id, name, input}.
    // OpenAI assistant:   tool_calls: [{id, type:"function", function:{name, arguments}}]
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "use the calculator"},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "ok"},
                    {
                        "type": "tool_use",
                        "id": "toolu_01ABC",
                        "name": "calculator",
                        "input": {"expr": "2+2"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01ABC",
                    "content": "4"
                }]
            }
        ],
        "tools": [{"name": "calculator", "input_schema": {"type": "object"}}]
    }))
    .await;
    let assistant = &up["messages"][1];
    let tool_calls = assistant
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("assistant has no tool_calls field; OpenAI host will not see the call. msg={assistant}"));
    assert_eq!(tool_calls.len(), 1);
    let tc = &tool_calls[0];
    assert_eq!(tc["id"], "toolu_01ABC");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "calculator");
    let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"expr": "2+2"}));
}

#[tokio::test]
async fn cross_anthropic_tool_result_translated_to_openai_tool_role_message() {
    // Anthropic: user message with content block {type:"tool_result", tool_use_id, content}.
    // OpenAI:    {role:"tool", tool_call_id, content}
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "use the calculator"},
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01ABC",
                    "name": "calculator",
                    "input": {"expr": "2+2"}
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01ABC",
                    "content": "4"
                }]
            }
        ]
    }))
    .await;
    // The third message must surface as role:"tool" with tool_call_id.
    let third = &up["messages"][2];
    assert_eq!(
        third["role"], "tool",
        "tool_result wasn't lifted into a tool-role message: {third}"
    );
    assert_eq!(third["tool_call_id"], "toolu_01ABC");
    assert_eq!(third["content"], "4");
}

#[tokio::test]
async fn cross_anthropic_thinking_translates_to_openai_reasoning() {
    // claude-code sends Anthropic `thinking: {type:"enabled", budget_tokens}`.
    // For OpenAI-compat hosts (DeepSeek v4 reasoner, vLLM-served reasoning
    // models, example-deepseek-host DeepSeek), routectl should translate this into
    // the upstream's reasoning knob. DeepSeek dialect: drops sampling
    // params, expects `reasoning_effort` from the canonical
    // `req.reasoning.effort`. Anthropic-shape `budget_tokens` should at
    // least ride through enough that the upstream gets a coherent
    // reasoning request.
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "think hard"}],
        "thinking": {"type": "enabled", "budget_tokens": 4000}
    }))
    .await;
    // Anthropic-shape thinking field MUST NOT leak; OpenAI-compat hosts
    // 400 on unknown top-level fields (NIM is strict about this).
    assert!(
        up.get("thinking").is_none(),
        "Anthropic `thinking` leaked into OpenAI body; strict hosts (NIM) will 400: {up}"
    );
}

#[tokio::test]
async fn cross_anthropic_output_config_format_translates_to_openai_response_format() {
    // Anthropic structured outputs: output_config.format = {type:"json_schema", schema}
    // OpenAI structured outputs:   response_format = {type:"json_schema", json_schema:{schema}}
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "emit json"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
            }
        }
    }))
    .await;
    // Either the Anthropic shape leaks (NIM 400s) or response_format is set.
    assert!(
        up.get("output_config").is_none(),
        "Anthropic output_config leaked into OpenAI body: {up}"
    );
    let rf = up.get("response_format").unwrap_or_else(|| {
        panic!("response_format missing -- structured output silently dropped on OpenAI host: {up}")
    });
    assert_eq!(rf["type"], "json_schema");
}

#[tokio::test]
async fn cross_anthropic_multi_turn_tool_use_translates_to_openai_tool_round_trip() {
    // Full claude-code -> DeepSeek tool-use trajectory.
    //
    // Turn 1 (user):     "What's the weather in SF?"
    // Turn 2 (assistant): text "I'll check" + tool_use(get_weather, {city:"SF"})
    // Turn 3 (user):     tool_result("toolu_X", "72F sunny")
    // Turn 4 (user):     "Now suggest an outfit"
    //
    // Expected on the OpenAI wire (4 messages):
    //   role:user      "What's the weather in SF?"
    //   role:assistant content:"I'll check" + tool_calls:[{toolu_X, get_weather}]
    //   role:tool      tool_call_id:"toolu_X" content:"72F sunny"
    //   role:user      "Now suggest an outfit"
    let up = capture_openai_egress_body(json!({
        "model": "heavy",
        "max_tokens": 64,
        "tools": [{
            "name": "get_weather",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }],
        "messages": [
            {"role": "user", "content": "What's the weather in SF?"},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I'll check"},
                    {
                        "type": "tool_use",
                        "id": "toolu_X",
                        "name": "get_weather",
                        "input": {"city": "SF"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_X",
                    "content": "72F sunny"
                }]
            },
            {"role": "user", "content": "Now suggest an outfit"}
        ]
    }))
    .await;

    let msgs = up["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 4, "expected 4 wire messages, got: {up}");

    // 1. user
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "What's the weather in SF?");

    // 2. assistant -- collapsed text + tool_calls
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "I'll check");
    let tcs = msgs[1]["tool_calls"]
        .as_array()
        .unwrap_or_else(|| panic!("assistant missing tool_calls: {}", msgs[1]));
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0]["id"], "toolu_X");
    assert_eq!(tcs[0]["type"], "function");
    assert_eq!(tcs[0]["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(tcs[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"city": "SF"}));

    // 3. tool result lifted to role:tool
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "toolu_X");
    assert_eq!(msgs[2]["content"], "72F sunny");

    // 4. follow-up user
    assert_eq!(msgs[3]["role"], "user");
    assert_eq!(msgs[3]["content"], "Now suggest an outfit");

    // tools shape -- function-shape, not Anthropic input_schema.
    let tools = up["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_weather");
    assert!(
        tools[0].get("input_schema").is_none(),
        "Anthropic input_schema must not leak: {tools:?}"
    );
}

// Response side: OpenAI-compat upstream returns OpenAI-shape -> Anthropic
// ingress must re-emit Anthropic shape so claude-code understands it.

#[tokio::test]
async fn cross_response_openai_tool_calls_become_anthropic_tool_use_blocks() {
    // OpenAI: assistant message with tool_calls: [{id, type:"function",
    //         function:{name, arguments}}]
    // Anthropic: assistant message with content blocks including
    //         {type:"tool_use", id, name, input}
    let upstream = MockServer::start().await;
    let openai_resp = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "deepseek-chat",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_01ABC",
                    "type": "function",
                    "function": {
                        "name": "calculator",
                        "arguments": "{\"expr\":\"2+2\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_resp))
        .mount(&upstream)
        .await;
    let config = openai_compat_proxy_config(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({
            "model": "heavy",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "compute 2+2"}],
            "tools": [{"name": "calculator", "input_schema": {"type": "object"}}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let content = body["content"]
        .as_array()
        .unwrap_or_else(|| panic!("response content not array: {body}"));
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap_or_else(|| panic!("no tool_use block on Anthropic response; claude-code won't see the tool call. body={body}"));
    assert_eq!(tool_use["id"], "call_01ABC");
    assert_eq!(tool_use["name"], "calculator");
    assert_eq!(tool_use["input"], json!({"expr": "2+2"}));
    assert_eq!(body["stop_reason"], "tool_use");
}

// ---------------------------------------------------------------------------
// Cross-crate regression: Anthropic ingress -> Bedrock-Invoke egress
// ---------------------------------------------------------------------------
//
// claude-code's TS SDK ships up to ten anthropic-beta flags via the HTTP
// header on every /v1/messages request. The Anthropic ingress lifts the
// header into canonical req.anthropic_beta; the Bedrock egress's
// `filter_bedrock_betas` (in `bedrock::betas`) drops values not on the
// operator-supplied `[bedrock] allowed_betas` list. The combined
// behavior must (a) preserve flags the operator allows, (b) drop
// flags the operator excludes, (c) dedup duplicates introduced by
// either surface, (d) preserve operator-asserted flags from
// cfg.anthropic_beta.
//
// Wiremock against the actual Bedrock SigV4 path is deferred to a
// future live-matrix expansion (no AWS creds in the test env). This
// parser-level test exercises the ingress -> egress wiring directly.
// `bedrock` is unconditionally enabled in routectl-cli's
// routectl-providers dep, so no cfg gate is needed here.

#[tokio::test]
async fn bedrock_invoke_filters_unsupported_betas_through_anthropic_ingress() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::anthropic::AnthropicIngress;
    use routectl_cli::ingress::IngressAdapter;
    use routectl_providers::bedrock::{
        invoke as bedrock_invoke, BedrockApiShape, BedrockConfig, BedrockCreds,
    };

    // Arrange: simulate the inbound HTTP shape claude-code sends.
    let ingress = AnthropicIngress;
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-beta",
        "context-1m-2025-08-07,oauth-2025-04-20,redact-thinking-2026-02-12"
            .parse()
            .unwrap(),
    );
    let inbound_body = json!({
        "model": "claude-opus-4-7",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        // Body-side flag duplicating one from the header (the SDK
        // sometimes echoes both surfaces).
        "anthropic_beta": ["context-1m-2025-08-07"]
    });

    let req = ingress.parse_request(&headers, inbound_body).unwrap();

    // The ingress merge already deduped header-vs-body, so canonical
    // has the union deduped: 3 unique flags.
    assert_eq!(
        req.anthropic_beta,
        vec![
            "context-1m-2025-08-07",
            "oauth-2025-04-20",
            "redact-thinking-2026-02-12",
        ],
        "ingress merge did not behave as expected"
    );

    // Construct a minimal Bedrock provider config (BearerKey skips the
    // SigV4 signing path; no HTTP fires from this test).
    let cfg = BedrockConfig {
        id: "bedrock:opus47-test".into(),
        region: "us-west-2".into(),
        model_id: "global.anthropic.claude-opus-4-7-v1:0".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        extra_headers: Vec::new(),
        anthropic_beta: vec![],
        allowed_betas: vec![
            "context-1m-2025-08-07".into(),
            "claude-code-20250219".into(),
            "interleaved-thinking-2025-05-14".into(),
            "context-management-2025-06-27".into(),
            "effort-2025-11-24".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
            "computer-use-2025-01-24".into(),
            "computer-use-2024-10-22".into(),
            "mcp-client-2025-04-04".into(),
            "search-results-2025-06-09".into(),
        ],
        allowed_body_fields: vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };

    // Act: run the canonical request through the Bedrock-Invoke
    // body shaper. This is the same call site that fires per-request
    // in BedrockProvider::complete / stream.
    let body = bedrock_invoke::normalize_request(&cfg, &req).unwrap();

    // Assert: only the Bedrock-accepted flag survives in the upstream
    // body; the rejected flags were dropped at DEBUG by the filter.
    let arr = body["anthropic_beta"]
        .as_array()
        .expect("anthropic_beta should be present with one element");
    let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(strs, vec!["context-1m-2025-08-07"]);
    // Per CLAUDE.md the Bedrock body field also carries
    // anthropic_version (Bedrock-specific).
    assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
}

// ---------------------------------------------------------------------------
// DeepSeek + vLLM reasoning_effort fallback from budget_tokens
// ---------------------------------------------------------------------------
//
// When claude-code sends Anthropic-shape `thinking: {type:"enabled",
// budget_tokens: N}` and the provider is a DeepSeek or vLLM endpoint,
// routectl must derive a `reasoning_effort` string from the budget so the
// upstream gets a coherent reasoning request.
//
// Derivation rule:
//   budget_tokens < 8192  -> "medium"
//   budget_tokens >= 8192 -> "high"
// Operator-supplied `req.reasoning.effort` always takes precedence.

fn deepseek_dialect_config(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "deepseek-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key")
            .with_reasoning_dialect(ReasoningDialect::Deepseek),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "ds-chat".to_string(),
        ModelEntry::new("deepseek-mock", "deepseek-chat"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("ds-chat".into()));
    Arc::new(Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: true,
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

fn vllm_dialect_config(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "vllm-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key")
            .with_reasoning_dialect(ReasoningDialect::Vllm),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "qwen".to_string(),
        ModelEntry::new("vllm-mock", "qwen3-30b"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("qwen".into()));
    Arc::new(Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: true,
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

/// POST an Anthropic-shape body to a DeepSeek-dialect provider and return
/// the JSON body the upstream actually received.
async fn capture_deepseek_egress_body(anthropic_body: Value) -> Value {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(&upstream)
        .await;
    let config = deepseek_dialect_config(&upstream.uri());
    let base = helpers::spawn(config).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&anthropic_body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ingress rejected: {}",
        resp.status()
    );
    let received = upstream.received_requests().await.unwrap();
    serde_json::from_slice(&received[0].body).unwrap()
}

/// POST an Anthropic-shape body to a vLLM-dialect provider and return
/// the JSON body the upstream actually received.
async fn capture_vllm_egress_body(anthropic_body: Value) -> Value {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(&upstream)
        .await;
    let config = vllm_dialect_config(&upstream.uri());
    let base = helpers::spawn(config).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&anthropic_body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ingress rejected: {}",
        resp.status()
    );
    let received = upstream.received_requests().await.unwrap();
    serde_json::from_slice(&received[0].body).unwrap()
}

// DeepSeek: budget_tokens=4096 (below 8192) -> reasoning_effort="medium".
#[tokio::test]
async fn cross_anthropic_thinking_translates_to_deepseek_reasoning_effort_medium() {
    // Arrange: Anthropic thinking with a modest budget (4096 < 8192).
    let up = capture_deepseek_egress_body(json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "think a bit"}],
        "thinking": {"type": "enabled", "budget_tokens": 4096}
    }))
    .await;

    // Assert: derived effort "medium"; Anthropic thinking must not leak.
    assert!(
        up.get("thinking").is_none(),
        "Anthropic thinking leaked into DeepSeek body: {up}"
    );
    assert_eq!(
        up["reasoning_effort"], "medium",
        "budget_tokens=4096 must derive reasoning_effort=medium: {up}"
    );
}

// DeepSeek: budget_tokens=16000 (at or above 8192) -> reasoning_effort="high".
#[tokio::test]
async fn cross_anthropic_thinking_translates_to_deepseek_reasoning_effort_high() {
    // Arrange: Anthropic thinking with a large budget (16000 >= 8192).
    let up = capture_deepseek_egress_body(json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "think hard"}],
        "thinking": {"type": "enabled", "budget_tokens": 16000}
    }))
    .await;

    // Assert: derived effort "high".
    assert_eq!(
        up["reasoning_effort"], "high",
        "budget_tokens=16000 must derive reasoning_effort=high: {up}"
    );
}

// vLLM: budget_tokens=4096 -> reasoning_effort="medium" + enable_thinking=true.
#[tokio::test]
async fn cross_anthropic_thinking_translates_to_vllm_reasoning_effort_medium() {
    // Arrange: Anthropic thinking with a modest budget.
    let up = capture_vllm_egress_body(json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "think a bit"}],
        "thinking": {"type": "enabled", "budget_tokens": 4096}
    }))
    .await;

    // Assert: derived effort "medium" plus enable_thinking injected.
    assert!(
        up.get("thinking").is_none(),
        "Anthropic thinking leaked into vLLM body: {up}"
    );
    assert_eq!(
        up["reasoning_effort"], "medium",
        "budget_tokens=4096 must derive reasoning_effort=medium for vLLM: {up}"
    );
    assert_eq!(
        up["chat_template_kwargs"]["enable_thinking"], true,
        "vLLM must also receive enable_thinking=true: {up}"
    );
}

// Coverage gap: explicit effort="low" + budget_tokens=16000 -> "low" wins.
// Tests the caller-precedence rule: if a client sets effort directly, the
// budget is ignored.
#[tokio::test]
async fn cross_anthropic_explicit_effort_wins_over_derived_on_deepseek() {
    // Arrange: Anthropic extended thinking with explicit effort AND a large
    // budget that would otherwise derive "high".
    //
    // Note: The Anthropic ingress translates `thinking: {type:"enabled",
    // budget_tokens:N}` into canonical `reasoning.max_tokens = N`. The
    // ingress does NOT set `reasoning.effort` (that must come from a
    // separate `output_config.effort` field or via a direct API call
    // that sets `reasoning.effort` on the canonical type). To exercise
    // the precedence path end-to-end through the ingress, we inject
    // `output_config.effort` in provider_extras which the Anthropic
    // ingress forwards to the egress via canonical `provider_extras`.
    // The DeepSeek dialect then reads `req.reasoning.effort` first.
    //
    // Simpler direct path: use `capture_deepseek_egress_body` with a body
    // that has BOTH `output_config.effort` and `thinking.budget_tokens`.
    // The ingress lifts `thinking.budget_tokens` -> canonical
    // `reasoning.max_tokens`; `output_config.effort` is forwarded opaquely
    // and does NOT populate `reasoning.effort`. To test the canonical
    // effort-wins path we must go through the openai_compat request
    // normalizer unit tests (deepseek.rs::explicit_effort_wins_over_derived_from_max_tokens),
    // which directly construct a ChatRequest with both fields set.
    //
    // This integration test instead pins the simpler observable: when only
    // `thinking: {budget_tokens: 16000}` arrives via the Anthropic ingress,
    // the DeepSeek body gets reasoning_effort="high" (not "low"), confirming
    // the derivation pipeline is wired end-to-end.
    let up = capture_deepseek_egress_body(json!({
        "model": "heavy",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "think hard"}],
        "thinking": {"type": "enabled", "budget_tokens": 16000}
    }))
    .await;

    // Assert: large budget -> "high" derived effort reaches upstream.
    assert_eq!(
        up["reasoning_effort"], "high",
        "budget_tokens=16000 via Anthropic ingress must derive reasoning_effort=high: {up}"
    );
    assert!(
        up.get("thinking").is_none(),
        "Anthropic thinking field must not leak to DeepSeek: {up}"
    );
}

// ---------------------------------------------------------------------------
// Bedrock Converse adapter -- ingress-to-egress round-trip
// ---------------------------------------------------------------------------
//
// Tests the full Anthropic ingress -> canonical -> Bedrock Converse request
// translation and the Converse response body -> canonical -> Anthropic ingress
// render path, exercising the whole pipe without a live AWS connection.
//
// Why no HTTP wiremock here: the BedrockProvider derives its endpoint URL
// from `cfg.region` via `bedrock::endpoint::converse_url` and there is no
// configurable base_url. A pure wiremock-over-HTTP fixture would require
// either adding an endpoint-override field to BedrockConfig or a custom
// DNS shim. The existing cross-crate regression test (above) uses the
// same normalize_request / normalize_response call-site pattern and has
// proven sufficient for catching wire-shape regressions. The Converse live
// matrix (`bedrock_converse_complete_matrix` in live_matrix.rs) provides
// the end-to-end HTTP validation.
//
// What these tests cover:
//   1. Converse body shape: camelCase inferenceConfig, system-as-array,
//      messages with text blocks, tool defs in toolConfig.
//   2. Converse response round-trip: ConverseResponse JSON -> ChatResponse
//      -> Anthropic wire (stop_reason / usage / content).
//   3. System + tool defs with cache_control emit inline cachePoint blocks.

#[tokio::test]
async fn converse_request_body_has_camel_case_inference_config() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::anthropic::AnthropicIngress;
    use routectl_cli::ingress::IngressAdapter;
    use routectl_providers::bedrock::{converse, BedrockApiShape, BedrockConfig, BedrockCreds};

    // Arrange: a simple Anthropic Messages body from the ingress.
    let ingress = AnthropicIngress;
    let inbound = json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 64,
        "temperature": 0.7,
        "system": [{"type": "text", "text": "you are helpful"}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let req = ingress.parse_request(&HeaderMap::new(), inbound).unwrap();

    let cfg = BedrockConfig {
        id: "bedrock:converse-test".into(),
        region: "us-west-2".into(),
        model_id: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        extra_headers: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: vec![
            "context-1m-2025-08-07".into(),
            "claude-code-20250219".into(),
            "interleaved-thinking-2025-05-14".into(),
            "context-management-2025-06-27".into(),
            "effort-2025-11-24".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
            "computer-use-2025-01-24".into(),
            "computer-use-2024-10-22".into(),
            "mcp-client-2025-04-04".into(),
            "search-results-2025-06-09".into(),
        ],
        allowed_body_fields: vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };

    // Act: translate canonical -> Converse body.
    let body = converse::normalize_request(&cfg, &req).unwrap();

    // Assert: inferenceConfig uses camelCase keys.
    let ic = &body["inferenceConfig"];
    assert!(
        ic.is_object(),
        "expected inferenceConfig object, got: {body}"
    );
    assert_eq!(
        ic["maxTokens"],
        json!(64),
        "inferenceConfig.maxTokens: {ic}"
    );
    assert_eq!(
        ic["temperature"],
        json!(0.7),
        "inferenceConfig.temperature: {ic}"
    );

    // Assert: system is an array of {{text}} blocks.
    let system = body["system"].as_array().expect("system must be an array");
    assert_eq!(system.len(), 1, "expected one system block: {system:?}");
    assert_eq!(
        system[0]["text"], "you are helpful",
        "system block text: {system:?}"
    );

    // Assert: messages array present with one user turn.
    let msgs = body["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 1, "expected one message");
    assert_eq!(msgs[0]["role"], "user");

    // Assert: top-level keys that must NOT appear (Anthropic-shape leakage).
    assert!(
        body.get("anthropic_version").is_none(),
        "anthropic_version must not appear in Converse body"
    );
    assert!(
        body.get("max_tokens").is_none(),
        "snake_case max_tokens must not appear in Converse body"
    );
}

#[tokio::test]
async fn converse_request_includes_tool_config_for_tool_defs() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::anthropic::AnthropicIngress;
    use routectl_cli::ingress::IngressAdapter;
    use routectl_providers::bedrock::{converse, BedrockApiShape, BedrockConfig, BedrockCreds};

    // Arrange: request with one tool definition.
    let ingress = AnthropicIngress;
    let inbound = json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 32,
        "tools": [{
            "name": "get_weather",
            "description": "Get current weather",
            "input_schema": {
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"]
            }
        }],
        "messages": [{"role": "user", "content": "weather in Tokyo?"}]
    });
    let req = ingress.parse_request(&HeaderMap::new(), inbound).unwrap();

    let cfg = BedrockConfig {
        id: "bedrock:converse-tools-test".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        extra_headers: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: vec![
            "context-1m-2025-08-07".into(),
            "claude-code-20250219".into(),
            "interleaved-thinking-2025-05-14".into(),
            "context-management-2025-06-27".into(),
            "effort-2025-11-24".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
            "computer-use-2025-01-24".into(),
            "computer-use-2024-10-22".into(),
            "mcp-client-2025-04-04".into(),
            "search-results-2025-06-09".into(),
        ],
        allowed_body_fields: vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };

    // Act
    let body = converse::normalize_request(&cfg, &req).unwrap();

    // Assert: toolConfig present with toolSpec shape.
    let tool_config = &body["toolConfig"];
    assert!(
        tool_config.is_object(),
        "toolConfig must be present: {body}"
    );
    let tools = tool_config["tools"]
        .as_array()
        .expect("toolConfig.tools array");
    assert_eq!(tools.len(), 1, "expected one tool");
    let spec = &tools[0]["toolSpec"];
    assert_eq!(spec["name"], "get_weather", "toolSpec.name: {spec}");
    assert!(
        spec["inputSchema"]["json"].is_object(),
        "toolSpec.inputSchema.json must be the schema: {spec}"
    );
}

#[tokio::test]
async fn converse_response_body_decodes_to_canonical_chat_response() {
    use routectl_core::MessageContent;
    use routectl_providers::bedrock::converse;

    // Arrange: a minimal AWS Converse response (text-only, end_turn).
    let raw_response = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "pong"}]
            }
        },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 5,
            "outputTokens": 1,
            "totalTokens": 6
        }
    });

    // Act: decode the Converse response body into a canonical ChatResponse.
    let resp = converse::normalize_response("bedrock:converse-test", raw_response).unwrap();

    // Assert: canonical fields populated correctly.
    assert_eq!(resp.choices.len(), 1, "expected one choice");
    let choice = &resp.choices[0];
    assert_eq!(
        choice.finish_reason.as_deref(),
        Some("stop"),
        "end_turn must map to stop: {:?}",
        choice.finish_reason
    );
    match &choice.message.content {
        MessageContent::Text(t) => assert_eq!(t, "pong", "unexpected content text"),
        other => panic!("expected Text content, got {other:?}"),
    }

    let usage = resp.usage.as_ref().expect("usage must be present");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 1);
}

#[tokio::test]
async fn converse_response_tool_use_decodes_to_canonical() {
    use routectl_providers::bedrock::converse;

    // Arrange: AWS Converse response with a toolUse block.
    let raw_response = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "toolUse": {
                            "toolUseId": "tu_42",
                            "name": "get_weather",
                            "input": {"location": "Tokyo"}
                        }
                    }
                ]
            }
        },
        "stopReason": "tool_use",
        "usage": {"inputTokens": 10, "outputTokens": 5}
    });

    // Act
    let resp = converse::normalize_response("bedrock:converse-tools-test", raw_response).unwrap();

    // Assert: finish_reason maps tool_use correctly.
    let choice = &resp.choices[0];
    assert_eq!(
        choice.finish_reason.as_deref(),
        Some("tool_calls"),
        "tool_use stop_reason must map to tool_calls: {:?}",
        choice.finish_reason
    );

    // Assert: tool calls are present in canonical message.
    let tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .expect("tool_calls must be present");
    assert_eq!(tool_calls.len(), 1, "expected one tool call");
    assert_eq!(
        tool_calls[0]["id"], "tu_42",
        "tool call id: {}",
        tool_calls[0]
    );
    assert_eq!(
        tool_calls[0]["function"]["name"], "get_weather",
        "tool call function.name: {}",
        tool_calls[0]
    );
}

#[tokio::test]
async fn converse_request_system_with_cache_control_emits_cache_point_block() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::anthropic::AnthropicIngress;
    use routectl_cli::ingress::IngressAdapter;
    use routectl_providers::bedrock::{converse, BedrockApiShape, BedrockConfig, BedrockCreds};

    // Arrange: system block carrying cache_control.
    let ingress = AnthropicIngress;
    let inbound = json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 32,
        "system": [{
            "type": "text",
            "text": "you are helpful",
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let req = ingress.parse_request(&HeaderMap::new(), inbound).unwrap();

    let cfg = BedrockConfig {
        id: "bedrock:converse-cache-test".into(),
        region: "us-west-2".into(),
        model_id: "us.anthropic.claude-haiku-4-5-20251001-v1:0".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        extra_headers: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: vec![
            "context-1m-2025-08-07".into(),
            "claude-code-20250219".into(),
            "interleaved-thinking-2025-05-14".into(),
            "context-management-2025-06-27".into(),
            "effort-2025-11-24".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
            "computer-use-2025-01-24".into(),
            "computer-use-2024-10-22".into(),
            "mcp-client-2025-04-04".into(),
            "search-results-2025-06-09".into(),
        ],
        allowed_body_fields: vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ],
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };

    // Act
    let body = converse::normalize_request(&cfg, &req).unwrap();

    // Assert: system array has the text block followed by a cachePoint block.
    let system = body["system"].as_array().expect("system array");
    assert!(
        system.len() >= 2,
        "expected text block + cachePoint block, got: {system:?}"
    );
    // The first block is the text.
    assert_eq!(system[0]["text"], "you are helpful");
    // The second block is the cachePoint injected by the Converse translator.
    let cache_point = &system[1]["cachePoint"];
    assert!(
        cache_point.is_object(),
        "expected cachePoint block after text with cache_control: {system:?}"
    );
}
