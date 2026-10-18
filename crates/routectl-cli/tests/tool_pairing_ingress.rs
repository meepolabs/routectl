//! End-to-end handling of malformed tool call / tool result pairing.
//!
//! Chat completions, messages and responses (streaming and not) forward a
//! malformed transcript to the first target unchanged: exactly ONE upstream
//! call, carrying the offending item, and the upstream's answer reaches the
//! client. Messages count_tokens still answers it with a local 400 in the
//! Anthropic error envelope and makes ZERO upstream calls. Each alias is a
//! two-target chain on a counting wiremock upstream, so a refused dispatch
//! or a fallback hop shows up in the received-request count. A paired
//! control request through the same server proves the counter sees traffic.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
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
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(sse(OPENAI_SSE))
            .with_priority(1)
            .mount(&openai_upstream)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_completion()))
            .mount(&openai_upstream)
            .await;
        let anthropic_upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages/count_tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
            .with_priority(1)
            .mount(&anthropic_upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(sse(ANTHROPIC_SSE))
            .with_priority(2)
            .mount(&anthropic_upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_message()))
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
        self.upstream_bodies().await.len()
    }

    /// Every request body either upstream received, parsed.
    async fn upstream_bodies(&self) -> Vec<Value> {
        let mut received = self
            .openai_upstream
            .received_requests()
            .await
            .expect("request recording on");
        received.extend(
            self.anthropic_upstream
                .received_requests()
                .await
                .expect("request recording on"),
        );
        received
            .iter()
            .map(|r| serde_json::from_slice(&r.body).expect("upstream body is JSON"))
            .collect()
    }

    /// Post `body` and require it to be forwarded: a 200 to the client and
    /// exactly one upstream request, whose `messages` carry `needle`.
    async fn assert_forwarded_once(&self, route: &str, body: &Value, needle: &str) {
        let (status, response) = self.post(route, body).await;

        assert_eq!(status, 200, "{route}: {response}");
        let bodies = self.upstream_bodies().await;
        assert_eq!(bodies.len(), 1, "{route}: one upstream call, no fallback");
        assert!(
            bodies[0]["messages"].to_string().contains(needle),
            "{route}: the upstream received the transcript unchanged: {}",
            bodies[0]
        );
    }
}

fn sse(body: &'static str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .append_header("content-type", "text/event-stream")
        .set_body_string(body)
}

const OPENAI_SSE: &str = concat!(
    "data: {\"id\":\"s1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn anthropic_message() -> Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
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

fn assert_anthropic_validation_envelope(status: u16, body: &Value, defect: &str) {
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(defect), "{body}");
}

#[tokio::test]
async fn chat_completions_forward_an_orphan_result_once() {
    for stream in [false, true] {
        let h = Harness::start().await;

        h.assert_forwarded_once(
            "/v1/chat/completions",
            &openai_orphan_result(stream),
            "call_missing",
        )
        .await;
    }
}

#[tokio::test]
async fn messages_forward_a_trailing_call_once() {
    for stream in [false, true] {
        let h = Harness::start().await;

        h.assert_forwarded_once("/v1/messages", &anthropic_trailing_call(stream), "toolu_1")
            .await;
    }
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
async fn responses_forward_an_orphan_function_call_output_once() {
    let h = Harness::start().await;
    let body = json!({
        "model": OPENAI_ALIAS,
        "input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_missing", "output": "r"}
        ]
    });

    h.assert_forwarded_once("/v1/responses", &body, "call_missing")
        .await;
}

#[tokio::test]
async fn paired_transcripts_reach_the_upstream() {
    // Arrange: the positive control for the count_tokens zero-call assertion.
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
