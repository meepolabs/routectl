//! Router behavior tests with mock Provider impls.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{
    schema::{ChunkChoice, ChunkDelta},
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, OpaqueSseEvent,
    Provider, Result, Role, Usage,
};
use routectl_router::{
    AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, ResolvedModel, RetryPolicy, Router,
    RouterOptions,
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
    StreamFirstChunkErrors(u16),
    StreamMidErrors,
    /// `provider.stream()` returns Ok, but the stream yields zero
    /// chunks before EOS. Pins the contract that the router treats
    /// this as a fallbackable streaming error rather than a clean
    /// empty completion.
    StreamEmpty,
    /// `provider.stream()` returns Ok and the stream yields chunks
    /// carrying `opaque_events` (capture path for unknown
    /// `content_block` types preserved verbatim) interleaved with a
    /// normal text chunk. Before forward-compat handling, a
    /// `server_tool_use` block crashed SSE deserialization with
    /// `Error::Streaming` and the router's `should_fallback`
    /// returned true, walking the chain across providers for a
    /// local forward-compat bug. This behavior pins the contract
    /// that opaque-event chunks pass through cleanly without
    /// triggering chain-walk.
    StreamWithOpaqueEvents,
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
            _ => Err(Error::upstream(&self.id, 500, "unexpected")),
        }
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let id = self.id.clone();
        match self.next_behavior() {
            Behavior::Ok | Behavior::OkSlow => {
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
            Behavior::StreamEmpty => Ok(futures::stream::empty().boxed()),
            Behavior::StreamWithOpaqueEvents => {
                // Emit an opaque-only chunk (capture path for unknown
                // `server_tool_use` block start + stop, no canonical
                // delta), then a normal text chunk. The router must
                // pass these through unchanged without raising
                // `Error::Streaming` and without walking the chain.
                let opaque_chunk = ChatChunk {
                    id: format!("chunk-{id}"),
                    model: req.model.clone(),
                    choices: Vec::new(),
                    usage: None,
                    opaque_events: vec![
                        OpaqueSseEvent::ContentBlockStart {
                            upstream_index: 0,
                            type_tag: "server_tool_use".into(),
                            raw_data: br#"{"type":"server_tool_use","id":"srvtoolu_01","name":"web_search","input":{}}"#.to_vec(),
                        },
                        OpaqueSseEvent::ContentBlockStop { upstream_index: 0 },
                    ],
                };
                let text_chunk = ok_chunk(&id, &req.model, "Hello");
                let chunks = vec![opaque_chunk, text_chunk];
                Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
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
            matched_stop_sequence: None,
        }],
        usage: Some(Usage::default()),
        routectl_provider: None,
        extras: Default::default(),
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
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
    }
}

/// Default top-level retry policy used by tests that don't otherwise
/// override the retry shape -- single attempt, fast backoff, fall
/// back on the standard set of retryable statuses.
fn default_test_retry() -> RetryPolicy {
    let mut r = RetryPolicy::default();
    r.max_attempts = 1;
    r.initial_backoff_ms = 1;
    r.backoff_multiplier = 1.0;
    r.retry_allowlist = vec![429, 500, 502, 503, 504];
    r
}

/// Build a router from an alias map (wire-string -> nickname chain),
/// a list of `(nickname, provider_name, upstream)` tuples, and a
/// pre-registered set of `Arc<dyn Provider>` instances keyed by
/// `provider_name`. The `[retry]` policy is the default test shape;
/// pass `with_retry` for cases that need a custom shape.
fn build_router_v6(
    aliases: BTreeMap<String, AliasValue>,
    models: Vec<(String, String, String)>,
    providers: Vec<(String, Arc<dyn Provider>)>,
) -> Router {
    build_router_v6_with_retry(aliases, models, providers, default_test_retry())
}

