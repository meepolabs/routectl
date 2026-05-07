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
    AliasEntry, Config, RetryPolicy, Router,
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
