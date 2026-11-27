//! A Responses-lane reasoning artifact shown to an Anthropic client, echoed
//! back, and carried across a fallback onto the genuine Anthropic host, end
//! to end through the assembled daemon.
//!
//! Request 1 is served by a real openai-responses egress whose mock origin
//! streams an encrypted reasoning item, visible text and a function call; the
//! daemon's own Anthropic renderer turns that into the client's
//! `redacted_thinking` envelope, so the envelope under test is the one
//! production mints. Request 2 echoes that response as history with the tool
//! result; the Responses origin fails it before any content, and the chain
//! falls back to a real anthropic-api egress on the exact Anthropic host,
//! resolved to a capturing mock. That body must carry neither the envelope
//! nor the artifact inside it, and must keep the text and the tool exchange.
//!
//! In-crate because the router-injection seam is `cfg(test)` only.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{Provider, reasoning_envelope};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ResolvedModel, RetryPolicy, Router,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::wire_loss_daemon_tests::Daemon;

const ALIAS: &str = "mixed";
const RESPONSES: &str = "pr";
const ANTHROPIC: &str = "pa";
const UPSTREAM: &str = "upstream";
const ARTIFACT_ID: &str = "rs_fallback1";
const ARTIFACT_BLOB: &str = "rsn_fallback-artifact-0001";
const CALL_ID: &str = "call_fallback1";
const VISIBLE_TEXT: &str = "visible-answer-sentinel";
const QUESTION: &str = "first-question-sentinel";
const TOOL_OUTPUT: &str = "tool-output-sentinel";

/// One Responses SSE frame per JSON value, each named by its `type`.
fn sse(frames: &[Value]) -> String {
    frames
        .iter()
        .map(|data| {
            format!(
                "event: {}\ndata: {data}\n\n",
                data["type"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

/// A completed Responses stream: an encrypted reasoning item, a message
/// with visible text, then a function call.
fn responses_answer() -> ResponseTemplate {
    let reasoning = json!({"type": "reasoning", "id": ARTIFACT_ID, "summary": [],
                           "encrypted_content": ARTIFACT_BLOB});
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": VISIBLE_TEXT,
                                      "annotations": []}]});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": CALL_ID,
                      "name": "lookup", "arguments": "{\"q\":\"x\"}",
                      "status": "completed"});
    let done = json!({
        "id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
        "status": "completed", "model": UPSTREAM,
        "output": [reasoning, message, call],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    });
    let mut created = done.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    let body = sse(&[
        json!({"type": "response.created", "response": created}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "reasoning", "id": ARTIFACT_ID, "summary": []}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": {"type": "message", "id": "msg_1", "role": "assistant",
                        "status": "in_progress", "content": []}}),
        json!({"type": "response.content_part.added", "output_index": 1, "content_index": 0,
               "part": {"type": "output_text", "text": "", "annotations": []}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0,
               "delta": VISIBLE_TEXT}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": message}),
        json!({"type": "response.output_item.added", "output_index": 2,
               "item": {"type": "function_call", "id": "fc_1", "call_id": CALL_ID,
                        "name": "lookup", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2,
               "delta": "{\"q\":\"x\"}"}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": call}),
        json!({"type": "response.completed", "response": done}),
    ]);
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn anthropic_answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": UPSTREAM,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

/// The Responses origin answers request 1 and fails request 2, which alone
/// carries the tool output, before any content.
async fn responses_origin() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains(TOOL_OUTPUT))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(responses_answer())
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

async fn anthropic_origin() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(anthropic_answer())
        .mount(&server)
        .await;
    server
}

/// The Responses target first, the genuine Anthropic host second, one
/// attempt each so the walk to the second is a fallback.
fn chain_config(responses: &MockServer) -> Config {
    let key = crate::test_secret::file_ref("k");
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    Config {
        providers: BTreeMap::from([
            (
                RESPONSES.to_string(),
                ProviderEntry::openai_responses(key.clone())
                    .with_openai_responses_auth_kind(AuthKind::ApiKey)
                    .with_openai_responses_base_url(responses.uri()),
            ),
            (
                ANTHROPIC.to_string(),
                ProviderEntry::anthropic_api(key).with_base_url("https://api.anthropic.com"),
            ),
        ]),
        models: BTreeMap::from([
            (RESPONSES.to_string(), ModelEntry::new(RESPONSES, UPSTREAM)),
            (ANTHROPIC.to_string(), ModelEntry::new(ANTHROPIC, UPSTREAM)),
        ]),
        aliases: BTreeMap::from([(
            ALIAS.to_string(),
            AliasValue::Chain(vec![RESPONSES.to_string(), ANTHROPIC.to_string()]),
        )]),
        retry,
        ..Default::default()
    }
}

