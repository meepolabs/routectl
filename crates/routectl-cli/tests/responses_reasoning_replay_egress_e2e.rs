//! Same-dialect reasoning replay through the real `POST /v1/responses`
//! ingress and the openai-responses egress, asserted on the bytes the
//! upstream received.
//!
//! A Responses client echoes back the reasoning items it was served. One
//! carrying a signature replays with its id and signature intact. One
//! carrying only a summary (the shape the server emits when encrypted
//! reasoning is not included) ships its summary with neither the id nor a
//! signature: an unreplayable id is rejected upstream as "item not found",
//! while the id-less summary item is accepted.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_providers::openai_responses::AuthKind as ResponsesAuthKind;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const SUMMARY_ONLY_ID: &str = "rs_marker_summary_only";
const SIGNED_ID: &str = "rs_marker_signed";
const SIGNATURE: &str = "gAAAAABmarker-signed-reasoning";

async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base_url).await;
    base_url
}

/// The Responses upstream is drained as SSE (`complete` forces
/// `stream=true`), so the mock returns a single terminal event.
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
            "content": [{"type": "output_text", "text": "ok"}]
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

/// A second turn echoing `reasoning` back before the assistant answer.
fn replay_body(reasoning: Value) -> Value {
    json!({
        "model": "mock-model",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "what is 17 * 26?"}]},
            reasoning,
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "442"}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "and doubled?"}]}
        ]
    })
}

/// POST `request` at the real `/v1/responses` route and return the raw
/// bytes the upstream received.
async fn upstream_bytes_for(request: &Value) -> String {
    let (upstream, config) = responses_lane().await;
    let base = spawn(config).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(request)
        .send()
        .await
        .expect("ingress request sends");
    assert_eq!(resp.status(), 200, "ingress returned {}", resp.status());

    let received = upstream
        .received_requests()
        .await
        .expect("requests recorded");
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    String::from_utf8(received[0].body.clone()).expect("upstream body is UTF-8")
}

/// The one `reasoning` input item in `bytes`, re-serialized.
fn sole_reasoning_item(bytes: &str) -> String {
    let body: Value = serde_json::from_str(bytes).expect("upstream body is JSON");
    let items: Vec<&Value> = body["input"]
        .as_array()
        .expect("input array")
        .iter()
        .filter(|i| i["type"] == "reasoning")
        .collect();
    assert_eq!(items.len(), 1, "expected one reasoning item: {bytes}");
    items[0].to_string()
}

#[tokio::test]
async fn a_summary_only_reasoning_item_reaches_the_upstream_without_an_id_or_a_signature() {
    // Arrange
    let request = replay_body(json!({
        "type": "reasoning",
        "id": SUMMARY_ONLY_ID,
        "summary": [{"type": "summary_text", "text": "multiply the tens then the ones"}]
    }));

    // Act
    let bytes = upstream_bytes_for(&request).await;

    // Assert
    let item = sole_reasoning_item(&bytes);
    assert!(
        item.contains("multiply the tens then the ones"),
        "the summary must reach the upstream: {item}"
    );
    assert!(!item.contains("\"id\""), "no id key on the item: {item}");
    assert!(
        !item.contains("\"encrypted_content\""),
        "no encrypted_content key on the item: {item}"
    );
    assert!(
        !bytes.contains(SUMMARY_ONLY_ID),
        "the unreplayable id must not reach the upstream anywhere: {bytes}"
    );
}

#[tokio::test]
async fn a_signed_reasoning_item_reaches_the_upstream_byte_for_byte() {
    // Arrange
    let request = replay_body(json!({
        "type": "reasoning",
        "id": SIGNED_ID,
        "summary": [{"type": "summary_text", "text": "signed summary"}],
        "encrypted_content": SIGNATURE
    }));

    // Act
    let bytes = upstream_bytes_for(&request).await;

    // Assert: the exact serialization a signed item has always had (the
    // request body is key-sorted on the way out).
    let expected = format!(
        r#"{{"encrypted_content":"{SIGNATURE}","id":"{SIGNED_ID}","summary":[{{"text":"signed summary","type":"summary_text"}}],"type":"reasoning"}}"#
    );
    assert!(
        bytes.contains(&expected),
        "the signed item must ship unchanged; expected {expected} in {bytes}"
    );
}