fn build_router_v6_with_retry(
    aliases: BTreeMap<String, AliasValue>,
    models: Vec<(String, String, String)>,
    providers: Vec<(String, Arc<dyn Provider>)>,
    retry: RetryPolicy,
) -> Router {
    build_router_v6_full(aliases, models, providers, retry, BTreeMap::new())
}

fn build_router_v6_full(
    aliases: BTreeMap<String, AliasValue>,
    models: Vec<(String, String, String)>,
    providers: Vec<(String, Arc<dyn Provider>)>,
    retry: RetryPolicy,
    provider_runtime: BTreeMap<String, ProviderRuntimePolicy>,
) -> Router {
    let mut config_providers = BTreeMap::new();
    for (name, runtime) in provider_runtime {
        config_providers.insert(
            name,
            ProviderEntry::openai_compat("http://example.invalid", "literal:x")
                .with_runtime(runtime),
        );
    }

    let cfg = Config {
        server: Default::default(),
        providers: config_providers,
        aliases,
        retry,
        legacy_compat: Default::default(),
        ..Default::default()
    };
    let mut router = Router::new(Arc::new(cfg));

    let provider_map: BTreeMap<String, Arc<dyn Provider>> = providers.into_iter().collect();
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, provider_name, upstream) in models {
        let provider = provider_map
            .get(&provider_name)
            .cloned()
            .expect("provider must be registered before resolved-model build");
        resolved.insert(
            nickname.clone(),
            Arc::new(ResolvedModel::new(
                nickname,
                provider_name,
                provider,
                upstream,
            )),
        );
    }
    router.install_resolved_models(resolved);
    router
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

/// Helper: build an alias map that points `key` at a chain of model
/// nicknames.
fn chain_alias(key: &str, nicknames: &[&str]) -> (String, AliasValue) {
    if nicknames.len() == 1 {
        (key.into(), AliasValue::Single(nicknames[0].into()))
    } else {
        (
            key.into(),
            AliasValue::Chain(nicknames.iter().map(|s| (*s).into()).collect()),
        )
    }
}

#[tokio::test]
async fn complete_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m1".into()),
            ("m2".into(), "p2".into(), "m2".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_falls_back_on_429() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(429)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn complete_does_not_fall_back_on_4xx_other_than_429() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(400)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
    );

    let err = r.complete(req("fast")).await.expect_err("all-fail");
    assert!(matches!(err, Error::Upstream { status: 502, .. }));
}

#[tokio::test]
async fn complete_unknown_alias_errors() {
    let r = build_router_v6(BTreeMap::new(), vec![], vec![]);
    let err = r.complete(req("nothing")).await.expect_err("unknown alias");
    assert!(matches!(err, Error::UnknownAlias(_)));
}

#[tokio::test]
async fn complete_direct_nickname_target_works() {
    // v0.6.0: the wire model can be a direct `[models]` table key,
    // bypassing the `[aliases]` table. (This replaces the old
    // `provider:model` literal escape hatch from v0.5.)
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let r = build_router_v6(
        BTreeMap::new(),
        vec![("m1".into(), "p1".into(), "wire-model".into())],
        vec![("p1".into(), p1 as Arc<dyn Provider>)],
    );
    let resp = r.complete(req("m1")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(resp.model, "wire-model");
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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 3;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 3);
}

#[tokio::test]
async fn stream_first_provider_succeeds() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let mut s = r.stream(req("fast")).await.expect("ok");
    let first = s.next().await.expect("first chunk").expect("ok");
    let _ = first;
    let second = s.next().await.expect("second item");
    assert!(matches!(second, Err(Error::Streaming(_))));
    // p2 was never used because we already started streaming from p1.
    assert_eq!(p2.calls(), 0);
}

