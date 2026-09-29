//! End-to-end rejection of malformed tool call / tool result pairing.
//!
//! Every inference entry point (chat completions, messages, responses, and
//! messages count_tokens; streaming and not) must answer a malformed
//! transcript with a local 400 in the ingress dialect's error envelope and
//! make ZERO upstream calls -- no dispatch, no retry, no fallback hop. Each
//! alias is a two-target chain on a counting wiremock upstream, so a leaked
//! dispatch or a fallback attempt shows up as a received request. A paired
//! control request through the same server proves the counter sees traffic.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const OPENAI_ALIAS: &str = "openai-chain";
const ANTHROPIC_ALIAS: &str = "anthropic-chain";

struct Harness {
    base: String,
    openai_upstream: MockServer,
    anthropic_upstream: MockServer,
}

impl Harness {
    async fn start() -> Self {
        let openai_upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_completion()))
            .mount(&openai_upstream)
            .await;
        let anthropic_upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
            .mount(&anthropic_upstream)
            .await;
        let config = chain_config(&openai_upstream.uri(), &anthropic_upstream.uri());
        let base = spawn(config).await;
        Self {
            base,
            openai_upstream,
            anthropic_upstream,
        }
    }

    async fn post(&self, route: &str, body: &Value) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}{route}", self.base))
            .json(body)
            .send()
            .await
            .expect("request reaches routectl");
        let status = resp.status().as_u16();
        let text = resp.text().await.expect("response body");
        let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, parsed)
    }

    async fn upstream_calls(&self) -> usize {
        let openai = self
            .openai_upstream
            .received_requests()
            .await
            .expect("request recording on");
        let anthropic = self
            .anthropic_upstream
            .received_requests()
            .await
            .expect("request recording on");
        openai.len() + anthropic.len()
    }
}

async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base).await;
    base
}

fn chain_config(openai_base: &str, anthropic_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for name in ["oa-primary", "oa-fallback"] {
        providers.insert(
            name.to_string(),
            ProviderEntry::openai_compat(openai_base, common::file_ref("test-key")),
        );
        models.insert(format!("{name}-model"), ModelEntry::new(name, "gpt-4o"));
    }
    for name in ["an-primary", "an-fallback"] {
        providers.insert(
            name.to_string(),
            ProviderEntry::anthropic_api(common::file_ref("test-key"))
                .with_base_url(anthropic_base.to_string()),
        );
        models.insert(
            format!("{name}-model"),
            ModelEntry::new(name, "claude-haiku-4-5"),
        );
    }
    let mut aliases = BTreeMap::new();
    aliases.insert(
        OPENAI_ALIAS.to_string(),
        AliasValue::Chain(vec!["oa-primary-model".into(), "oa-fallback-model".into()]),
    );
    aliases.insert(
        ANTHROPIC_ALIAS.to_string(),
        AliasValue::Chain(vec!["an-primary-model".into(), "an-fallback-model".into()]),
    );
    Arc::new(Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    })
}

fn openai_completion() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

/// OpenAI chat body whose `role: tool` result names a call that was never made.
fn openai_orphan_result(stream: bool) -> Value {
    json!({
        "model": OPENAI_ALIAS,
        "stream": stream,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "tool", "tool_call_id": "call_missing", "content": "r"}
        ]
    })
}

/// Anthropic body whose assistant `tool_use` is never answered.
fn anthropic_trailing_call(stream: bool) -> Value {
    json!({
        "model": ANTHROPIC_ALIAS,
        "max_tokens": 64,
        "stream": stream,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
            ]}
        ]
    })
}

fn assert_openai_validation_envelope(status: u16, body: &Value, defect: &str) {
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], "validation_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(defect), "{body}");
}

fn assert_anthropic_validation_envelope(status: u16, body: &Value, defect: &str) {
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(defect), "{body}");
}

