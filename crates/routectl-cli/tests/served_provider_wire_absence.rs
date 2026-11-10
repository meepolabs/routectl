//! The served provider never reaches a client response body, on any
//! ingress, while the usage ledger still records which provider served.
//!
//! Each row drives a real routectl server against one openai-compat mock
//! upstream, asserts the client body carries no `routectl_provider` key,
//! and reads the ledger row for the same request id back as the positive
//! control that the served provider was populated.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const PROVIDER: &str = "served-upstream";
const ALIAS: &str = "mock-model";
const FIELD: &str = "routectl_provider";

fn completion_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

fn completion_stream() -> String {
    concat!(
        "data: {\"id\":\"s1\",\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"s1\",\"model\":\"upstream-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string()
}

async fn mount_upstream(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completion_stream()),
        )
        .with_priority(1)
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_body()))
        .with_priority(2)
        .mount(upstream)
        .await;
}

/// A routectl server whose one alias dispatches to `upstream`, plus the
/// path of its isolated usage ledger.
async fn routectl(upstream: &MockServer) -> (String, std::path::PathBuf) {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut providers = BTreeMap::new();
    providers.insert(
        PROVIDER.to_string(),
        ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new(PROVIDER, "upstream-model"));
    let mut aliases = BTreeMap::new();
    aliases.insert(ALIAS.to_string(), AliasValue::Single("m".into()));
    let config = common::isolate_usage_db(Arc::new(Config {
        providers,
        models,
        aliases,
        retry,
        ..Default::default()
    }));
    let ledger = config.usage.db_path.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base).await;
    (base, ledger)
}

/// The `provider` column of the ledger row for `request_id`, once it lands.
async fn ledger_provider(ledger: &std::path::Path, request_id: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(db) = routectl_usage::open_readonly(ledger)
            && let Ok(provider) = db.conn().query_row(
                "SELECT provider FROM requests WHERE request_id = ?1",
                [request_id],
                |r| r.get::<_, Option<String>>(0),
            )
        {
            return provider;
        }
        assert!(Instant::now() < deadline, "row {request_id} never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every JSON document the client received: the body itself, or each SSE
/// `data:` payload of a stream.
fn client_documents(text: &str, is_stream: bool) -> Vec<Value> {
    if !is_stream {
        return vec![serde_json::from_str(text).expect("json body")];
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("json frame"))
        .collect()
}

fn has_field_anywhere(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| k == FIELD || has_field_anywhere(v)),
        Value::Array(items) => items.iter().any(has_field_anywhere),
        _ => false,
    }
}

struct Row {
    name: &'static str,
    route: &'static str,
    is_stream: bool,
    body: Value,
}

fn rows() -> [Row; 4] {
    let messages = json!([{"role": "user", "content": "hi"}]);
    [
        Row {
            name: "openai chat non-stream",
            route: "/v1/chat/completions",
            is_stream: false,
            body: json!({"model": ALIAS, "messages": messages}),
        },
        Row {
            name: "openai chat stream chunks",
            route: "/v1/chat/completions",
            is_stream: true,
            body: json!({"model": ALIAS, "messages": messages, "stream": true}),
        },
        Row {
            name: "anthropic messages",
            route: "/v1/messages",
            is_stream: false,
            body: json!({"model": ALIAS, "max_tokens": 64, "messages": messages}),
        },
        Row {
            name: "responses",
            route: "/v1/responses",
            is_stream: false,
            body: json!({
                "model": ALIAS,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]
                }]
            }),
        },
    ]
}

/// Send `row` tagged `request_id` and return every failed expectation.
async fn row_failures(
    client: &reqwest::Client,
    base: &str,
    ledger: &std::path::Path,
    row: &Row,
    request_id: &str,
) -> Vec<String> {
    let resp = client
        .post(format!("{base}{}", row.route))
        .header("x-request-id", request_id)
        .json(&row.body)
        .send()
        .await
        .expect("request sent");
    let status = resp.status();
    let text = resp.text().await.expect("body");
    if status != 200 {
        return vec![format!("{}: status {status}: {text}", row.name)];
    }
    let documents = client_documents(&text, row.is_stream);
    let mut failures = Vec::new();
    if documents.is_empty() {
        failures.push(format!("{}: no documents in {text}", row.name));
    }
    failures.extend(
        documents
            .iter()
            .filter(|doc| has_field_anywhere(doc))
            .map(|doc| format!("{}: provider key on the wire: {doc}", row.name)),
    );
    let recorded = ledger_provider(ledger, request_id).await;
    if recorded.as_deref() != Some(PROVIDER) {
        failures.push(format!("{}: ledger provider {recorded:?}", row.name));
    }
    failures
}

#[tokio::test]
async fn client_responses_omit_the_served_provider_while_the_ledger_records_it() {
    // Arrange
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let (base, ledger) = routectl(&upstream).await;
    let client = reqwest::Client::new();
    let mut failures = Vec::new();

    for (i, row) in rows().iter().enumerate() {
        // Act
        let request_id = format!("served-provider-row-{i}");
        failures.extend(row_failures(&client, &base, &ledger, row, &request_id).await);
    }

    // Assert
    assert!(failures.is_empty(), "{failures:#?}");
}
