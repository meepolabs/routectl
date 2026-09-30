//! Anthropic thinking tokens reach the usage ledger's `reasoning_tokens`
//! column, end to end: a real routectl server, the anthropic-api egress
//! against a mock upstream, and the persisted row read back.
//!
//! The upstream usage shapes are the ones live claude-sonnet-4-5 responses
//! returned: thinking tokens nest under `usage.output_tokens_details` on a
//! buffered response and on the closing `message_delta` of a stream, and a
//! non-thinking response omits the object.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const UPSTREAM_MODEL: &str = "claude-sonnet-4-5";

fn thinking_usage() -> Value {
    json!({
        "input_tokens": 69,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "output_tokens": 124,
        "output_tokens_details": {"thinking_tokens": 118}
    })
}

fn plain_usage() -> Value {
    json!({
        "input_tokens": 36,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "output_tokens": 5
    })
}

fn buffered_response(usage: Value) -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "text", "text": "391"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage
    })
}

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// A stream whose opener carries no thinking breakdown and whose closing
/// `message_delta` carries `delta_usage`, as the live wire does.
fn streamed_response(delta_usage: Value) -> String {
    [
        sse(
            "message_start",
            &json!({"type": "message_start", "message": {
                "id": "msg_01", "type": "message", "role": "assistant", "content": [],
                "model": "claude-sonnet-4-5-20250929", "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 66, "cache_creation_input_tokens": 0,
                          "cache_read_input_tokens": 0, "output_tokens": 1}}}),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "391"}}),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
        sse(
            "message_delta",
            &json!({"type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": delta_usage}),
        ),
        sse("message_stop", &json!({"type": "message_stop"})),
    ]
    .concat()
}

/// A routectl server whose one alias dispatches to `upstream`, plus the
/// path of its isolated usage ledger.
async fn routectl(upstream: &MockServer) -> (String, std::path::PathBuf) {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("k")).with_base_url(upstream.uri()),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", UPSTREAM_MODEL));
    let mut aliases = BTreeMap::new();
    aliases.insert("claude-sonnet".to_string(), AliasValue::Single("m".into()));
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

async fn mount(upstream: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(upstream)
        .await;
}

/// Send one Messages turn tagged `request_id` and read the body to its end.
async fn send_turn(base: &str, request_id: &str, stream: bool) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-request-id", request_id)
        .json(&json!({
            "model": "claude-sonnet",
            "max_tokens": 2048,
            "stream": stream,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [{"role": "user", "content": "What is 17 * 23?"}]
        }))
        .send()
        .await
        .expect("request sent");
    assert_eq!(resp.status(), 200, "ingress returned {}", resp.status());
    let _ = resp.text().await.expect("body");
}

/// `(reasoning_tokens, output_tokens)` of the ledger row for `request_id`,
/// once it lands.
async fn ledger_tokens(ledger: &std::path::Path, request_id: &str) -> (Option<i64>, Option<i64>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(db) = routectl_usage::open_readonly(ledger)
            && let Ok(row) = db.conn().query_row(
                "SELECT reasoning_tokens, output_tokens FROM requests WHERE request_id = ?1",
                [request_id],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
        {
            return row;
        }
        assert!(Instant::now() < deadline, "row {request_id} never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_buffered_thinking_response_records_its_thinking_tokens() {
    // Arrange
    let upstream = MockServer::start().await;
    mount(
        &upstream,
        ResponseTemplate::new(200).set_body_json(buffered_response(thinking_usage())),
    )
    .await;
    let (base, ledger) = routectl(&upstream).await;

    // Act
    send_turn(&base, "thinking-buffered", false).await;

    // Assert: thinking is recorded, and output_tokens is not inflated by it.
    assert_eq!(
        ledger_tokens(&ledger, "thinking-buffered").await,
        (Some(118), Some(124))
    );
}

#[tokio::test]
async fn a_streamed_thinking_response_records_its_thinking_tokens() {
    // Arrange
    let upstream = MockServer::start().await;
    let delta_usage = json!({
        "input_tokens": 66, "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0, "output_tokens": 57,
        "output_tokens_details": {"thinking_tokens": 51}
    });
    mount(
        &upstream,
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(streamed_response(delta_usage)),
    )
    .await;
    let (base, ledger) = routectl(&upstream).await;

    // Act
    send_turn(&base, "thinking-streamed", true).await;

    // Assert
    assert_eq!(
        ledger_tokens(&ledger, "thinking-streamed").await,
        (Some(51), Some(57))
    );
}

#[tokio::test]
async fn a_response_without_thinking_records_no_reasoning_tokens() {
    // Arrange
    let upstream = MockServer::start().await;
    mount(
        &upstream,
        ResponseTemplate::new(200).set_body_json(buffered_response(plain_usage())),
    )
    .await;
    let (base, ledger) = routectl(&upstream).await;

    // Act
    send_turn(&base, "no-thinking", false).await;

    // Assert: absent, not zero-filled.
    assert_eq!(ledger_tokens(&ledger, "no-thinking").await, (None, Some(5)));
}
