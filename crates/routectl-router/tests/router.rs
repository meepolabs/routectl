//! Router behavior tests with mock Provider impls.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{
    schema::{ChunkChoice, ChunkDelta},
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider,
    Result, Role, Usage,
};
use routectl_router::{
    AliasEntry, Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy, Router, RouterOptions,
};

/// Mock provider whose behavior is parameterized per-call.
struct MockProvider {
    id: String,
    behaviors: Vec<Behavior>,
    call_count: AtomicUsize,
}

#[derive(Clone)]
enum Behavior {
    Ok,
    Status(u16),
    Streaming(String),
    StreamFirstChunkErrors(u16),
    StreamMidErrors,
}

impl MockProvider {
    fn new(id: &str, behaviors: Vec<Behavior>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            behaviors,
            call_count: AtomicUsize::new(0),
        })
    }

    fn next_behavior(&self) -> Behavior {
        let i = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.behaviors
            .get(i)
            .cloned()
            .unwrap_or_else(|| self.behaviors.last().cloned().unwrap_or(Behavior::Ok))
    }

    fn calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> {
        Ok(None)
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        match self.next_behavior() {
            Behavior::Ok => Ok(ok_response(&self.id, &req.model)),
            Behavior::Status(s) => Err(Error::upstream(&self.id, s, "mock")),
            Behavior::Streaming(msg) => Err(Error::Streaming(msg)),
            _ => Err(Error::upstream(&self.id, 500, "unexpected")),
        }
    }
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let id = self.id.clone();
        match self.next_behavior() {
            Behavior::Ok | Behavior::Streaming(_) => {
                let chunks = vec![ok_chunk(&id, &req.model, "Hello"), ok_chunk(&id, &req.model, " world")];
                Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
            }
            Behavior::Status(s) => Err(Error::upstream(&id, s, "open-stream-error")),
            Behavior::StreamFirstChunkErrors(s) => {
                let err = Error::upstream(&id, s, "first-chunk-error");
                Ok(futures::stream::once(async move { Err(err) }).boxed())
            }
            Behavior::StreamMidErrors => {
                let first = ok_chunk(&id, &req.model, "first");
                let mid_err = Error::Streaming("mid-stream".into());
                let s = futures::stream::iter(vec![Ok(first), Err(mid_err)]);
                Ok(s.boxed())
            }
        }
    }
}

fn ok_response(id: &str, model: &str) -> ChatResponse {
    ChatResponse {
        id: format!("resp-{id}"),
        model: model.to_string(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Some(Usage::default()),
        routectl_provider: None,
    }
}

fn ok_chunk(id: &str, model: &str, content: &str) -> ChatChunk {
    ChatChunk {
        id: format!("chunk-{id}"),
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(content.into()),
                ..Default::default()
            },
            finish_reason: None,
        }],
    }
}

fn build_router(aliases: BTreeMap<String, AliasEntry>) -> Router {
    let cfg = Config {
        server: Default::default(),
        providers: BTreeMap::new(),
        aliases,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            fallback_on_status: vec![429, 500, 502, 503, 504],
            ..Default::default()
        },
        legacy_compat: Default::default(),
    };
    Router::new(Arc::new(cfg))
}

fn req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop: None,
        stream: None,
        n: None,
        seed: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        reasoning: None,
        chat_template_kwargs: None,
        provider_extras: None,
    }
}

fn alias(chain: &[&str]) -> AliasEntry {
    AliasEntry {
        chain: chain.iter().map(|s| s.to_string()).collect(),
        retry: None,
    }
}

#[tokio::test]
async fn complete_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m1", "p2:m2"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn complete_falls_back_on_5xx() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_falls_back_on_429() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(429)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_does_not_fall_back_on_4xx_other_than_429() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(400)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let err = r.complete(req("fast")).await.expect_err("400 should propagate");
    assert!(matches!(err, Error::Upstream { status: 400, .. }));
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn complete_all_providers_fail_returns_last_error() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Status(502)]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1);
    r.register("p2", p2);

    let err = r.complete(req("fast")).await.expect_err("all-fail");
    assert!(matches!(err, Error::Upstream { status: 502, .. }));
}

#[tokio::test]
async fn complete_unknown_alias_errors() {
    let r = build_router(BTreeMap::new());
    let err = r.complete(req("nothing")).await.expect_err("unknown alias");
    assert!(matches!(err, Error::UnknownAlias(_)));
}

