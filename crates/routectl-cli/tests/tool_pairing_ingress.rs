//! End-to-end handling of malformed tool call / tool result pairing.
//!
//! Chat completions, messages (streaming and not), messages count_tokens and
//! responses forward a malformed transcript to the first target unchanged:
//! exactly ONE upstream call, carrying the offending item, and the
//! upstream's answer reaches the client. Each alias is a two-target chain
//! whose entries each have their own counting wiremock upstream, both able
//! to serve, so a refused dispatch or a fallback hop shows up in the
//! per-entry received-request counts. A paired control request through the
//! same servers proves the counters see traffic.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const OPENAI_ALIAS: &str = "openai-chain";
const ANTHROPIC_ALIAS: &str = "anthropic-chain";

/// The four upstreams behind the two aliases, one per chain entry, so a
/// call reaching the fallback instead of the primary shows up per server.
struct Upstreams {
    openai_primary: MockServer,
    openai_fallback: MockServer,
    anthropic_primary: MockServer,
    anthropic_fallback: MockServer,
}

struct Harness {
    base: String,
    upstreams: Upstreams,
}

/// An openai-compat upstream answering both a streaming and a plain chat
/// completion.
async fn openai_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(sse(OPENAI_SSE))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_completion()))
        .mount(&server)
        .await;
    server
}

/// An anthropic upstream answering count_tokens, a streaming and a plain
/// message.
async fn anthropic_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(sse(ANTHROPIC_SSE))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_message()))
        .mount(&server)
        .await;
    server
}

/// Every request body `server` received, parsed.
async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("request recording on")
        .iter()
        .map(|r| serde_json::from_slice(&r.body).expect("upstream body is JSON"))
        .collect()
}

impl Harness {
    async fn start() -> Self {
        let upstreams = Upstreams {
            openai_primary: openai_upstream().await,
            openai_fallback: openai_upstream().await,
            anthropic_primary: anthropic_upstream().await,
            anthropic_fallback: anthropic_upstream().await,
        };
        let config = chain_config(&upstreams);
        let base = spawn(config).await;
        Self { base, upstreams }
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

    /// The `(primary, fallback)` upstreams behind `alias`.
    fn chain_of(&self, alias: &str) -> (&MockServer, &MockServer) {
        match alias {
            OPENAI_ALIAS => (
                &self.upstreams.openai_primary,
                &self.upstreams.openai_fallback,
            ),
            ANTHROPIC_ALIAS => (
                &self.upstreams.anthropic_primary,
                &self.upstreams.anthropic_fallback,
            ),
            other => panic!("no chain behind alias {other}"),
        }
    }

    /// `(primary, fallback)` request counts behind `alias`.
    async fn chain_calls(&self, alias: &str) -> (usize, usize) {
        let (primary, fallback) = self.chain_of(alias);
        (bodies(primary).await.len(), bodies(fallback).await.len())
    }

    /// Post `body` and require it to be forwarded: a 200 to the client and
    /// exactly one request, to its alias's primary upstream and none to the
    /// fallback, whose `messages` carry `needle`.
    async fn assert_forwarded_once(&self, route: &str, body: &Value, needle: &str) {
        let alias = body["model"].as_str().expect("body names its alias");

        let (status, response) = self.post(route, body).await;

        assert_eq!(status, 200, "{route}: {response}");
        let (primary, fallback) = self.chain_of(alias);
        let primary_bodies = bodies(primary).await;
        assert_eq!(
            (primary_bodies.len(), bodies(fallback).await.len()),
            (1, 0),
            "{route}: (primary, fallback) calls: the primary serves, no fallback hop"
        );
        assert!(
            primary_bodies[0]["messages"].to_string().contains(needle),
            "{route}: the upstream received the transcript unchanged: {}",
            primary_bodies[0]
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

fn chain_config(upstreams: &Upstreams) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for (name, upstream) in [
        ("oa-primary", &upstreams.openai_primary),
        ("oa-fallback", &upstreams.openai_fallback),
    ] {
        providers.insert(
            name.to_string(),
            ProviderEntry::openai_compat(upstream.uri(), common::file_ref("test-key")),
        );
        models.insert(format!("{name}-model"), ModelEntry::new(name, "gpt-4o"));
    }
    for (name, upstream) in [
        ("an-primary", &upstreams.anthropic_primary),
        ("an-fallback", &upstreams.anthropic_fallback),
    ] {
        providers.insert(
            name.to_string(),
            ProviderEntry::anthropic_api(common::file_ref("test-key"))
                .with_base_url(upstream.uri()),
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
async fn count_tokens_forwards_a_trailing_call_once() {
    let h = Harness::start().await;

    h.assert_forwarded_once(
        "/v1/messages/count_tokens",
        &anthropic_trailing_call(false),
        "toolu_1",
    )
    .await;
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
    // Arrange: the positive control for the one-call counts above.
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
    assert_eq!(
        h.chain_calls(OPENAI_ALIAS).await,
        (1, 0),
        "chat (primary, fallback)"
    );
    assert_eq!(
        h.chain_calls(ANTHROPIC_ALIAS).await,
        (1, 0),
        "count_tokens (primary, fallback)"
    );
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
