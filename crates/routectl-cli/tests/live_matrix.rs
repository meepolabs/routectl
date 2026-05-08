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
    build_provider, AliasEntry, Config, ProviderEntry, ReasoningDialect, Router,
};

const SHORT_PROMPT: &str = "Reply with just the word: pong";
const MAX_TOKENS_COMPLETE: u32 = 80;
const MAX_TOKENS_STREAM: u32 = 60;
const PER_CALL_TIMEOUT_SECS: u64 = 60;
const PARALLEL_LIMIT: usize = 6;

const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go/v1";
const NIM_BASE: &str = "https://integrate.api.nvidia.com/v1";

/// One representative model per major OpenRouter provider org.
/// Selected by hand to favor cheap / free / small models.
const OPENROUTER_MODELS: &[&str] = &[
    "openai/gpt-4o-mini",
    "openai/gpt-oss-120b:free",
    "anthropic/claude-haiku-4-5",
    "google/gemma-3n-e4b-it",
    "meta-llama/llama-3.2-3b-instruct:free",
    "mistralai/mistral-nemo",
    "deepseek/deepseek-v4-flash",
    "deepseek/deepseek-r1",
    "qwen/qwen3-coder:free",
    "x-ai/grok-3-mini",
    "nvidia/nemotron-nano-9b-v2:free",
    "microsoft/phi-4",
    "cohere/command-r7b-12-2024",
    "minimax/minimax-m2.5:free",
    "moonshotai/kimi-k2-0905",
    "z-ai/glm-4.5-air:free",
    "amazon/nova-micro-v1",
    "perplexity/sonar",
    "arcee-ai/trinity-mini",
    "nousresearch/hermes-3-llama-3.1-405b:free",
];

/// All models exposed under the OpenCode-Go subscription. Discovered by
/// hitting `<base>/models` if `OPENCODE_GO_FETCH_MODELS=1` is set; otherwise
/// uses this static list. Static is the default to keep tests deterministic
/// in CI.
const OPENCODE_GO_MODELS: &[&str] = &[
    "minimax-m2.7",
    "minimax-m2.5",
    "kimi-k2.6",
    "kimi-k2.5",
    "glm-5.1",
    "glm-5",
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "qwen3.6-plus",
    "qwen3.5-plus",
    "mimo-v2-pro",
    "mimo-v2-omni",
    "mimo-v2.5-pro",
    "mimo-v2.5",
];

