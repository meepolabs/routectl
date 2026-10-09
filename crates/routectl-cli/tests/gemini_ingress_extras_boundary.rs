//! The Gemini payload-extras source boundary, driven end to end: a real
//! Anthropic Messages or OpenAI Chat Completions request through the axum
//! server, dispatched to a wiremock Gemini upstream, asserted on the body the
//! upstream received and on every log event the process emitted meanwhile.
//!
//! Neither ingress speaks the Gemini dialect, so the extras each one sweeps
//! from the client body are addressed to another vendor. Only the operator's
//! own `payload_extras` may reach Google; the client's `metadata` block, a
//! remote MCP server's bearer, and any key a future client adds stay behind.
//!
//! Its own integration binary, and every test `#[tokio::test]`
//! (current-thread): `with_capture` installs a thread-local subscriber, which
//! only sees the server's events when the server runs on the same thread.

use std::sync::Arc;

use routectl_router::{Config, ServerConfig};
use routectl_testkit::{CapturedEvent, with_capture};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const TOKEN_TELL: &str = "e2e-mcp-tok-6h";
const FUTURE_TELL: &str = "e2e-future-2c";
const METADATA_TELL: &str = "e2e-user-9x";
const SAFETY_ID_TELL: &str = "e2e-sid-1f";
const CACHE_KEY_TELL: &str = "e2e-pck-7j";
const OPERATOR_THRESHOLD: &str = "BLOCK_ONLY_HIGH";

fn gemini_response_body() -> Value {
    json!({
        "candidates": [{
            "content": {"parts": [{"text": "ok"}], "role": "model"},
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1, "totalTokenCount": 6},
        "modelVersion": "gemini-2.5-pro",
        "responseId": "resp-e2e"
    })
}

/// A Gemini provider whose operator `payload_extras` carries `safetySettings`.
fn gemini_config(upstream: &MockServer) -> Arc<Config> {
    let toml_src = format!(
        r#"
[providers.gemini-mock]
kind = "gemini"
api_key_ref = "{key}"
base_url = "{base}"
payload_extras = {{ safetySettings = [{{ category = "HARM_CATEGORY_HATE_SPEECH", threshold = "{threshold}" }}] }}

[models.target]
provider = "gemini-mock"
upstream = "gemini-2.5-pro"

[aliases]
mock-model = "target"
"#,
        key = common::file_ref("test-key"),
        base = upstream.uri(),
        threshold = OPERATOR_THRESHOLD,
    );
    let mut cfg: Config = toml::from_str(&toml_src).expect("gemini test config parses");
    cfg.server = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
        ..Default::default()
    };
    common::isolate_usage_db(Arc::new(cfg))
}

async fn mount_gemini(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_response_body()))
        .mount(upstream)
        .await;
}

/// Boot the server on this thread, POST `body` at `route`, and return the
/// body Gemini received plus every event emitted meanwhile.
async fn send_through(route: &str, body: Value) -> (Value, Vec<CapturedEvent>) {
    let upstream = MockServer::start().await;
    mount_gemini(&upstream).await;
    let config = gemini_config(&upstream);
    let ((), events) = with_capture(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener, None)
                .await
                .expect("server failed");
        });
        common::readiness::await_health(&base).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}{route}"))
            .header("x-api-key", "client-key")
            .json(&body)
            .send()
            .await
            .expect("ingress request sends");
        assert_eq!(resp.status(), 200, "ingress returned {}", resp.status());
    })
    .await;
    let received = upstream.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    let upstream_body = serde_json::from_slice(&received[0].body).expect("upstream JSON");
    (upstream_body, events)
}

/// The opt-in TRACE dump of the client's own inbound body, which carries
/// everything the client sent by design. Every other event -- including the
/// outgoing-body trace -- is held to the no-echo rule.
fn is_inbound_body_trace(event: &CapturedEvent) -> bool {
    event.level == tracing::Level::TRACE && event.message == "ingress request body"
}

fn assert_withheld(upstream_body: &Value, events: &[CapturedEvent], tells: &[&str]) {
    let wire = upstream_body.to_string();
    assert_eq!(
        upstream_body["safetySettings"][0]["threshold"],
        json!(OPERATOR_THRESHOLD),
        "operator payload_extras must still reach Gemini: {wire}"
    );
    for tell in tells {
        assert!(!wire.contains(tell), "{tell} reached Gemini: {wire}");
        let echoes: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| !is_inbound_body_trace(e) && format!("{e:?}").contains(tell))
            .collect();
        assert!(echoes.is_empty(), "{tell} reached a log event: {echoes:?}");
    }
    let withhold_reports = events
        .iter()
        .filter(|e| e.message.contains("ingress extras"))
        .collect::<Vec<_>>();
    assert_eq!(
        withhold_reports.len(),
        1,
        "the withhold is reported once: {events:?}"
    );
    assert_eq!(
        withhold_reports[0].level,
        tracing::Level::DEBUG,
        "the withhold is an expected transform, reported at DEBUG"
    );
}

#[tokio::test]
async fn anthropic_ingress_credentials_identity_and_future_keys_never_reach_gemini() {
    // Arrange
    let body = json!({
        "model": "mock-model",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": METADATA_TELL},
        "mcp_servers": [{
            "type": "url",
            "url": "https://mcp.example.com/sse",
            "name": "example",
            "authorization_token": TOKEN_TELL
        }],
        "some_future_knob": {"nested": FUTURE_TELL}
    });

    // Act
    let (upstream_body, events) = send_through("/v1/messages", body).await;

    // Assert
    assert!(upstream_body.get("metadata").is_none());
    assert!(upstream_body.get("mcp_servers").is_none());
    assert_withheld(
        &upstream_body,
        &events,
        &[TOKEN_TELL, FUTURE_TELL, METADATA_TELL],
    );
}

#[tokio::test]
async fn openai_ingress_identity_and_future_keys_never_reach_gemini() {
    // Arrange
    let body = json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "hi"}],
        "safety_identifier": SAFETY_ID_TELL,
        "prompt_cache_key": CACHE_KEY_TELL,
        "some_future_knob": FUTURE_TELL,
        "safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"}]
    });

    // Act
    let (upstream_body, events) = send_through("/v1/chat/completions", body).await;

    // Assert: the client's own same-key safetySettings loses to the operator's.
    assert!(!upstream_body.to_string().contains("BLOCK_NONE"));
    assert_withheld(
        &upstream_body,
        &events,
        &[SAFETY_ID_TELL, CACHE_KEY_TELL, FUTURE_TELL],
    );
}
