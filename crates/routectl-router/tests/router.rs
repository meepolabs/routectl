//! Router behavior tests with mock Provider impls.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{
    schema::{ChunkChoice, ChunkDelta},
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider, Result,
    Role, Usage,
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
    /// Sleep briefly before returning Ok. Used by concurrency tests to
    /// keep the half-open probe in flight long enough for a sibling
    /// dispatch to race the gate.
    OkSlow,
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
            Behavior::OkSlow => {
                // Slow enough that a concurrent caller is virtually
                // certain to arrive before this future resolves, even
                // on a heavily loaded CI runner. Used by the
                // half-open-single-probe race test.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(ok_response(&self.id, &req.model))
            }
            Behavior::Status(s) => Err(Error::upstream(&self.id, s, "mock")),
            Behavior::Streaming(msg) => Err(Error::Streaming(msg)),
            _ => Err(Error::upstream(&self.id, 500, "unexpected")),
        }
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let id = self.id.clone();
        match self.next_behavior() {
            Behavior::Ok | Behavior::OkSlow | Behavior::Streaming(_) => {
                let chunks = vec![
                    ok_chunk(&id, &req.model, "Hello"),
                    ok_chunk(&id, &req.model, " world"),
                ];
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
        usage: None,
    }
}

fn build_router(aliases: BTreeMap<String, AliasEntry>) -> Router {
    let cfg = Config {
        server: Default::default(),
        providers: BTreeMap::new(),
        aliases,
        retry: {
            let mut r = RetryPolicy::default();
            r.max_attempts = 1;
            r.initial_backoff_ms = 1;
            r.backoff_multiplier = 1.0;
            r.fallback_on_status = vec![429, 500, 502, 503, 504];
            r
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
        ..Default::default()
    }
}

fn alias(chain: &[&str]) -> AliasEntry {
    AliasEntry::new(chain.iter().map(|s| s.to_string()).collect())
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

    let err = r
        .complete(req("fast"))
        .await
        .expect_err("400 should propagate");
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
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 3;
        rp.initial_backoff_ms = 1;
        rp.backoff_multiplier = 1.0;
        rp.fallback_on_status = vec![503];
        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
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
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 1;
        rp.initial_backoff_ms = 1;
        rp.backoff_multiplier = 1.0;
        rp.fallback_on_status = vec![408, 429, 500, 502, 503, 504];
        rp.retry_on_429 = Some(4);
        AliasEntry::new(vec!["p:m".into()]).with_retry(rp)
    });
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
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503), Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 5;
        rp.initial_backoff_ms = 1;
        rp.backoff_multiplier = 1.0;
        rp.fallback_on_status = vec![503];
        rp.retry_on_5xx = Some(1);
        rp.retry_on_429 = Some(5);
        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
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
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 1;
        rp.initial_backoff_ms = 1;
        rp.backoff_multiplier = 1.0;
        rp.fallback_on_status = vec![];
        rp.retry_on_network = Some(3);
        rp.request_timeout_ms = Some(20);
        AliasEntry::new(vec!["slow:m".into()]).with_retry(rp)
    });
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
    let p = MockProvider::new("p", vec![Behavior::Status(503), Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();
        rp.max_attempts = 2;
        rp.initial_backoff_ms = 1;
        rp.backoff_multiplier = 1.0;
        rp.jitter_ms = 5;
        rp.fallback_on_status = vec![503];
        AliasEntry::new(vec!["p:m".into()]).with_retry(rp)
    });
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
            ProviderEntry::openai_compat("http://example.invalid", "literal:x")
                .with_reasoning_dialect(routectl_router::ReasoningDialect::Openai)
                .with_runtime(runtime),
        );
    }
    let cfg = Config {
        server: Default::default(),
        providers,
        aliases,
        retry: {
            let mut r = RetryPolicy::default();
            r.max_attempts = 1;
            r.initial_backoff_ms = 1;
            r.backoff_multiplier = 1.0;
            r
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
        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]),
    );
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.rpm_limit = Some(2);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    // First two go to p1.
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
        Some("p1")
    );
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
        Some("p1")
    );
    // Third hits the bucket limit; falls through to p2.
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
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
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(2);

        rt.circuit_cooldown_ms = Some(30_000);
        rt
    });
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
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
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
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut r = build_router_with_runtime(aliases, BTreeMap::new());
    r.register("p1", p1b);
    r.register("p2", p2b.clone());
    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;
    let err = r
        .complete_with_options(req("fast"), opts)
        .await
        .unwrap_err();
    match err {
        Error::Upstream { status: 503, .. } => {}
        other => panic!("expected 503 from p1, got: {other:?}"),
    }
    assert_eq!(p2b.calls(), 0, "p2 should not have been touched");
}