const NIM_MODELS: &[&str] = &[
    "meta/llama-3.1-8b-instruct",
    "meta/llama-3.3-70b-instruct",
    "meta/llama-4-maverick-17b-128e-instruct",
    "google/gemma-3-12b-it",
    "qwen/qwen3-coder-480b-a35b-instruct",
];

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
            role: Role::User,
            content: MessageContent::Text(SHORT_PROMPT.to_string()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
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
            let msg = &resp.choices.get(0).map(|c| &c.message);
            let content_preview = match msg {
                Some(m) => match &m.content {
                    MessageContent::Text(t) => t.chars().take(40).collect::<String>(),
                    MessageContent::Parts(_) => "<parts>".into(),
                    MessageContent::Null => "<null>".into(),
                },
                None => "<no choice>".into(),
            };
            let rd_count = msg.map(|m| m.reasoning_details.len()).unwrap_or(0);
            let rd_format = msg
                .and_then(|m| m.reasoning_details.first())
                .and_then(|d| d.format.as_deref())
                .unwrap_or("-");
            let has_reasoning = msg.and_then(|m| m.reasoning.as_deref()).is_some();
            let tokens = resp.usage.as_ref().map(|u| u.total_tokens).unwrap_or(0);
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
                            if d.content.as_deref().map_or(false, |s| !s.is_empty()) {
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
                        }
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

/// Build a Router with one provider entry plus aliases, all targets routed
/// through `provider_name`. The MemoryStore feeds the API key directly.
async fn build_test_router(
    provider_name: &str,
    base_url: &str,
    api_key_env: &str,
    dialect: ReasoningDialect,
    targets: &[&str],
    extra_headers: BTreeMap<String, String>,
) -> Option<Arc<Router>> {
    let key = std::env::var(api_key_env).ok()?;
    if key.trim().is_empty() {
        return None;
    }

    // MemoryStore resolves env:// refs by reading the process env directly,
    // so we don't need to write the key into the store.
    let store = MemoryStore::new();
    let secret_uri = format!("env://{api_key_env}");

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ProviderEntry::openai_compat(base_url, secret_uri.clone())
            .with_extra_headers(extra_headers)
            .with_reasoning_dialect(dialect),
    );

    let mut aliases = BTreeMap::new();
    for t in targets {
        aliases.insert(
            (*t).to_string(),
            AliasEntry::new(vec![format!("{provider_name}:{t}")]),
        );
    }

    let cfg = Arc::new(Config {
        server: Default::default(),
        providers,
        aliases,
        retry: Default::default(),
        legacy_compat: Default::default(),
        ingress: Default::default(),
    });

    let mut router = Router::new(cfg.clone());
    for (name, entry) in &cfg.providers {
        let provider = build_provider(name, entry, &store)
            .await
            .expect("build provider");
        router.register(name.clone(), provider);
    }
    Some(Arc::new(router))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opencode_go_complete_matrix() {
    let mut headers = BTreeMap::new();
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "opencode-go",
        OPENCODE_GO_BASE,
        "OPENCODE_GO_API_KEY",
        ReasoningDialect::Deepseek,
        OPENCODE_GO_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENCODE_GO_API_KEY not set");
        return;
    };

    let targets: Vec<String> = OPENCODE_GO_MODELS.iter().map(|s| s.to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("OpenCode-Go", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "OpenCode-Go: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openrouter_complete_matrix() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "HTTP-Referer".into(),
        "https://github.com/meepolabs/routectl".into(),
    );
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "openrouter",
        OPENROUTER_BASE,
        "OPENROUTER_API_KEY",
        ReasoningDialect::Openrouter,
        OPENROUTER_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENROUTER_API_KEY not set");
        return;
    };

    let targets: Vec<String> = OPENROUTER_MODELS.iter().map(|s| s.to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("OpenRouter", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "OpenRouter: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nim_complete_matrix() {
    let Some(router) = build_test_router(
        "nim",
        NIM_BASE,
        "NIM_API_KEY",
        ReasoningDialect::Openai,
        NIM_MODELS,
        BTreeMap::new(),
    )
    .await
    else {
        eprintln!("skip: NIM_API_KEY not set");
        return;
    };

    let targets: Vec<String> = NIM_MODELS.iter().map(|s| s.to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("NIM", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "NIM: 0/{total} models passed -- routectl or provider broken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opencode_go_stream_subset() {
    let Some(router) = build_test_router(
        "opencode-go",
        OPENCODE_GO_BASE,
        "OPENCODE_GO_API_KEY",
        ReasoningDialect::Deepseek,
        OPENCODE_GO_MODELS,
        BTreeMap::new(),
    )
    .await
    else {
        eprintln!("skip: OPENCODE_GO_API_KEY not set");
        return;
    };

    // One per provider family.
    let subset = vec![
        "qwen3.6-plus".to_string(),
        "kimi-k2.6".to_string(),
        "deepseek-v4-flash".to_string(),
        "glm-5.1".to_string(),
    ];
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("OpenCode-Go", "stream", &rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openrouter_stream_subset() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "HTTP-Referer".into(),
        "https://github.com/meepolabs/routectl".into(),
    );
    headers.insert("X-Title".into(), "routectl-live-test".into());
    let Some(router) = build_test_router(
        "openrouter",
        OPENROUTER_BASE,
        "OPENROUTER_API_KEY",
        ReasoningDialect::Openrouter,
        OPENROUTER_MODELS,
        headers,
    )
    .await
    else {
        eprintln!("skip: OPENROUTER_API_KEY not set");
        return;
    };

    // Mix of standard and reasoning models.
    let subset = vec![
        "openai/gpt-4o-mini".to_string(),
        "deepseek/deepseek-r1".to_string(),
        "anthropic/claude-haiku-4-5".to_string(),
        "google/gemma-3n-e4b-it".to_string(),
    ];
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("OpenRouter", "stream", &rows);
}

// -- Bedrock live matrix ----------------------------------------------------
//
// Anthropic-on-Bedrock via InvokeModel + bearer-key auth. Reads
// `AWS_BEARER_TOKEN_BEDROCK` (short-term Bedrock console API key) and
// `AWS_REGION`. Skips cleanly when either is unset.
//
// Run:
//   cargo test -p routectl-cli --features live-integration,bedrock --release \
//     --test live_matrix bedrock -- --nocapture --test-threads=1
//
// Cross-region inference profiles (`us.`-prefixed) are used because they
// have the broadest streaming-permission surface across AWS accounts.

#[cfg(feature = "bedrock")]
const BEDROCK_MODELS: &[&str] = &[
    "us.anthropic.claude-3-5-haiku-20241022-v1:0",
    "us.anthropic.claude-sonnet-4-20250514-v1:0",
    "us.anthropic.claude-opus-4-20250514-v1:0",
    "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
];

#[cfg(feature = "bedrock")]
async fn build_bedrock_test_router(targets: &[&str]) -> Option<Arc<Router>> {
    use routectl_providers::bedrock::{
        auth as bedrock_auth, BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider,
    };

    let key = std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    let region = std::env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "us-east-1".into());

    let mut providers = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut router_providers: Vec<(String, Arc<dyn routectl_core::Provider>)> = Vec::new();

    for model_id in targets {
        let provider_name = format!("bedrock-{}", sanitize_provider_name(model_id));
        let creds = BedrockCreds::BearerKey { key: key.clone() };
        let resolved = bedrock_auth::resolve(&creds, &region)
            .await
            .expect("resolve bedrock bearer creds");
        let cfg = BedrockConfig {
            id: format!("bedrock:{provider_name}"),
            region: region.clone(),
            model_id: (*model_id).to_string(),
            api_shape: BedrockApiShape::Invoke,
            creds,
            user_agent: Some("routectl-live-test/0.4".into()),
            extra_headers: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        };
        let provider: Arc<dyn routectl_core::Provider> =
            Arc::new(BedrockProvider::new(cfg, resolved));

        // The router config still needs an entry per provider; its
        // contents don't matter for the in-memory `register` path
        // because we replace the runtime via `register()` below. Use
        // a placeholder OpenAiCompat entry to satisfy the schema.
        providers.insert(
            provider_name.clone(),
            ProviderEntry::openai_compat("https://placeholder.invalid/v1", "literal:placeholder"),
        );
        aliases.insert(
            (*model_id).to_string(),
            AliasEntry::new(vec![format!("{provider_name}:{model_id}")]),
        );
        router_providers.push((provider_name, provider));
    }

    let cfg = Arc::new(Config {
        server: Default::default(),
        providers,
        aliases,
        retry: Default::default(),
        legacy_compat: Default::default(),
        ingress: Default::default(),
    });

    let mut router = Router::new(cfg);
    for (name, provider) in router_providers {
        router.register(name, provider);
    }
    Some(Arc::new(router))
}

#[cfg(feature = "bedrock")]
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

#[cfg(feature = "bedrock")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bedrock_complete_matrix() {
    let Some(router) = build_bedrock_test_router(BEDROCK_MODELS).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let targets: Vec<String> = BEDROCK_MODELS.iter().map(|s| s.to_string()).collect();
    let total = targets.len();
    let r = router.clone();
    let rows = run_matrix(targets, move |t| {
        let r = r.clone();
        async move { run_complete(r, t).await }
    })
    .await;
    print_summary("Bedrock", "complete", &rows);

    let pass = rows.iter().filter(|r| r.ok).count();
    assert!(
        pass > 0,
        "Bedrock: 0/{total} models passed -- routectl or provider broken"
    );
}

// -- Bedrock ingress-through-bedrock end-to-end ----------------------------
//
// These tests exercise the v0.4.0 hub-and-spoke seam: a wire-format
// request body parsed by an ingress adapter, run through the Router,
// and rendered back into wire format. Both ingress dialects feed the
// same bedrock egress, so this proves N+M (not NxM) translation is
// real and verifies cache_control + anthropic_beta round-trip
// losslessly on a live Anthropic-on-Bedrock request.

#[cfg(feature = "bedrock")]
const BEDROCK_INGRESS_MODEL: &str = "us.anthropic.claude-haiku-4-5-20251001-v1:0";

#[cfg(feature = "bedrock")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_ingress_through_bedrock() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::{openai::OpenAiIngress, IngressAdapter};
    use serde_json::json;

    let Some(router) = build_bedrock_test_router(&[BEDROCK_INGRESS_MODEL]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let body = json!({
        "model": BEDROCK_INGRESS_MODEL,
        "max_tokens": 32,
        "messages": [
            {"role": "system", "content": "Reply with one short word."},
            {"role": "user", "content": "Say pong."}
        ]
    });

    let ingress = OpenAiIngress::default();
    let req = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse openai body");
    let resp = router.complete(req).await.expect("complete via bedrock");
    let wire = ingress.render_response(resp).expect("render openai");

    // OpenAI shape: choices[0].message.content. Note: canonical
    // ChatResponse has no `object` field today, so the wire body
    // omits `object: "chat.completion"`. Most clients tolerate this
    // (Anthropic's official sdk does, opencode does, claude-code does).
    // Tracking the wire-format completeness as a separate follow-up.
    let choices = wire["choices"].as_array().expect("choices array");
    assert!(!choices.is_empty(), "expected at least one choice");
    let content = choices[0]["message"]["content"]
        .as_str()
        .expect("string content");
    assert!(!content.trim().is_empty(), "expected non-empty content");
    println!("openai-ingress -> bedrock: content={content:?}");
}

#[cfg(feature = "bedrock")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_ingress_through_bedrock_cache_and_beta() {
    use axum::http::HeaderMap;
    use routectl_cli::ingress::{anthropic::AnthropicIngress, IngressAdapter};
    use serde_json::json;

    // Use sonnet-4-5 here: its prompt-cache minimum is 1024 tokens,
    // vs haiku models which require 2048+. The matrix already proves
    // haiku works end-to-end; this test specifically verifies the
    // cache_control round-trip, so we use the model with the lower
    // threshold to keep the test reliable.
    let cache_model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
    let Some(router) = build_bedrock_test_router(&[cache_model]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    // System prompt above the 1024-token cache minimum. Repetition
    // is fine -- we only need the byte count to clear the threshold.
    let big_filler = "You are a careful assistant. ".repeat(400);
    let system_text = format!("{big_filler}\n\nRespond to every user message with a single word.",);

    let body = json!({
        "model": cache_model,
        "max_tokens": 16,
        "anthropic_beta": ["interleaved-thinking-2025-05-14"],
        "system": [
            {
                "type": "text",
                "text": system_text,
                "cache_control": {"type": "ephemeral"}
            }
        ],
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "Say pong"}
                ]
            }
        ]
    });

    let ingress = AnthropicIngress::default();

    // First call: should create the cache entry.
    let req1 = ingress
        .parse_request(&HeaderMap::new(), body.clone())
        .expect("parse anthropic body 1");
    let resp1 = router.complete(req1).await.expect("complete 1");
    let wire1 = ingress.render_response(resp1).expect("render anthropic 1");

    assert_eq!(wire1["type"], "message");
    let content1 = wire1["content"].as_array().expect("content array");
    assert!(!content1.is_empty(), "non-empty content");
    assert_eq!(content1[0]["type"], "text");
    let text1 = content1[0]["text"].as_str().expect("text");
    assert!(!text1.trim().is_empty(), "non-empty text");
    let cache_creation_1 = wire1["usage"]
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_1 = wire1["usage"]
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!(
        "anthropic-ingress -> bedrock call#1: text={text1:?} cache_create={cache_creation_1} cache_read={cache_read_1}"
    );

    // Second call (same body): should hit the cache.
    let req2 = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse anthropic body 2");
    let resp2 = router.complete(req2).await.expect("complete 2");
    let wire2 = ingress.render_response(resp2).expect("render anthropic 2");

    let cache_read_2 = wire2["usage"]
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_2 = wire2["usage"]
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!(
        "anthropic-ingress -> bedrock call#2: cache_create={cache_creation_2} cache_read={cache_read_2}"
    );

    // Cache fields must be present in the rendered Anthropic usage
    // (round-trip from upstream usage to ingress wire). On the second
    // call we expect cache_read > 0 unless Bedrock decided the prompt
    // is too small (in which case cache_creation would also be 0 on
    // the first call, surfacing the misconfiguration).
    let any_cache_tokens =
        cache_creation_1 > 0 || cache_read_1 > 0 || cache_creation_2 > 0 || cache_read_2 > 0;
    assert!(
        any_cache_tokens,
        "expected non-zero cache tokens on at least one call -- \
         cache_control did not round-trip to Bedrock or prompt was too small"
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_ingress_streaming_through_bedrock() {
    use axum::http::HeaderMap;
    use futures::StreamExt;
    use routectl_cli::ingress::{anthropic::AnthropicIngress, IngressAdapter};
    use serde_json::json;

    let Some(router) = build_bedrock_test_router(&[BEDROCK_INGRESS_MODEL]).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let body = json!({
        "model": BEDROCK_INGRESS_MODEL,
        "max_tokens": 24,
        "stream": true,
        "messages": [
            {"role": "user", "content": "Say pong."}
        ]
    });

    let ingress = AnthropicIngress::default();
    let req = ingress
        .parse_request(&HeaderMap::new(), body)
        .expect("parse anthropic body");

    let mut state = ingress.new_stream_state();
    let mut upstream = router.stream(req).await.expect("stream via bedrock");
    let mut events: Vec<(Option<String>, String)> = Vec::new();
    while let Some(item) = upstream.next().await {
        let chunk = item.expect("upstream chunk ok");
        for ev in ingress
            .render_chunk(chunk, state.as_mut())
            .expect("render chunk")
        {
            events.push((ev.event, ev.data));
        }
    }
    for ev in ingress.render_eos(state.as_mut()) {
        events.push((ev.event, ev.data));
    }

    let names: Vec<&str> = events
        .iter()
        .filter_map(|(name, _)| name.as_deref())
        .collect();
    println!("anthropic-ingress streaming events: {names:?}");

    assert!(
        names.contains(&"message_start"),
        "expected message_start event, got {names:?}"
    );
    assert!(
        names.contains(&"content_block_start"),
        "expected content_block_start, got {names:?}"
    );
    assert!(
        names.iter().any(|n| *n == "content_block_delta"),
        "expected at least one content_block_delta, got {names:?}"
    );
    assert!(
        names.contains(&"message_stop"),
        "expected message_stop, got {names:?}"
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bedrock_stream_subset() {
    // Stream against the smallest models to keep cost tiny.
    let subset_static: &[&str] = &[
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        "us.anthropic.claude-3-5-haiku-20241022-v1:0",
    ];
    let Some(router) = build_bedrock_test_router(subset_static).await else {
        eprintln!("skip: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    };

    let subset: Vec<String> = subset_static.iter().map(|s| s.to_string()).collect();
    let r = router.clone();
    let rows = run_matrix(subset, move |t| {
        let r = r.clone();
        async move { run_stream(r, t).await }
    })
    .await;
    print_summary("Bedrock", "stream", &rows);
}