#[tokio::test]
async fn complete_direct_provider_target_works() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let mut r = build_router(BTreeMap::new());
    r.register("p1", p1);
    let resp = r.complete(req("p1:any")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
}

#[tokio::test]
async fn complete_retries_within_provider_then_falls_back() {
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Status(503), Behavior::Status(503)],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 3,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![503],
                ..Default::default()
            }),
        },
    );
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 3);
}

#[tokio::test]
async fn stream_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2);
    assert_eq!(p2.calls(), 0);
}

#[tokio::test]
async fn stream_falls_back_when_first_chunk_errors() {
    let p1 = MockProvider::new("p1", vec![Behavior::StreamFirstChunkErrors(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2, "p2 should have been used");
}

#[tokio::test]
async fn stream_falls_back_when_open_stream_call_fails() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1);
    r.register("p2", p2);

    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("chunk ok");
        count += 1;
    }
    assert_eq!(count, 2);
}

#[tokio::test]
async fn stream_propagates_mid_stream_error_no_fallback() {
    let p1 = MockProvider::new("p1", vec![Behavior::StreamMidErrors]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m", "p2:m"]));
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let mut s = r.stream(req("fast")).await.expect("ok");
    let first = s.next().await.expect("first chunk").expect("ok");
    let _ = first;
    let second = s.next().await.expect("second item");
    assert!(matches!(second, Err(Error::Streaming(_))));
    // p2 was never used because we already started streaming from p1.
    assert_eq!(p2.calls(), 0);
}

// ---------------------------------------------------------------------------
// Tier 1: timeouts + jitter + per-error retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_on_429_overrides_max_attempts() {
    // max_attempts = 1, but retry_on_429 = 4 -> 429 retries 4x.
    let p = MockProvider::new(
        "p",
        vec![
            Behavior::Status(429),
            Behavior::Status(429),
            Behavior::Status(429),
            Behavior::Ok,
        ],
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![408, 429, 500, 502, 503, 504],
                retry_on_429: Some(4),
                ..Default::default()
            }),
        },
    );
    let mut r = build_router(aliases);
    r.register("p", p.clone());
    let resp = r.complete(req("fast")).await.expect("ok after retries");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(p.calls(), 4);
}

#[tokio::test]
async fn retry_on_5xx_independent_of_429() {
    // 5xx retries get 1 attempt; 429 retries get 5. A 5xx run that
    // exhausts its budget falls through.
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Status(503)],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 5,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![503],
                retry_on_5xx: Some(1),
                retry_on_429: Some(5),
                ..Default::default()
            }),
        },
    );
    let mut r = build_router(aliases);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());
    let resp = r.complete(req("fast")).await.expect("eventually ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 made one attempt (per retry_on_5xx=1), then fell through.
    assert_eq!(p1.calls(), 1);
}

#[tokio::test]
async fn request_timeout_translates_to_network_error_and_retries() {
    use std::time::Duration;
    // Provider that sleeps before completing; timeout fires first.
    struct SlowProvider {
        id: String,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for SlowProvider {
        fn id(&self) -> &str { &self.id }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        fn normalize_chunk(&self, _: &str) -> Result<Option<ChatChunk>> { Ok(None) }
        async fn complete(&self, _req: ChatRequest) -> Result<ChatResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            // First two calls hang past the timeout; third returns fast.
            if n < 2 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(ok_response(&self.id, "m"))
        }
        async fn stream(&self, _req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }
    let p = Arc::new(SlowProvider {
        id: "slow".into(),
        calls: AtomicUsize::new(0),
    });
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["slow:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![],
                retry_on_network: Some(3),
                request_timeout_ms: Some(20),
                ..Default::default()
            }),
        },
    );
    let mut r = build_router(aliases);
    r.register("slow", p.clone());
    let resp = r.complete(req("fast")).await.expect("eventually ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("slow"));
    // Two timeouts retried, then a fast OK.
    assert_eq!(p.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn per_attempt_jitter_does_not_break_retries() {
    // Smoke test: jitter_ms > 0 doesn't crash the retry loop.
    let p = MockProvider::new(
        "p",
        vec![Behavior::Status(503), Behavior::Ok],
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 2,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                jitter_ms: 5,
                fallback_on_status: vec![503],
                ..Default::default()
            }),
        },
    );
    let mut r = build_router(aliases);
    r.register("p", p.clone());
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(p.calls(), 2);
}

