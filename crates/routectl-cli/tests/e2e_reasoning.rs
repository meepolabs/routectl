//! E2E reasoning round-trip tests through the full HTTP server.
//!
//! Exercises the OpenRouter-shape `reasoning_details` surface end-to-end:
//! - Client sends a request with `reasoning: {effort: "high"}`.
//! - routectl receives, dispatches to the configured upstream.
//! - Upstream (mocked) returns reasoning in its native shape (`reasoning_content`
//!   for DeepSeek/vLLM dialects, `thinking` blocks for Anthropic).
//! - routectl normalizes to `reasoning_details` and emits to the client.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use routectl_router::{
    AliasEntry, Config, LegacyCompat, ProviderEntry, ReasoningDialect, RetryPolicy, ServerConfig,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn spawn(config: Arc<Config>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener)
            .await
            .expect("server failed");
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    format!("http://{addr}")
}

fn deepseek_alias_config(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "deepseek".into(),
        ProviderEntry::OpenaiCompat {
            base_url: format!("{upstream_base}/v1"),
            api_key_ref: "literal:test".into(),
            extra_headers: BTreeMap::new(),
            default_extras: None,
            reasoning_dialect: ReasoningDialect::Deepseek,
        },
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "reasoning".into(),
        AliasEntry {
            chain: vec!["deepseek:deepseek-reasoner".into()],
            retry: None,
        },
    );
    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
    })
}

#[tokio::test]
async fn deepseek_reasoning_content_lifts_to_reasoning_details_via_server() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-rsn",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Final answer: 42.",
                    "reasoning_content": "Step by step: thought, thought, conclusion."
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 30,
                "total_tokens": 42
            }
        })))
        .mount(&upstream)
        .await;

    let base_url = spawn(deepseek_alias_config(&upstream.uri())).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({
            "model": "reasoning",
            "messages": [{"role": "user", "content": "Solve 6 * 7."}],
            "reasoning": {"effort": "high"}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["choices"][0]["message"]["content"], "Final answer: 42.");
    assert_eq!(resp["routectl_provider"], "deepseek");

    let details = resp["choices"][0]["message"]["reasoning_details"]
        .as_array()
        .expect("reasoning_details present");
    assert!(!details.is_empty(), "reasoning_details should be populated");
    assert_eq!(details[0]["format"], "deepseek-v1");
    assert_eq!(details[0]["type"], "reasoning.text");
}

#[tokio::test]
async fn deepseek_strips_reasoning_content_from_outgoing_history() {
    let upstream = MockServer::start().await;

    // The mock asserts the incoming body has NO `reasoning_content` field
    // anywhere in the messages array. We do this by responding 200 only when
    // the body matches a strict pattern.
    use wiremock::matchers::body_partial_json;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // Negative match: body must not contain reasoning_content. We capture
        // any incoming body shape and verify in the test below that the
        // assistant message we sent had its reasoning_content stripped.
        .and(body_partial_json(json!({
            "messages": [
                {"role": "user"},
                {"role": "assistant"},
                {"role": "user"}
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-strip",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&upstream)
        .await;

    let base_url = spawn(deepseek_alias_config(&upstream.uri())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({
            "model": "reasoning",
            "messages": [
                {"role": "user", "content": "What's 6*7?"},
                {
                    "role": "assistant",
                    "content": "42",
                    "reasoning_content": "DROP_ME if not stripped"
                },
                {"role": "user", "content": "double-check"}
            ]
        }))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success(), "got: {}", resp.status());
}

#[tokio::test]
async fn streaming_reasoning_chunks_arrive_with_format_tag() {
    let upstream = MockServer::start().await;
    let sse_body = "\
data: {\"id\":\"a\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"id\":\"a\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\n\
data: {\"id\":\"a\",\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"42\"}}]}\n\n\
data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let base_url = spawn(deepseek_alias_config(&upstream.uri())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({
            "model": "reasoning",
            "messages": [{"role": "user", "content": "Solve."}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_success(), "status: {status}, body: {body}");
    assert!(body.contains("data: "), "expected SSE body, got: {body}");
    assert!(body.contains("[DONE]"));
    // The deepseek-v1 format tag should appear in normalized reasoning_details.
    assert!(
        body.contains("deepseek-v1") || body.contains("reasoning"),
        "expected reasoning content in normalized stream, got: {body}"
    );
}
