//! The first-event opening usage crosses the router's pre-content buffer
//! only on the attempt that wins. Both legs are the REAL anthropic-api egress
//! against a wiremock upstream, so the carrier is produced by the shipped
//! parser and head-merge rather than hand-built: a leg that opens with
//! `message_start` and then fails before content must not leak its opening
//! count, and the winning leg's opening count reaches the caller exactly.

use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, RetryPolicy};
use crate::resolved::ResolvedModel;
use futures::stream::StreamExt;
use routectl_core::{ChatChunk, ChatRequest, OpeningUsage};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use std::collections::BTreeMap;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FAILED_LEG_INPUT: u32 = 111_111;
const WINNER_INPUT: u32 = 2_345;
const WINNER_CACHE_WRITE: u32 = 678;
const WINNER_CACHE_READ: u32 = 90_123;

fn message_start(id: &str, input: u32, cache_write: u32, cache_read: u32) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{id}\",\
         \"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\
         \"usage\":{{\"input_tokens\":{input},\"output_tokens\":1,\
         \"cache_creation_input_tokens\":{cache_write},\"cache_read_input_tokens\":{cache_read}}}}}}}\n\n"
    )
}

const TEXT_EVENTS: &str = concat!(
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const OVERLOADED_EVENT: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n";

async fn sse_upstream(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    server
}

fn anthropic_leg(id: &str, base_url: &str) -> Arc<dyn Provider> {
    let mut cfg = AnthropicApiConfig::new(id, "test-key");
    cfg.base_url = base_url.to_string();
    Arc::new(AnthropicApiProvider::new(cfg))
}

fn chain_router(legs: Vec<(&str, Arc<dyn Provider>)>) -> Router {
    let mut config = Config::default();
    for (name, _) in &legs {
        config.providers.insert(
            format!("p-{name}"),
            ProviderEntry::anthropic_api("file:///unused"),
        );
    }
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(legs.iter().map(|(name, _)| format!("m-{name}")).collect()),
    );
    config.retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 1,
        backoff_multiplier: 1.0,
        ..RetryPolicy::default()
    };
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = legs
        .into_iter()
        .map(|(name, provider)| {
            let model = ResolvedModel::new(
                format!("m-{name}"),
                format!("p-{name}"),
                provider,
                "claude-opus-4-7",
            );
            (format!("m-{name}"), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn alias_request() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        messages: vec![].into(),
        max_tokens: Some(16),
        stream: Some(true),
        ..Default::default()
    }
}

async fn collect(router: &Router) -> Vec<ChatChunk> {
    router
        .stream(alias_request())
        .await
        .expect("a leg commits")
        .map(|item| item.expect("no mid-stream error"))
        .collect()
        .await
}

fn openings(chunks: &[ChatChunk]) -> Vec<&OpeningUsage> {
    chunks
        .iter()
        .filter_map(|c| c.upstream_meta.as_ref()?.opening_usage.as_ref())
        .collect()
}

fn assert_winner_opening(chunks: &[ChatChunk]) {
    let found = openings(chunks);
    assert_eq!(found.len(), 1, "exactly one opener reaches the caller");
    let opening = found[0];
    assert_eq!(opening.input_tokens, WINNER_INPUT);
    assert_eq!(
        opening.cache_creation_input_tokens,
        Some(WINNER_CACHE_WRITE)
    );
    assert_eq!(opening.cache_read_input_tokens, Some(WINNER_CACHE_READ));
    let first = chunks.first().expect("chunks");
    assert!(
        first.upstream_meta.is_some(),
        "the opener leads the committed stream"
    );
    let usage_chunks: Vec<&ChatChunk> = chunks.iter().filter(|c| c.usage.is_some()).collect();
    assert_eq!(usage_chunks.len(), 1, "canonical usage rides one chunk");
    assert!(
        usage_chunks[0]
            .choices
            .iter()
            .any(|ch| ch.finish_reason.is_some()),
        "and that chunk is the terminal one"
    );
}

#[tokio::test]
async fn a_committed_winner_keeps_its_buffered_opening_usage() {
    // Arrange
    let winner = sse_upstream(format!(
        "{}{TEXT_EVENTS}",
        message_start(
            "msg_win",
            WINNER_INPUT,
            WINNER_CACHE_WRITE,
            WINNER_CACHE_READ
        )
    ))
    .await;
    let router = chain_router(vec![("win", anthropic_leg("p-win", &winner.uri()))]);

    // Act
    let chunks = collect(&router).await;

    // Assert
    assert_winner_opening(&chunks);
}

#[tokio::test]
async fn a_failed_attempts_opening_usage_never_reaches_the_caller() {
    // Arrange: the first leg opens (message_start with its own count) and
    // then fails before any content; the fallback leg succeeds.
    let failed = sse_upstream(format!(
        "{}{OVERLOADED_EVENT}",
        message_start("msg_lost", FAILED_LEG_INPUT, 1, 1)
    ))
    .await;
    let winner = sse_upstream(format!(
        "{}{TEXT_EVENTS}",
        message_start(
            "msg_win",
            WINNER_INPUT,
            WINNER_CACHE_WRITE,
            WINNER_CACHE_READ
        )
    ))
    .await;
    let router = chain_router(vec![
        ("lost", anthropic_leg("p-lost", &failed.uri())),
        ("win", anthropic_leg("p-win", &winner.uri())),
    ]);

    // Act
    let chunks = collect(&router).await;

    // Assert
    assert_eq!(
        failed.received_requests().await.map(|r| r.len()),
        Some(1),
        "positive control: the failing leg was dispatched and opened"
    );
    assert!(
        chunks.iter().all(|c| c.id != "msg_lost"),
        "no chunk of the failed attempt leaks"
    );
    assert!(
        openings(&chunks)
            .iter()
            .all(|o| o.input_tokens != FAILED_LEG_INPUT),
        "the failed attempt's opening count must not reach the caller"
    );
    assert_winner_opening(&chunks);
}
