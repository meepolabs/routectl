//! Live provider matrix tests.
//!
//! Hits real providers using API keys from the environment, exercises every
//! model on a curated list, and prints a report per provider. The goal is to
//! quickly answer:
//!
//!   1. Which models reach completion through routectl?
//!   2. For reasoning-capable models, does routectl lift reasoning into
//!      `reasoning_details[]` correctly?
//!   3. Does streaming work end-to-end?
//!
//! Tests skip cleanly when their key is absent. Individual model failures are
//! reported but do NOT fail the test (rate limits / model deprecation /
//! provider outages would cause flakes); a sanity gate fails the test only
//! when zero models pass for a provider whose key WAS set.
//!
//! Run with:
//!
//!   cargo test -p routectl-cli --features live-integration --release \
//!     --test live_matrix -- --nocapture --test-threads=1
//!
//! Setting `--test-threads=1` keeps the per-provider reports legible.

#![cfg(feature = "live-integration")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use routectl_auth::MemoryStore;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::{
    AliasValue, BuildOptions, Config, ModelEntry, ProviderEntry, ReasoningDialect, Router,
    build_resolved_models,
};

const SHORT_PROMPT: &str = "Reply with just the word: pong";
const MAX_TOKENS_COMPLETE: u32 = 80;
const MAX_TOKENS_STREAM: u32 = 60;
const PER_CALL_TIMEOUT_SECS: u64 = 60;
const PARALLEL_LIMIT: usize = 6;

// Shared by the openai_responses and oauth_codex scenarios, so it lives at
// the crate root rather than inside either module.
const OPENAI_RESPONSES_BASE: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Debug)]
struct Row {
    target: String,
    ok: bool,
    elapsed_ms: u128,
    detail: String,
}

impl Row {
    fn print(&self) {
        let flag = if self.ok { "PASS" } else { "FAIL" };
        println!(
            "  {flag:<4}  {:<55}  {:>5}ms  {}",
            self.target, self.elapsed_ms, self.detail
        );
    }
}

fn make_request(target: &str, max_tokens: u32, stream: bool) -> ChatRequest {
    ChatRequest {
        model: target.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(SHORT_PROMPT.to_string()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(max_tokens),
        stream: Some(stream),
        ..Default::default()
    }
}

async fn run_complete(router: Arc<Router>, target: String) -> Row {
    let req = make_request(&target, MAX_TOKENS_COMPLETE, false);
    let start = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(PER_CALL_TIMEOUT_SECS),
        router.complete(req),
    )
    .await;
    let elapsed = start.elapsed().as_millis();
    match res {
        Ok(Ok(resp)) => {
            let msg = &resp.choices.first().map(|c| &c.message);
            let content_preview = match msg {
                Some(m) => match &m.content {
                    MessageContent::Text(t) => t.chars().take(40).collect::<String>(),
                    MessageContent::Parts(_) => "<parts>".into(),
                    MessageContent::Null => "<null>".into(),
                },
                None => "<no choice>".into(),
            };
            let rd_count = msg.map_or(0, |m| m.reasoning_details.len());
            let rd_format = msg
                .and_then(|m| m.reasoning_details.first())
                .and_then(|d| d.format.as_deref())
                .unwrap_or("-");
            let has_reasoning = msg.and_then(|m| m.reasoning.as_deref()).is_some();
            let tokens = resp.usage.as_ref().map_or(0, |u| u.total_tokens);
            Row {
                target,
                ok: true,
                elapsed_ms: elapsed,
                detail: format!(
                    "tokens={tokens} rd={rd_count} fmt={rd_format} reason_field={has_reasoning} content={content_preview:?}"
                ),
            }
        }
        Ok(Err(e)) => Row {
            target,
            ok: false,
            elapsed_ms: elapsed,
            detail: format!("err: {e}").chars().take(180).collect(),
        },
        Err(_) => Row {
            target,
            ok: false,
            elapsed_ms: elapsed,
            detail: format!("timeout after {PER_CALL_TIMEOUT_SECS}s"),
        },
    }
}