// ---------------------------------------------------------------------------
// Tier-2 hardening: gates apply per-attempt, not per-routed-request
// ---------------------------------------------------------------------------

/// H1 fix: each retry against the same provider should consume one RPM
/// token. With `rpm_limit = 2` and `retry_on_5xx = 3`, a provider that
/// 503s every time exhausts its bucket on the second attempt and the
/// router falls through to the next chain entry instead of completing
/// all 3 retries against an over-budget provider.
#[tokio::test]
async fn retries_consume_rpm_tokens_and_fall_through_when_bucket_empty() {
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        rp.retry_on_5xx = Some(5);

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.rpm_limit = Some(2);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.unwrap();
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 saw exactly 2 calls before the RPM bucket emptied; the router
    // then fell through to p2.
    assert_eq!(
        p1.calls(),
        2,
        "RPM gate must apply per-attempt, not per-request"
    );
    assert_eq!(p2.calls(), 1);
}

/// H1 fix: each failed attempt should increment the breaker, not the
/// whole routed request. With `circuit_failures = 2`, a provider that
/// 503s repeatedly should trip after the second attempt and the third
/// attempt should hit a CircuitOpen gate (router falls through).
#[tokio::test]
async fn retries_count_toward_circuit_breaker_threshold() {
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        rp.retry_on_5xx = Some(5);

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(2);

        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let resp = r.complete(req("fast")).await.unwrap();
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 saw exactly 2 calls; the third would have been gate-blocked
    // because each retry increments the breaker counter.
    assert_eq!(p1.calls(), 2, "breaker must count each retry as a failure");
    assert_eq!(p2.calls(), 1);
}

/// T fix: client-side errors (400, 401, 404, ...) must NOT charge the
/// circuit breaker. They say nothing about provider health -- they are
/// the caller's mistake (malformed request, wrong auth, unknown model).
/// Repeatedly sending one should propagate the error each time, never
/// quarantine an otherwise-healthy provider.
#[tokio::test]
async fn client_errors_do_not_charge_the_circuit_breaker() {
    // 5 consecutive 400s, but the breaker is configured to trip after 2.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
        ],
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), alias(&["p1:m"]));
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());

    // Five sequential 400s. If client errors charged the breaker, the
    // third call would be gate-blocked and surface a status-0
    // CircuitOpen error instead of the upstream's 400. Assert that
    // every call reaches the provider AND that the upstream 400 is the
    // error every caller sees.
    for i in 0..5 {
        let err = r
            .complete(req("fast"))
            .await
            .expect_err(&format!("call {i} should propagate 400"));
        assert!(
            matches!(err, Error::Upstream { status: 400, .. }),
            "call {i}: expected upstream 400, got {err:?}"
        );
    }
    assert_eq!(
        p1.calls(),
        5,
        "every client-error attempt must reach the provider; breaker must NOT quarantine on 400s"
    );
}

/// H3 fix: a stream that emits one chunk and then errors should NOT
/// mark the provider healthy. The breaker counter should reflect the
/// failure recorded on the in-stream error.
#[tokio::test]
async fn stream_mid_failure_charges_the_breaker() {
    use futures::StreamExt;
    // Provider whose stream emits one chunk then errors mid-stream on
    // every call. Three calls in a row -> breaker should trip after 2
    // failures (per circuit_failures = 2).
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "fast".into(),
        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]),
    );
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(2);

        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    // First request: starts streaming from p1, gets one chunk, errors.
    // Drain the stream so the wrapper records the failure.
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut count = 0;
    while let Some(_) = s.next().await {
        count += 1;
    }
    drop(s);
    assert!(count >= 2, "expected at least one chunk + one error");

    // Second request: same outcome, breaker hits threshold (2).
    let mut s = r.stream(req("fast")).await.expect("stream open");
    while let Some(_) = s.next().await {}
    drop(s);

    // Third request: p1's circuit is now open. The router should
    // gate-block p1 and fall through to p2 without ever calling p1.
    let calls_before = p1.calls();
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut got_chunk = false;
    while let Some(item) = s.next().await {
        if item.is_ok() {
            got_chunk = true;
        }
    }
    assert!(got_chunk, "p2 should answer once p1's breaker is open");
    assert_eq!(
        p1.calls(),
        calls_before,
        "p1 must be skipped while breaker is open"
    );
}