/// Before forward-compat handling, an Anthropic SSE stream containing
/// an unknown `content_block` type (e.g. `server_tool_use`) crashed
/// deserialization with `Error::Streaming`; the router's
/// `should_fallback` returned true and the chain walked across
/// providers, multiplying upstream calls for a local forward-compat
/// bug (production logs showed 11+ retries / 3 minutes). With the
/// catchall + sink-drain plus opaque-event replay in place,
/// unknown variants travel through the canonical chunk
/// stream as `opaque_events` payload and the router never sees
/// `Error::Streaming`. This test pins the router-side regression
/// gate: a streaming response carrying opaque events completes
/// cleanly and the backstop provider is NEVER touched.
#[tokio::test]
async fn stream_with_unknown_anthropic_block_does_not_walk_chain() {
    // Arrange: chain with primary + backstop. Primary emits an
    // opaque-only chunk (server_tool_use start/stop) followed by a
    // normal text chunk. Backstop is wired with `Ok` behavior so a
    // regression that walks the chain produces visible call-count
    // drift instead of a confusingly-empty failure.
    let primary = MockProvider::new("primary", vec![Behavior::StreamWithOpaqueEvents]);
    let backstop = MockProvider::new("backstop", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "primary".into(), "m".into()),
            ("m2".into(), "backstop".into(), "m".into()),
        ],
        vec![
            ("primary".into(), primary.clone() as Arc<dyn Provider>),
            ("backstop".into(), backstop.clone() as Arc<dyn Provider>),
        ],
    );

    // Act: drain the stream to completion. Each item must be Ok --
    // a single Err here means the regression is back.
    let mut s = r.stream(req("fast")).await.expect("ok");
    let mut count = 0;
    while let Some(item) = s.next().await {
        let _ = item.expect("opaque-event chunks must not surface as Err");
        count += 1;
    }

    // Assert: stream completed without error, primary served the
    // entire response, and the backstop was never reached.
    assert_eq!(count, 2, "expected opaque-only chunk + text chunk");
    assert_eq!(primary.calls(), 1, "primary should be called exactly once");
    assert_eq!(
        backstop.calls(),
        0,
        "backstop must NEVER be called -- chain-walk regression gate",
    );
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
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![408, 429, 500, 502, 503, 504];
    rp.retry_on_429 = Some(4);
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "p".into(), "m".into())],
        vec![("p".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 5;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    rp.retry_on_5xx = Some(1);
    rp.retry_on_429 = Some(5);
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
    );
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
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![];
    rp.retry_on_network = Some(3);
    rp.request_timeout_ms = Some(20);
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "slow".into(), "m".into())],
        vec![("slow".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
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
    let (k, v) = chain_alias("fast", &["m"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 2;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.jitter_ms = 5;
    rp.retry_allowlist = vec![503];
    let r = build_router_v6_with_retry(
        aliases,
        vec![("m".into(), "p".into(), "m".into())],
        vec![("p".into(), p.clone() as Arc<dyn Provider>)],
        rp,
    );
    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(p.calls(), 2);
}

// ---------------------------------------------------------------------------
// Tier 2: per-provider RPM, circuit breaker, disable-fallbacks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpm_limit_falls_through_to_next_provider() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.rpm_limit = Some(2);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        default_test_retry(),
        runtime,
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(30_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

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
    // First subtest: without disable_fallbacks, falls back to p2.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let r = build_router_v6_with_retry(
        aliases.clone(),
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp.clone(),
    );
    let ok = r.complete(req("fast")).await.unwrap();
    assert_eq!(ok.routectl_provider.as_deref(), Some("p2"));

    // Second subtest: with disable_fallbacks, error from p1 propagates.
    let p1b = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2b = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1b as Arc<dyn Provider>),
            ("p2".into(), p2b.clone() as Arc<dyn Provider>),
        ],
        rp,
    );
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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    rp.retry_on_5xx = Some(5);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.rpm_limit = Some(2);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    rp.retry_on_5xx = Some(5);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

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
    let (k, v) = chain_alias("fast", &["m1"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![("m1".into(), "p1".into(), "m".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
        default_test_retry(),
        runtime,
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        default_test_retry(),
        runtime,
    );

    // First request: starts streaming from p1, gets one chunk, errors.
    // Drain the stream so the wrapper records the failure.
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut count = 0;
    while s.next().await.is_some() {
        count += 1;
    }
    drop(s);
    assert!(count >= 2, "expected at least one chunk + one error");

    // Second request: same outcome, breaker hits threshold (2).
    let mut s = r.stream(req("fast")).await.expect("stream open");
    while s.next().await.is_some() {}
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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        // 250ms cooldown -- generous so the wait below is safely
        // past it even on a contended runner.
        rt.circuit_cooldown_ms = Some(250);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );
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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(1);
        // 200ms cooldown -- the sleeps below use 350ms margin so the
        // assertion fires even on a contended runner.
        rt.circuit_cooldown_ms = Some(200);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

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
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_allowlist = vec![503];
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(1);
        rt.circuit_cooldown_ms = Some(50);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

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

// ---------------------------------------------------------------------------
// `default` alias key (v0.6.0 replacement for v0.5's `default_model`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_alias_routes_unknown_model_to_default_chain() {
    // Client sends a model name that's not in [aliases] and isn't a
    // direct nickname. With aliases."default" pointing at "m1", the
    // request should land on m1's provider.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("m1".into()));
    let r = build_router_v6(
        aliases,
        vec![("m1".into(), "p1".into(), "m1".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
    );

    let resp = r
        .complete(req("claude-future-model-99-20300101"))
        .await
        .expect("default alias must route unknown model");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
}

#[tokio::test]
async fn default_alias_does_not_override_explicit_alias() {
    // When the requested model IS itself a configured alias key,
    // `default` must NOT preempt it.
    let p_fast = MockProvider::new("p_fast", vec![Behavior::Ok]);
    let p_slow = MockProvider::new("p_slow", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("m_fast".into()));
    aliases.insert("slow".into(), AliasValue::Single("m_slow".into()));
    aliases.insert("default".into(), AliasValue::Single("m_slow".into()));
    let r = build_router_v6(
        aliases,
        vec![
            ("m_fast".into(), "p_fast".into(), "m".into()),
            ("m_slow".into(), "p_slow".into(), "m".into()),
        ],
        vec![
            ("p_fast".into(), p_fast.clone() as Arc<dyn Provider>),
            ("p_slow".into(), p_slow.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("fast")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p_fast"));
    assert_eq!(p_fast.calls(), 1);
    assert_eq!(
        p_slow.calls(),
        0,
        "default alias must not override an explicit alias hit"
    );
}

#[tokio::test]
async fn default_alias_does_not_override_direct_nickname() {
    // A direct `[models]` nickname must continue to bypass alias
    // resolution; `default` never enters the picture for it.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p_default = MockProvider::new("p_default", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("m_default".into()));
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m_default".into(), "p_default".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p_default".into(), p_default.clone() as Arc<dyn Provider>),
        ],
    );

    let resp = r.complete(req("m1")).await.expect("ok");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
    assert_eq!(p_default.calls(), 0);
}

#[tokio::test]
async fn stream_empty_first_provider_falls_back() {
    // A provider whose `stream()` returns Ok but yields zero chunks
    // before EOS must NOT be reported as a successful empty stream.
    // Pre-fix the router treated this as `Ok(empty().boxed())` and
    // the breaker recorded a successful probe for an unhealthy
    // upstream. Now it must surface as a fallbackable streaming
    // error so the chain walks to the next provider AND the breaker
    // records a failure.
    let p1 = MockProvider::new("p1", vec![Behavior::StreamEmpty]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("multi", &["m1", "m2"]);
    aliases.insert(k, v);
    let r = build_router_v6(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
    );

    let stream = r
        .stream(req("multi"))
        .await
        .expect("router stream() must produce p2's stream after p1 falls back");
    let chunks: Vec<_> = stream.collect().await;
    assert!(
        chunks.iter().any(|c| c.is_ok()),
        "expected at least one Ok chunk from the fallback provider"
    );
    assert_eq!(p1.calls(), 1, "p1 must have been tried exactly once");
    assert!(p2.calls() >= 1, "p2 must have been called as fallback");
}

// ---------------------------------------------------------------------------
// v0.8 max_output_tokens (per-model override) resolution
// ---------------------------------------------------------------------------
//
// The router writes the per-model override (when set) to
// `req.routectl_internal.max_output_tokens`. Sentinel `0` means "no
// override"; the consuming Anthropic-shape egress falls through to its
// hardcoded 64000 baseline. Other egresses (openai-compat,
// openai-responses, bedrock-converse) ignore this field.

mod max_output_tokens_resolution {
    use super::*;
    use routectl_router::ModelEntry;

    /// Build a router around a `MockCaptureProvider` that snapshots the
    /// `routectl_internal.max_output_tokens` value on each dispatch
    /// attempt.
    struct CaptureProvider {
        id: String,
        observed: std::sync::Mutex<Vec<u32>>,
    }

    impl CaptureProvider {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                observed: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn observed(&self) -> Vec<u32> {
            self.observed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for CaptureProvider {
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
            self.observed
                .lock()
                .unwrap()
                .push(req.routectl_internal.max_output_tokens);
            Ok(ChatResponse::default())
        }

        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    fn config_with_model_override(model_override: Option<u32>) -> Config {
        let mut providers = BTreeMap::new();
        providers.insert(
            "p1".into(),
            ProviderEntry::openai_compat("http://example.invalid", "literal:x"),
        );
        let mut models = BTreeMap::new();
        let mut entry = ModelEntry::new("p1", "u");
        if let Some(t) = model_override {
            entry = entry.with_max_output_tokens(t);
        }
        models.insert("m".into(), entry);
        let mut aliases = BTreeMap::new();
        aliases.insert("alias".into(), AliasValue::Single("m".into()));
        Config {
            providers,
            aliases,
            models,
            ..Default::default()
        }
    }

    fn router_with_override(model_override: u32) -> (Router, Arc<CaptureProvider>) {
        let capture = CaptureProvider::new("p1");
        let cfg = config_with_model_override(if model_override > 0 {
            Some(model_override)
        } else {
            None
        });
        let mut router = Router::new(Arc::new(cfg));
        let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        let mut rm = ResolvedModel::new("m", "p1", capture.clone() as Arc<dyn Provider>, "u");
        if model_override > 0 {
            rm = rm.with_max_output_tokens(model_override);
        }
        resolved.insert("m".into(), Arc::new(rm));
        router.install_resolved_models(resolved);
        (router, capture)
    }

    /// No per-model override means routectl_internal carries the `0`
    /// sentinel; the consuming egress falls through to its hardcoded
    /// baseline.
    #[tokio::test]
    async fn no_model_override_writes_zero_sentinel() {
        let (router, capture) = router_with_override(0);
        let mut r = req("alias");
        r.max_tokens = None;
        let _ = router.complete(r).await.expect("dispatch ok");
        assert_eq!(capture.observed(), vec![0]);
    }

    /// Per-model `max_output_tokens` is projected onto routectl_internal.
    #[tokio::test]
    async fn model_override_lands_in_routectl_internal() {
        let (router, capture) = router_with_override(16_000);
        let mut r = req("alias");
        r.max_tokens = None;
        let _ = router.complete(r).await.expect("dispatch ok");
        assert_eq!(capture.observed(), vec![16_000]);
    }

    /// req.max_tokens does not alter the carrier value -- the egress's
    /// own `resolve_max_tokens` picks req.max_tokens first.
    #[tokio::test]
    async fn req_max_tokens_does_not_alter_routectl_internal() {
        let (router, capture) = router_with_override(0);
        let mut r = req("alias");
        r.max_tokens = Some(1234);
        let _ = router.complete(r).await.expect("dispatch ok");
        assert_eq!(capture.observed(), vec![0]);
    }
}
