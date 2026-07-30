//! End-to-end coverage of the chained tool_choice path: a FLAT Responses
//! named-forcing tool_choice (`{"type":"function","name":X}`) arrives at
//! the `POST /v1/responses` ingress, is normalized to the canonical
//! nested form, and is then re-mapped by each egress.
//!
//! - Anthropic egress -> `{"type":"tool","name":X}`.
//! - openai-compat egress -> `{"type":"function","function":{"name":X}}`.
//!
//! Both halves drive the real axum server + a wiremock upstream and
//! assert the tool_choice shape on the body the upstream received.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
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

fn base_server() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
        ..Default::default()
    }
}

/// Flat Responses request body with a named-forcing tool_choice and a
/// matching function tool so the forcing choice has something to force.
fn flat_responses_body(model: &str) -> Value {
    json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "weather?"}]
        }],
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "parameters": {"type": "object", "properties": {}}
        }],
        "tool_choice": {"type": "function", "name": "get_weather"}
    })
}

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

// ---------------------------------------------------------------------------
// openai-compat egress: flat Responses tool_choice -> nested function shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_responses_tool_choice_reaches_openai_compat_egress_as_nested_function() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_body()))
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "openai-mock".to_string(),
        ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
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

    let config = Arc::new(Config {
        server: base_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    });
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&flat_responses_body("mock-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();

    assert_eq!(
        upstream_body["tool_choice"],
        json!({"type": "function", "function": {"name": "get_weather"}}),
        "openai-compat egress must emit the nested function shape: {upstream_body}"
    );
}

// ---------------------------------------------------------------------------
// Anthropic egress: flat Responses tool_choice -> {type:tool, name}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_responses_tool_choice_reaches_anthropic_egress_as_tool() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key")).with_base_url(upstream.uri()),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("mock-model".to_string(), AliasValue::Single("haiku".into()));

    let config = Arc::new(Config {
        server: base_server(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    });
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&flat_responses_body("mock-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();

    assert_eq!(
        upstream_body["tool_choice"],
        json!({"type": "tool", "name": "get_weather"}),
        "Anthropic egress must emit the tool shape: {upstream_body}"
    );
}