// ---------------------------------------------------------------------------
// Tier 2: per-provider RPM, circuit breaker, disable-fallbacks
// ---------------------------------------------------------------------------

fn build_router_with_runtime(
    aliases: BTreeMap<String, AliasEntry>,
    provider_runtime: BTreeMap<String, ProviderRuntimePolicy>,
) -> Router {
    let mut providers = BTreeMap::new();
    for (name, runtime) in provider_runtime {
        // The factory path is bypassed in tests; we stuff a dummy
        // OpenaiCompat entry here so Router::new picks up the runtime.
        providers.insert(
            name,
            ProviderEntry::OpenaiCompat {
                base_url: "http://example.invalid".into(),
                api_key_ref: "literal:x".into(),
                extra_headers: BTreeMap::new(),
                default_extras: None,
                reasoning_dialect: routectl_router::ReasoningDialect::Openai,
                runtime,
            },
        );
    }
    let cfg = Config {
        server: Default::default(),
        providers,
        aliases,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            ..Default::default()
        },
        legacy_compat: Default::default(),
    };
    Router::new(Arc::new(cfg))
}

#[tokio::test]
async fn rpm_limit_falls_through_to_next_provider() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: None,
        },
    );
    let mut runtime = BTreeMap::new();
    runtime.insert(
        "p1".into(),
        ProviderRuntimePolicy {
            rpm_limit: Some(2),
            ..Default::default()
        },
    );
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    // First two go to p1.
    assert_eq!(
        r.complete(req("fast")).await.unwrap().routectl_provider.as_deref(),
        Some("p1")
    );
    assert_eq!(
        r.complete(req("fast")).await.unwrap().routectl_provider.as_deref(),
        Some("p1")
    );
    // Third hits the bucket limit; falls through to p2.
    assert_eq!(
        r.complete(req("fast")).await.unwrap().routectl_provider.as_deref(),
        Some("p2")
    );
    assert_eq!(p1.calls(), 2);
    assert_eq!(p2.calls(), 1);
}

#[tokio::test]
async fn circuit_breaker_skips_provider_after_consecutive_failures() {
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Status(503), Behavior::Ok],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![503],
                ..Default::default()
            }),
        },
    );
    let mut runtime = BTreeMap::new();
    runtime.insert(
        "p1".into(),
        ProviderRuntimePolicy {
            circuit_failures: Some(2),
            circuit_cooldown_ms: Some(30_000),
            ..Default::default()
        },
    );
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    // First two requests trip the breaker on p1, both end up on p2.
    let r1 = r.complete(req("fast")).await.unwrap();
    let r2 = r.complete(req("fast")).await.unwrap();
    assert_eq!(r1.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(r2.routectl_provider.as_deref(), Some("p2"));
    // Third request: breaker is open, p1 is skipped silently and p2
    // serves it without p1 being called again.
    let r3 = r.complete(req("fast")).await.unwrap();
    assert_eq!(r3.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 2);
    assert_eq!(p2.calls(), 3);
}

#[tokio::test]
async fn disable_fallbacks_propagates_first_error() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![503],
                ..Default::default()
            }),
        },
    );
    let mut r = build_router_with_runtime(aliases, BTreeMap::new());
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    // Without options: falls back to p2.
    let ok = r.complete(req("fast")).await.unwrap();
    assert_eq!(ok.routectl_provider.as_deref(), Some("p2"));

    // With disable_fallbacks: error from p1 propagates verbatim.
    let p1b = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2b = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry {
            chain: vec!["p1:m".into(), "p2:m".into()],
            retry: Some(RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                fallback_on_status: vec![503],
                ..Default::default()
            }),
        },
    );
    let mut r = build_router_with_runtime(aliases, BTreeMap::new());
    r.register("p1", p1b);
    r.register("p2", p2b.clone());
    let err = r
        .complete_with_options(
            req("fast"),
            RouterOptions {
                disable_fallbacks: true,
            },
        )
        .await
        .unwrap_err();
    match err {
        Error::Upstream { status: 503, .. } => {}
        other => panic!("expected 503 from p1, got: {other:?}"),
    }
    assert_eq!(p2b.calls(), 0, "p2 should not have been touched");
}
