//! A same-dialect client's carriage does not turn a healthy stream into a
//! pre-content failure. Each row is a stream that commits when the client
//! speaks another dialect (no carriage is produced); the same bytes, read
//! for a client of the upstream's own dialect, put a carriage-only chunk
//! before first content for every event that produced nothing canonical.
//! Both legs are the REAL providers against a mock upstream, so the
//! carriage is the shipped parser's own.

#![cfg(feature = "openai-responses")]

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::StreamExt;
use routectl_core::{ChatChunk, ChatRequest, Provider, RequestProvenance};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_responses::{
    AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, RetryPolicy, Router};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const REASONING_ITEMS: usize = 3;
const PARALLEL_CALLS: usize = 4;
const EMPTY_TEXT_BLOCKS: usize = 9;

async fn sse_upstream(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

fn frame(event: &Value) -> String {
    let name = event["type"].as_str().expect("event type");
    format!("event: {name}\ndata: {event}\n\n")
}

/// A Responses turn whose reasoning items carry no summary or signature and
/// whose parallel calls all open before the first one's arguments arrive.
fn responses_body() -> String {
    let mut events = vec![json!({"type": "response.created", "response": {
        "id": "resp_1", "object": "response", "status": "in_progress",
        "model": "gpt-5", "created_at": 1, "output": []}})];
    let mut output = Vec::new();
    for i in 0..REASONING_ITEMS {
        let item = |status: &str| {
            json!({"type": "reasoning", "id": format!("rs_{i}"),
                                         "summary": [], "status": status})
        };
        events.push(
            json!({"type": "response.output_item.added", "output_index": i,
                           "item": item("in_progress")}),
        );
        events.push(
            json!({"type": "response.output_item.done", "output_index": i,
                           "item": item("completed")}),
        );
        output.push(item("completed"));
    }
    let call = |n: usize, arguments: &str, status: &str| {
        json!({"type": "function_call", "id": format!("fc_{n}"),
               "call_id": format!("call_{n}"), "name": "read_file",
               "arguments": arguments, "status": status})
    };
    for n in 0..PARALLEL_CALLS {
        events.push(json!({"type": "response.output_item.added",
                           "output_index": REASONING_ITEMS + n,
                           "item": call(n, "", "in_progress")}));
    }
    for n in 0..PARALLEL_CALLS {
        let index = REASONING_ITEMS + n;
        events.push(json!({"type": "response.function_call_arguments.done",
                           "output_index": index, "item_id": format!("fc_{n}"),
                           "arguments": "{}"}));
        events.push(
            json!({"type": "response.output_item.done", "output_index": index,
                           "item": call(n, "{}", "completed")}),
        );
        output.push(call(n, "{}", "completed"));
    }
    events.push(json!({"type": "response.completed", "response": {
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5",
        "created_at": 1, "output": output,
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}));
    events.iter().map(frame).collect()
}

/// An Anthropic turn that opens several empty text blocks before the one
/// that carries text.
fn anthropic_body() -> String {
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [],
        "model": "claude-opus-4-7", "usage": {"input_tokens": 1, "output_tokens": 1}}})];
    for index in 0..=EMPTY_TEXT_BLOCKS {
        events.push(json!({"type": "content_block_start", "index": index,
                           "content_block": {"type": "text", "text": ""}}));
        if index == EMPTY_TEXT_BLOCKS {
            events.push(json!({"type": "content_block_delta", "index": index,
                               "delta": {"type": "text_delta", "text": "ok"}}));
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                       "usage": {"output_tokens": 2}}),
    );
    events.push(json!({"type": "message_stop"}));
    events.iter().map(frame).collect()
}

fn responses_leg(base_url: &str) -> Arc<dyn Provider> {
    let mut cfg = OpenAiResponsesConfig::new("p-leg", "test-key");
    cfg.auth_kind = AuthKind::ApiKey;
    cfg.base_url = base_url.to_owned();
    Arc::new(OpenAiResponsesProvider::new(cfg))
}

fn anthropic_leg(base_url: &str) -> Arc<dyn Provider> {
    let mut cfg = AnthropicApiConfig::new("p-leg", "test-key");
    cfg.base_url = base_url.to_owned();
    Arc::new(AnthropicApiProvider::new(cfg))
}