#[tokio::test]
async fn chat_completions_complete_rejects_orphan_result_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;

    // Act
    let (status, body) = h
        .post("/v1/chat/completions", &openai_orphan_result(false))
        .await;

    // Assert
    assert_openai_validation_envelope(status, &body, "orphan_result");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn chat_completions_stream_rejects_orphan_result_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;

    // Act
    let (status, body) = h
        .post("/v1/chat/completions", &openai_orphan_result(true))
        .await;

    // Assert
    assert_openai_validation_envelope(status, &body, "orphan_result");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn messages_complete_rejects_trailing_call_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;

    // Act
    let (status, body) = h
        .post("/v1/messages", &anthropic_trailing_call(false))
        .await;

    // Assert
    assert_anthropic_validation_envelope(status, &body, "trailing_pending_calls");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn messages_stream_rejects_trailing_call_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;

    // Act
    let (status, body) = h.post("/v1/messages", &anthropic_trailing_call(true)).await;

    // Assert
    assert_anthropic_validation_envelope(status, &body, "trailing_pending_calls");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn count_tokens_rejects_trailing_call_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;

    // Act
    let (status, body) = h
        .post("/v1/messages/count_tokens", &anthropic_trailing_call(false))
        .await;

    // Assert
    assert_anthropic_validation_envelope(status, &body, "trailing_pending_calls");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn responses_rejects_orphan_function_call_output_without_upstream_call() {
    // Arrange
    let h = Harness::start().await;
    let body = json!({
        "model": OPENAI_ALIAS,
        "input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_missing", "output": "r"}
        ]
    });

    // Act
    let (status, body) = h.post("/v1/responses", &body).await;

    // Assert
    assert_openai_validation_envelope(status, &body, "orphan_result");
    assert_eq!(h.upstream_calls().await, 0);
}

#[tokio::test]
async fn paired_transcripts_reach_the_upstream() {
    // Arrange: the positive control for every zero-call assertion above.
    let h = Harness::start().await;
    let chat = json!({
        "model": OPENAI_ALIAS,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "lookup", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "r"}
        ]
    });
    let count = json!({
        "model": ANTHROPIC_ALIAS,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "r"}
            ]}
        ]
    });

    // Act
    let (chat_status, chat_body) = h.post("/v1/chat/completions", &chat).await;
    let (count_status, count_body) = h.post("/v1/messages/count_tokens", &count).await;

    // Assert
    assert_eq!(chat_status, 200, "{chat_body}");
    assert_eq!(count_status, 200, "{count_body}");
    assert_eq!(h.upstream_calls().await, 2);
}

/// Whether a canonical transcript carries at least one tool call on any
/// carrier -- the density check that keeps the corpus test below from
/// passing on a corpus of plain-text turns.
fn carries_tool_call(req: &routectl_core::ChatRequest) -> bool {
    req.messages.iter().any(|m| {
        m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
            || matches!(&m.content, routectl_core::MessageContent::Parts(parts)
                if parts.iter().any(|p| p.type_tag() == "tool_use"))
    })
}

#[test]
fn every_replay_fixture_transcript_passes_pairing_validation() {
    // Arrange: the committed driver corpus plus any local captures.
    let driver = common::replay::discover_driver_fixtures(&common::replay::driver_root())
        .expect("driver corpus readable");
    let captured = common::replay::discover_fixtures(&common::replay::local_root())
        .map(|c| c.fixtures)
        .unwrap_or_default();
    let fixtures: Vec<_> = driver.fixtures.into_iter().chain(captured).collect();

    // Act
    let mut validated = 0_usize;
    let mut with_tools = 0_usize;
    for fixture in &fixtures {
        let Some(req) = common::replay::parse_enriched_canonical(fixture)
            .unwrap_or_else(|e| panic!("{}: {e}", fixture.name))
        else {
            continue;
        };
        if let Err(e) = routectl_core::validate_tool_pairing(&req.messages) {
            panic!("fixture {} rejected: {e}", fixture.name);
        }
        validated += 1;
        with_tools += usize::from(carries_tool_call(&req));
    }

    // Assert
    eprintln!("[tool-pairing] fixtures accepted={validated} with_tool_calls={with_tools}");
    assert!(validated > 0, "no replay fixture was validated");
    assert!(with_tools > 0, "no validated fixture carried a tool call");
}
