//! Router behavior tests with mock Provider impls.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, OpaqueSseEvent,
    Provider, Result, Role, Usage,
    schema::{ChunkChoice, ChunkDelta},
};
use routectl_router::{
    AliasValue, Config, Dispatched, DispatchedStream, ProviderEntry, ProviderRuntimePolicy,
    ResolvedModel, RetryPolicy, Router, RouterOptions,
};

mod common;

// Scenario-grouped test modules. Each sibling file under `router/` is a
// child module of this test binary, reaching the shared helpers below
// via `use super::*`. They are NOT separate test binaries -- cargo does
// not auto-compile files in `tests/` subdirectories.
#[path = "router/default_alias.rs"]
mod default_alias;
#[path = "router/dispatch_fallback.rs"]
mod dispatch_fallback;
#[path = "router/dispatch_meta.rs"]
mod dispatch_meta;
#[path = "router/gate_per_attempt.rs"]
mod gate_per_attempt;
#[path = "router/reported_model.rs"]
mod reported_model;
#[path = "router/retry.rs"]
mod retry;
#[path = "router/runtime_policy.rs"]
mod runtime_policy;

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
    /// First (and only) stream item is the error the Anthropic SSE
    /// classifier emits for an `overloaded_error` event: an
    /// `Error::Upstream` carrying status 529 and
    /// `upstream_type = Some("overloaded_error")`. Pins the permissive
    /// side of the in-stream error boundary -- a first-event upstream
    /// error must fall the chain over to the next target.
    StreamFirstChunkOverloaded,
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
            Behavior::StreamFirstChunkOverloaded => {
                let err = Error::upstream_full(
                    &id,
                    529,
                    "overloaded_error: upstream signaled error event mid-stream",
                    None,
                    Some("overloaded_error".into()),
                    None,
                );
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
                    upstream_meta: None,
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
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
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
        upstream_meta: None,
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
        upstream_meta: None,
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
            ProviderEntry::openai_compat("http://example.invalid", common::file_ref("x"))
                .with_runtime(runtime),
        );
    }

    let cfg = Config {
        server: Default::default(),
        providers: config_providers,
        aliases,
        retry,
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
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
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

/// Build a router whose config providers exist (so provider_kind
/// resolves) for a two-entry chain. Providers are openai-compat in
/// config, so the resolved kind token is "openai-compat".
fn router_with_config_providers(
    chain: &[&str],
    models: Vec<(String, String, String)>,
    providers: Vec<(String, Arc<dyn Provider>)>,
    retry: RetryPolicy,
) -> Router {
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", chain);
    aliases.insert(k, v);
    let runtime: BTreeMap<String, ProviderRuntimePolicy> = providers
        .iter()
        .map(|(name, _)| (name.clone(), ProviderRuntimePolicy::default()))
        .collect();
    build_router_v6_full(aliases, models, providers, retry, runtime)
}

/// Build a single-model router whose resolved model carries an optional
/// `reported_model` override. The alias `fast` routes to nickname `m1`.
fn router_with_reported_model(
    upstream: &str,
    reported: Option<&str>,
    provider: Arc<dyn Provider>,
) -> Router {
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("m1".into()));
    let cfg = Config {
        aliases,
        retry: default_test_retry(),
        ..Default::default()
    };
    let mut router = Router::new(Arc::new(cfg));
    let mut model = ResolvedModel::new("m1", "p1", provider, upstream);
    if let Some(label) = reported {
        model = model.with_reported_model(label);
    }
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert("m1".into(), Arc::new(model));
    router.install_resolved_models(resolved);
    router
}

// ---------------------------------------------------------------------------
// v0.8 max_output_tokens (per-model override) resolution
// ---------------------------------------------------------------------------

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
            ProviderEntry::openai_compat("http://example.invalid", common::file_ref("x")),
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

// The M4 first-activity mark (see `try_stream_with_first_chunk` in
// src/router.rs) is observed via a documented manual capture recipe
// (docs/LOGGING.md, "First-activity mark (M4)") rather than an
// automated test: capturing a `tracing` debug event through a
// thread-local subscriber across the async runtime was flaky under
// the parallel test harness (0 events captured intermittently under
// load), while passing reliably in isolation. The production log
// site is unchanged and covered structurally by the existing stream
// tests in the scenario modules above.
