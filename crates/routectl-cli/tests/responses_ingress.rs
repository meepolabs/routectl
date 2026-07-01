//! Integration tests for the OpenAI Responses ingress
//! (`POST /v1/responses`). End-to-end through the axum server + a
//! wiremock upstream that pretends to be an openai-compat host.
//!
//! What's covered:
//!   - Happy path: a minimal Responses body translates to the
//!     openai-compat upstream and the client gets a Responses-shaped
//!     completion (`object:"response"`, `status:"completed"`,
//!     `output[0].type:"message"`).
//!   - Statefulness contract: `previous_response_id` -> 400 with the
//!     OpenAI error envelope; `store:true` without a prior id is
//!     accepted (persistence ignored, see the ingress WARN).
//!   - Listener auth: `[server.auth].tokens` enforced on `x-api-key`.

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

    use routectl_router::Config;
    use tokio::net::TcpListener;

    pub async fn spawn(config: Arc<Config>) -> String {
        let config = crate::common::isolate_usage_db(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener, None)
                .await
                .expect("server failed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        base_url
    }
}

/// Canonical openai-compat chat completion the wiremock upstream
/// returns. The Responses ingress renders this back into the
/// Responses wire shape on the way out to the client.
fn openai_response_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

/// Point the `mock-model` target at the wiremock upstream as an
/// openai-compat provider. The egress shape is irrelevant to the
/// Responses ingress under test (the ingress only produces canonical);
/// openai-compat is the simplest egress that accepts the request shape.
/// Optional listener auth via `[server.auth].tokens`.
fn responses_proxy_config(upstream_base: &str, auth_tokens: Option<Vec<String>>) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key"),
    );

    let mut models = BTreeMap::new();
    models.insert(
        "mockmodel".to_string(),
        ModelEntry::new("openai-mock", "mock-model"),
    );

    let mut aliases = BTreeMap::new();
    aliases.insert(
        "mock-model".to_string(),
        AliasValue::Single("mockmodel".into()),
    );

    let server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: auth_tokens.map(|tokens| ServerAuth { tokens }),
        strict_translation: false,
        allow_disable_fallbacks: true,
        ..Default::default()
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

async fn mount_upstream(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(upstream)
        .await;
}

// ---------------------------------------------------------------------------
// Happy path: Responses body -> upstream -> Responses-shaped completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn responses_post_translates_to_upstream_and_returns_completion() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;

    let config = responses_proxy_config(&upstream.uri(), None);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "mock-model",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }]
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let client_body: Value = resp.json().await.unwrap();
    assert_eq!(
        client_body["object"], "response",
        "expected a Responses-shaped envelope: {client_body}"
    );
    assert_eq!(
        client_body["status"], "completed",
        "expected status=completed: {client_body}"
    );
    assert_eq!(
        client_body["output"][0]["type"], "message",
        "expected output[0].type=message: {client_body}"
    );
}

// ---------------------------------------------------------------------------
// Statefulness contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn previous_response_id_rejected_with_400() {
    // No upstream needed: the request 400s at the ingress before any
    // dispatch. Point at a dead address to prove no egress fires.
    let config = responses_proxy_config("http://127.0.0.1:1", None);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "mock-model",
        "previous_response_id": "resp_abc",
        "input": []
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let envelope: Value = resp.json().await.unwrap();
    // OpenAI-shape envelope (set on the ResponsesIngress adapter): a flat
    // `{"error":{...}}`. A parse-time `Error::Validation` surfaces with
    // routectl's internal `validation_error` tag on the OpenAI envelope
    // (the OpenAI envelope passes the routectl tag through verbatim;
    // only the Anthropic envelope remaps it to `invalid_request_error`).
    assert!(
        envelope.get("error").is_some(),
        "expected an OpenAI error envelope: {envelope}"
    );
    assert!(
        envelope.get("type").is_none(),
        "OpenAI envelope is flat (no outer `type`): {envelope}"
    );
    assert_eq!(envelope["error"]["type"], "validation_error");
}

#[tokio::test]
async fn store_true_without_prev_id_is_accepted() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;

    let config = responses_proxy_config(&upstream.uri(), None);
    let base = helpers::spawn(config).await;

    let body = json!({
        "model": "mock-model",
        "store": true,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]
        }]
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    // store-only (no previous_response_id) -> accepted; persistence is
    // ignored with a WARN, the turn is self-contained so the answer is
    // correct.
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Listener auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_accepts_x_api_key() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;

    let config = responses_proxy_config(&upstream.uri(), Some(vec!["literal:sk-test".into()]));
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("x-api-key", "sk-test")
        .json(&json!({
            "model": "mock-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn auth_rejects_bogus_token() {
    let config = responses_proxy_config("http://127.0.0.1:1", Some(vec!["literal:sk-test".into()]));
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("x-api-key", "wrong")
        .json(&json!({
            "model": "mock-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