/// H2 fix: under concurrent dispatches after cooldown, only ONE caller
/// should hit the upstream as the half-open probe; the other should
/// see a CircuitOpen gate and fall through.
// `start_paused` requires the `current_thread` runtime, but this test
// is fundamentally about real parallelism between two spawned tasks
// racing on the half-open slot, so we keep the multi-thread runtime
// and use generous wall-clock margins (CI-safe) instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn half_open_probe_is_single_under_concurrent_load() {
    use std::sync::Arc as StdArc;
    // Trip p1's breaker by feeding 2 failures inline.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            // After the breaker trips: deliberately make the probe
            // slow so concurrent callers race the half-open slot.
            Behavior::OkSlow,
            Behavior::OkSlow,
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(2);

        // 250ms cooldown -- generous so the wait below is safely
        // past it even on a contended runner.
        rt.circuit_cooldown_ms = Some(250);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());
    let r = StdArc::new(r);

    // Trip the breaker.
    r.complete(req("fast")).await.unwrap();
    r.complete(req("fast")).await.unwrap();
    // Two p1 calls already done; breaker now open.
    let p1_after_trip = p1.calls();
    assert_eq!(p1_after_trip, 2);

    // Wait for cooldown (250ms cooldown, sleep 350ms).
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    // Fire two concurrent requests. Exactly one should reach p1 as
    // the half-open probe; the other should see CircuitOpen and fall
    // through to p2.
    let r1 = r.clone();
    let r2 = r.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { r1.complete(req("fast")).await }),
        tokio::spawn(async move { r2.complete(req("fast")).await }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();

    let providers: Vec<_> = [a, b]
        .iter()
        .map(|r| r.routectl_provider.clone().unwrap_or_default())
        .collect();
    // Exactly one of the two requests went to p1 (the probe); the
    // other was deflected to p2 by the half-open guard.
    let p1_count = providers.iter().filter(|p| p.as_str() == "p1").count();
    let p2_count = providers.iter().filter(|p| p.as_str() == "p2").count();
    assert_eq!(
        p1_count, 1,
        "exactly one half-open probe expected: {providers:?}"
    );
    assert_eq!(
        p2_count, 1,
        "the other concurrent request must fall through: {providers:?}"
    );
    // p1 saw exactly 1 additional call (the probe) on top of the trip calls.
    assert_eq!(p1.calls(), p1_after_trip + 1);
}

// Paused-time would be ideal here but the breaker tracks cooldowns
// against `std::time::Instant`, which is not affected by Tokio's
// paused-time clock. Use generous wall-clock margins instead.
#[tokio::test]
async fn dropped_stream_releases_half_open_probe_and_reopens_breaker() {
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Ok, Behavior::Ok],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(1);

        // 200ms cooldown -- the sleeps below use 350ms margin so the
        // assertion fires even on a contended runner.
        rt.circuit_cooldown_ms = Some(200);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let first = r.complete(req("fast")).await.unwrap();
    assert_eq!(first.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 1);

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    let stream = r.stream(req("fast")).await.unwrap();
    drop(stream);
    assert_eq!(p1.calls(), 2, "half-open probe should reach p1 once");

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    let recovered = r.complete(req("fast")).await.unwrap();
    assert_eq!(recovered.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(
        p1.calls(),
        3,
        "dropped stream must not leak the half-open slot"
    );
}

#[tokio::test]
async fn dropped_steady_state_stream_does_not_trip_breaker() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok, Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), {
        let mut rp = RetryPolicy::default();

        rp.max_attempts = 1;

        rp.initial_backoff_ms = 1;

        rp.backoff_multiplier = 1.0;

        rp.fallback_on_status = vec![503];

        AliasEntry::new(vec!["p1:m".into(), "p2:m".into()]).with_retry(rp)
    });
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();

        rt.circuit_failures = Some(1);

        rt.circuit_cooldown_ms = Some(50);
        rt
    });
    let mut r = build_router_with_runtime(aliases, runtime);
    r.register("p1", p1.clone());
    r.register("p2", p2.clone());

    let mut stream = r.stream(req("fast")).await.unwrap();
    let first = stream.next().await.transpose().unwrap();
    assert!(first.is_some(), "expected first chunk before cancel");
    drop(stream);

    let recovered = r.complete(req("fast")).await.unwrap();
    assert_eq!(recovered.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(
        p1.calls(),
        2,
        "client cancel after first chunk must not open the breaker"
    );
    assert_eq!(p2.calls(), 0);
}
