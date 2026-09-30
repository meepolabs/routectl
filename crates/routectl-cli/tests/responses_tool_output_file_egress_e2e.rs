//! A file returned by a tool reaches the openai-responses upstream as an
//! `input_file` item inside `function_call_output.output`.
//!
//! Drives the real `/v1/chat/completions` ingress (axum server) against a
//! wiremock Responses upstream and asserts on the body the upstream
//! received, so the canonical `File` part is followed from the client wire
//! to the upstream wire rather than from a hand-built canonical request.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_providers::openai_responses::AuthKind as ResponsesAuthKind;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const FILE_DATA: &str = "data:text/plain;base64,S0lURS03NzMx";

async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base_url).await;
    base_url
}

/// The Responses upstream is drained as SSE, so the mock answers with one
/// terminal event.
fn responses_sse_body() -> String {
    let completed = json!({
        "id": "resp_01",
        "object": "response",
        "status": "completed",
        "model": "mock-model",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "KITE-7731"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!("data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n")
}

async fn responses_lane() -> (MockServer, Arc<Config>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_sse_body()),
        )
        .mount(&upstream)
        .await;

    let mut providers = BTreeMap::new();
    providers.insert(
        "responses-mock".to_string(),
        ProviderEntry::openai_responses(common::file_ref("test-key"))
            .with_openai_responses_base_url(upstream.uri())
            .with_openai_responses_auth_kind(ResponsesAuthKind::ApiKey),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "target".to_string(),
        ModelEntry::new("responses-mock", "mock-model"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "mock-model".to_string(),
        AliasValue::Single("target".into()),
    );
    let config = Arc::new(Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: true,
            ..Default::default()
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    });
    (upstream, config)
}

/// A chat-completions conversation whose tool answer is text plus a file.
fn chat_body_with_tool_file() -> Value {
    json!({
        "model": "mock-model",
        "messages": [
            {"role": "user", "content": "read the file and tell me the code"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": [
                {"type": "text", "text": "contents attached"},
                {"type": "file", "file": {"filename": "notes.txt", "file_data": FILE_DATA}}
            ]}
        ]
    })
}

#[tokio::test]
async fn a_tool_result_file_reaches_the_responses_upstream_as_input_file() {
    // Arrange
    let (upstream, config) = responses_lane().await;
    let base = spawn(config).await;

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body_with_tool_file())
        .send()
        .await
        .expect("ingress request sends");

    // Assert
    assert_eq!(resp.status(), 200, "ingress returned {}", resp.status());
    let received = upstream
        .received_requests()
        .await
        .expect("requests recorded");
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    let body: Value = serde_json::from_slice(&received[0].body).expect("upstream body is JSON");
    let Some(call_output) = body["input"]
        .as_array()
        .expect("input is an array")
        .iter()
        .find(|item| item["type"] == "function_call_output")
    else {
        panic!("no function_call_output upstream: {body}");
    };
    assert_eq!(
        call_output["output"],
        json!([
            {"type": "input_text", "text": "contents attached"},
            {"type": "input_file", "filename": "notes.txt", "file_data": FILE_DATA}
        ]),
        "the tool's file must ship inside function_call_output: {body}"
    );
}