async fn run_stream(router: Arc<Router>, target: String) -> Row {
    let req = make_request(&target, MAX_TOKENS_STREAM, true);
    let start = Instant::now();
    let stream_res = tokio::time::timeout(
        Duration::from_secs(PER_CALL_TIMEOUT_SECS),
        router.stream(req),
    )
    .await;
    let mut content_chunks = 0usize;
    let mut reasoning_chunks = 0usize;
    let mut first_byte_ms: Option<u128> = None;
    let mut total_chunks = 0usize;
    match stream_res {
        Ok(Ok(mut s)) => {
            while let Ok(Some(item)) =
                tokio::time::timeout(Duration::from_secs(PER_CALL_TIMEOUT_SECS), s.next()).await
            {
                if first_byte_ms.is_none() {
                    first_byte_ms = Some(start.elapsed().as_millis());
                }
                total_chunks += 1;
                match item {
                    Ok(chunk) => {
                        for ch in &chunk.choices {
                            let d = &ch.delta;
                            if d.content.as_deref().is_some_and(|s| !s.is_empty()) {
                                content_chunks += 1;
                            }
                            if d.reasoning.is_some() || !d.reasoning_details.is_empty() {
                                reasoning_chunks += 1;
                            }
                        }
                    }
                    Err(e) => {
                        return Row {
                            target,
                            ok: false,
                            elapsed_ms: start.elapsed().as_millis(),
                            detail: format!(
                                "stream-err after {total_chunks} chunks: {}",
                                e.to_string().chars().take(140).collect::<String>()
                            ),
                        };
                    }
                }
            }
            let elapsed = start.elapsed().as_millis();
            Row {
                target,
                ok: total_chunks > 0,
                elapsed_ms: elapsed,
                detail: format!(
                    "ttfb={}ms chunks={total_chunks} content={content_chunks} reasoning={reasoning_chunks}",
                    first_byte_ms.unwrap_or(elapsed)
                ),
            }
        }
        Ok(Err(e)) => Row {
            target,
            ok: false,
            elapsed_ms: start.elapsed().as_millis(),
            detail: format!("stream-init-err: {e}").chars().take(180).collect(),
        },
        Err(_) => Row {
            target,
            ok: false,
            elapsed_ms: start.elapsed().as_millis(),
            detail: format!("stream-init-timeout after {PER_CALL_TIMEOUT_SECS}s"),
        },
    }
}

async fn run_matrix<F, Fut>(targets: Vec<String>, run: F) -> Vec<Row>
where
    F: Fn(String) -> Fut + Clone,
    Fut: std::future::Future<Output = Row>,
{
    let mut in_flight = FuturesUnordered::new();
    let mut iter = targets.into_iter();
    let mut rows = Vec::new();

    for _ in 0..PARALLEL_LIMIT {
        if let Some(t) = iter.next() {
            in_flight.push(run.clone()(t));
        }
    }
    while let Some(row) = in_flight.next().await {
        rows.push(row);
        if let Some(t) = iter.next() {
            in_flight.push(run.clone()(t));
        }
    }
    rows.sort_by(|a, b| a.target.cmp(&b.target));
    rows
}

fn print_summary(label: &str, mode: &str, rows: &[Row]) {
    let pass = rows.iter().filter(|r| r.ok).count();
    let total = rows.len();
    println!("\n=== {label} ({mode}) ===");
    for row in rows {
        row.print();
    }
    println!("  -> {pass}/{total} pass");
}

fn sanitize_provider_name(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[path = "live_matrix/bedrock_converse.rs"]
mod bedrock_converse;
#[path = "live_matrix/bedrock_invoke.rs"]
mod bedrock_invoke;
#[path = "live_matrix/gemini.rs"]
mod gemini;
#[path = "live_matrix/mantle_anthropic.rs"]
mod mantle_anthropic;
#[path = "live_matrix/mantle_chat_completions.rs"]
mod mantle_chat_completions;
#[path = "live_matrix/mantle_responses.rs"]
mod mantle_responses;
#[path = "live_matrix/oauth_antigravity.rs"]
mod oauth_antigravity;
#[path = "live_matrix/oauth_codex.rs"]
mod oauth_codex;
#[path = "live_matrix/openai_compat.rs"]
mod openai_compat;
#[path = "live_matrix/openai_responses.rs"]
mod openai_responses;
#[path = "live_matrix/responses_ingress_live.rs"]
mod responses_ingress_live;