fn one_leg_router(entry: ProviderEntry, provider: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert("p-leg".into(), entry);
    config
        .aliases
        .insert("alias".into(), AliasValue::Chain(vec!["m-leg".into()]));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.initial_backoff_ms = 1;
    retry.backoff_multiplier = 1.0;
    config.retry = retry;
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m-leg".to_owned(),
        Arc::new(ResolvedModel::new("m-leg", "p-leg", provider, "wire-model")),
    )]));
    router
}

fn request(provenance: RequestProvenance) -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        messages: vec![routectl_core::test_utils::user_msg("hi")].into(),
        max_tokens: Some(16),
        stream: Some(true),
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req
}

async fn stream(router: &Router, provenance: RequestProvenance) -> Vec<ChatChunk> {
    match router.stream(request(provenance)).await {
        Ok(committed) => {
            committed
                .map(|item| item.expect("no mid-stream error"))
                .collect()
                .await
        }
        Err(e) => panic!("{provenance:?}: the stream did not commit: {e}"),
    }
}

fn has_tool_calls(chunks: &[ChatChunk]) -> bool {
    chunks.iter().any(|c| {
        c.choices
            .iter()
            .any(|ch| ch.delta.tool_calls.as_ref().is_some_and(|t| !t.is_empty()))
    })
}

/// The chunks a stream yields before its first content-bearing one.
fn leading(chunks: &[ChatChunk]) -> usize {
    chunks
        .iter()
        .position(|c| {
            c.choices.iter().any(|ch| {
                ch.delta.content.as_deref().is_some_and(|t| !t.is_empty())
                    || ch.delta.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
            })
        })
        .expect("a content chunk")
}

/// The router's cap on counted content-free chunks before first content.
const METADATA_CAP: usize = 8;

fn text_of(chunks: &[ChatChunk]) -> String {
    chunks
        .iter()
        .flat_map(|c| &c.choices)
        .filter_map(|ch| ch.delta.content.as_deref())
        .collect()
}

#[tokio::test]
#[serial_test::serial]
async fn a_responses_stream_opening_many_items_commits_with_and_without_carriage() {
    // Arrange: the provider's cookie jar would otherwise persist under the
    // real home directory on drop.
    let jar_dir = tempfile::tempdir().expect("temp dir");
    let _jar = routectl_testkit::ScopedEnv::set(
        "ROUTECTL_COOKIE_FILE",
        jar_dir.path().join("cookies.json"),
    );
    let upstream = sse_upstream(responses_body()).await;
    let router = one_leg_router(
        ProviderEntry::openai_responses(common::file_ref("k")),
        responses_leg(&upstream.uri()),
    );

    // Act
    let without = stream(&router, RequestProvenance::OpenaiIngress).await;
    let with = stream(&router, RequestProvenance::ResponsesIngress).await;

    // Assert
    assert!(has_tool_calls(&without), "control: the calls arrive");
    assert!(has_tool_calls(&with), "the calls arrive with carriage on");
    assert!(
        leading(&without) <= METADATA_CAP,
        "control: without carriage the stream opens under the cap"
    );
    assert!(
        leading(&with) > METADATA_CAP + 1,
        "carriage puts more chunks than the cap before content: {}",
        leading(&with)
    );
}

#[tokio::test]
async fn an_anthropic_stream_opening_empty_blocks_commits_with_and_without_carriage() {
    // Arrange
    let upstream = sse_upstream(anthropic_body()).await;
    let router = one_leg_router(
        ProviderEntry::anthropic_api(common::file_ref("k")),
        anthropic_leg(&upstream.uri()),
    );

    // Act
    let without = stream(&router, RequestProvenance::OpenaiIngress).await;
    let with = stream(&router, RequestProvenance::AnthropicIngress).await;

    // Assert
    assert_eq!(text_of(&without), "ok", "control: the text arrives");
    assert_eq!(text_of(&with), "ok", "the text arrives with carriage on");
    assert!(
        leading(&without) <= METADATA_CAP,
        "control: without carriage the stream opens under the cap"
    );
    assert!(
        leading(&with) > 2 * EMPTY_TEXT_BLOCKS,
        "every empty block's start and stop rides its own chunk: {}",
        leading(&with)
    );
}