/// The real providers behind `config`: the Anthropic one dials the exact
/// Anthropic host on the mock's port, resolved to the mock, so every
/// host-keyed decision reads the genuine host.
fn chain_router(config: &Config, responses: &MockServer, anthropic: &MockServer) -> Router {
    let mut responses_cfg = OpenAiResponsesConfig::new(RESPONSES, "test-key");
    responses_cfg.auth_kind = AuthKind::ApiKey;
    responses_cfg.base_url = responses.uri();
    let addr = *anthropic.address();
    let mut anthropic_cfg = AnthropicApiConfig::new(ANTHROPIC, "test-key");
    anthropic_cfg.base_url = format!("http://api.anthropic.com:{}", addr.port());
    let anthropic_provider =
        AnthropicApiProvider::new(anthropic_cfg).with_host_resolved_to_for_tests(addr);
    let resolved = |nickname: &str, provider: Arc<dyn Provider>| {
        (
            nickname.to_string(),
            Arc::new(ResolvedModel::new(nickname, nickname, provider, UPSTREAM)),
        )
    };
    let mut router = Router::new(Arc::new(config.clone()));
    router.install_resolved_models(BTreeMap::from([
        resolved(
            RESPONSES,
            Arc::new(OpenAiResponsesProvider::new(responses_cfg)),
        ),
        resolved(ANTHROPIC, Arc::new(anthropic_provider)),
    ]));
    router
}

async fn post_messages(daemon: &Daemon, body: &Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", daemon.base_url))
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .expect("the daemon answers");
    let status = response.status().as_u16();
    let text = response.text().await.expect("the client body");
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

fn request_1() -> Value {
    json!({
        "model": ALIAS, "max_tokens": 64, "stream": false,
        "messages": [{"role": "user", "content": QUESTION}]
    })
}

/// Request 2: request 1's history, the assistant turn exactly as the client
/// received it, and the tool result answering its tool_use.
fn request_2(assistant_content: &Value, tool_use_id: &str) -> Value {
    json!({
        "model": ALIAS, "max_tokens": 64, "stream": false,
        "messages": [
            {"role": "user", "content": QUESTION},
            {"role": "assistant", "content": assistant_content},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_use_id, "content": TOOL_OUTPUT}
            ]}
        ]
    })
}

fn blocks_of<'a>(content: &'a Value, kind: &str) -> Vec<&'a Value> {
    content
        .as_array()
        .expect("content is an array")
        .iter()
        .filter(|b| b["type"] == kind)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn an_echoed_responses_artifact_never_reaches_the_anthropic_host_after_a_fallback() {
    // Arrange
    let jar_dir = tempfile::tempdir().expect("cookie jar dir");
    let _jar = routectl_testkit::ScopedEnv::set(
        "ROUTECTL_COOKIE_FILE",
        jar_dir.path().join("cookies.json"),
    );
    let responses = responses_origin().await;
    let anthropic = anthropic_origin().await;
    let config = chain_config(&responses);
    let router = chain_router(&config, &responses, &anthropic);
    let daemon = Daemon::spawn(config, router).await;

    // Act -- request 1, served by the Responses target.
    let (status_1, response_1) = post_messages(&daemon, &request_1()).await;

    // Assert -- the client holds the envelope production minted, carrying
    // the original id and the artifact byte for byte.
    assert_eq!(status_1, 200, "request 1: {response_1}");
    let content_1 = &response_1["content"];
    let redacted = blocks_of(content_1, "redacted_thinking");
    assert_eq!(redacted.len(), 1, "one redacted block: {response_1}");
    let envelope = redacted[0]["data"].as_str().expect("data string");
    let (_, id, blob) = reasoning_envelope::unwrap(envelope).expect("a routectl envelope");
    assert_eq!((id, blob), (Some(ARTIFACT_ID), ARTIFACT_BLOB));
    let tool_uses = blocks_of(content_1, "tool_use");
    assert_eq!(tool_uses.len(), 1, "one tool_use: {response_1}");
    let tool_use_id = tool_uses[0]["id"]
        .as_str()
        .expect("tool_use id")
        .to_string();
    assert!(
        anthropic
            .received_requests()
            .await
            .expect("recording")
            .is_empty(),
        "request 1 never reaches the Anthropic target"
    );

    // Act -- request 2, the echo, failing over to the Anthropic host.
    let (status_2, response_2) = post_messages(&daemon, &request_2(content_1, &tool_use_id)).await;

    // Assert -- the fallback served the request.
    assert_eq!(status_2, 200, "request 2: {response_2}");
    let received = anthropic.received_requests().await.expect("recording");
    assert_eq!(received.len(), 1, "exactly one Anthropic attempt");
    let sent = String::from_utf8(received[0].body.clone()).expect("utf-8 body");
    let sent_json: Value = serde_json::from_str(&sent).expect("json body");

    // Assert -- neither the envelope nor the artifact inside it is on the
    // wire, and no redacted block is left to carry either.
    for forbidden in [envelope, ARTIFACT_BLOB, "rctl1", "redacted_thinking"] {
        assert!(
            !sent.contains(forbidden),
            "the Anthropic body carries `{forbidden}`: {sent}"
        );
    }

    // Assert -- the visible text and the tool exchange are intact and in
    // order.
    let messages = sent_json["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "no turn lost or added: {sent}");
    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    let texts = blocks_of(&assistant["content"], "text");
    assert!(
        texts.iter().any(|b| b["text"] == VISIBLE_TEXT),
        "the visible text survives: {sent}"
    );
    let sent_uses = blocks_of(&assistant["content"], "tool_use");
    assert_eq!(sent_uses.len(), 1, "the tool_use survives: {sent}");
    let results = blocks_of(&messages[2]["content"], "tool_result");
    assert_eq!(results.len(), 1, "the tool_result survives: {sent}");
    assert_eq!(
        results[0]["tool_use_id"], sent_uses[0]["id"],
        "the tool_result still answers the tool_use: {sent}"
    );

    daemon.shutdown().await;
}
