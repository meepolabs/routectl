//! The per-attempt deadline the dispatch walk enforces around a completion:
//! an expired attempt is a status-0 upstream error carrying the exact
//! timeout text, and the walk moves on to the next target. With no tier
//! set, the shipped request deadline bounds a completion and never reaches
//! a stream that has committed content.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
};
use serde_json::{Value, json};

use super::scripted_provider_test_support::ScriptedProvider;
use super::{Router, RouterOptions};
use crate::config::{AliasValue, Config, ProviderEntry};
use crate::resolved::ResolvedModel;

const ALIAS: &str = "a";
const HANGS: &str = "hangs";
const ANSWERS: &str = "answers";
const TIMEOUT_MS: u64 = 200;
/// Long enough for the deadline to fire many times over, so a walk that
/// never enforces it is caught here rather than hanging the test.
const OUTER_BOUND: Duration = Duration::from_secs(5);

/// A provider whose completion reaches the upstream and never answers.
#[derive(Default)]
struct HangingProvider {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for HangingProvider {
    fn id(&self) -> &'static str {
        "hanging"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("hanging", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("hanging", "unused"))
    }
}

fn provider_of(nickname: &str) -> String {
    format!("p-{nickname}")
}

/// A router whose alias chains `seats` in order, one attempt per seat, with
/// `request_timeout_ms` set on the global retry policy.
fn router_over(seats: Vec<(&'static str, Arc<dyn Provider>)>) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    config.retry.request_timeout_ms = Some(TIMEOUT_MS);
    let mut models = BTreeMap::new();
    let mut chain = Vec::with_capacity(seats.len());
    for (nickname, provider) in seats {
        let provider_name = provider_of(nickname);
        config.providers.insert(
            provider_name.clone(),
            ProviderEntry::anthropic_api("literal:k"),
        );
        let model = ResolvedModel::new(nickname, provider_name, provider, format!("up-{nickname}"));
        models.insert(nickname.to_string(), Arc::new(model));
        chain.push(nickname.to_string());
    }
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Chain(chain));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// A server task can only hold the walk across an await if it is `Send`.
const fn assert_send<T: Send>(_: &T) {}

fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn an_expired_completion_attempt_walks_to_the_next_target() {
    let hangs = Arc::new(HangingProvider::default());
    let answers = ScriptedProvider::new(None, |_| Ok(()));
    let router = router_over(vec![(HANGS, hangs.clone()), (ANSWERS, answers.clone())]);

    let walk = router.complete_with_options(request(), RouterOptions::default());
    assert_send(&walk);
    let dispatched = tokio::time::timeout(OUTER_BOUND, Box::pin(walk))
        .await
        .expect("the attempt deadline ends the hanging attempt");

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result);
    assert_eq!(hangs.calls.load(Ordering::SeqCst), 1);
    assert_eq!(answers.calls(), 1);
    assert_eq!(dispatched.meta.attempt_count, 2);
    assert_eq!(dispatched.meta.fallback_count, 1);
    assert_eq!(
        dispatched.meta.served_provider.as_deref(),
        Some("p-answers")
    );
}

#[tokio::test(start_paused = true)]
async fn an_expired_completion_attempt_is_a_status_zero_error_with_the_exact_text() {
    let hangs = Arc::new(HangingProvider::default());
    let router = router_over(vec![(HANGS, hangs.clone())]);

    let dispatched = tokio::time::timeout(
        OUTER_BOUND,
        Box::pin(router.complete_with_options(request(), RouterOptions::default())),
    )
    .await
    .expect("the attempt deadline ends the hanging attempt");

    match dispatched.result {
        Err(Error::Upstream {
            provider,
            status,
            body,
            ..
        }) => {
            assert_eq!(status, 0);
            assert_eq!(body, format!("request timed out after {TIMEOUT_MS}ms"));
            assert_eq!(provider, provider_of(HANGS));
        }
        other => panic!("expected the deadline's upstream error, got {other:?}"),
    }
    assert_eq!(hangs.calls.load(Ordering::SeqCst), 1);
}

/// The provider tier's `request_timeout_ms` beats `[retry]`'s at the
/// dispatched deadline, read from the stamp installed for the target.
#[tokio::test(start_paused = true)]
async fn provider_timeout_reaches_deadline_through_stamp() {
    // Arrange
    let mut config: Config = toml::from_str(
        r#"
[providers.p-hangs]
kind = "anthropic-api"
api_key_ref = "literal:k"
request_timeout_ms = 20

[retry]
max_attempts = 1
request_timeout_ms = 5000
"#,
    )
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Chain(vec![HANGS.into()]));
    let hangs = Arc::new(HangingProvider::default());
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new(HANGS, provider_of(HANGS), hangs.clone(), "up");
    router.install_resolved_models(BTreeMap::from([(HANGS.to_string(), Arc::new(model))]));
    let started = tokio::time::Instant::now();

    // Act
    let dispatched = tokio::time::timeout(
        OUTER_BOUND,
        Box::pin(router.complete_with_options(request(), RouterOptions::default())),
    )
    .await
    .expect("the attempt deadline ends the hanging attempt");

    // Assert
    match dispatched.result {
        Err(Error::Upstream { body, .. }) => {
            assert_eq!(body, "request timed out after 20ms");
        }
        other => panic!("expected the deadline's upstream error, got {other:?}"),
    }
    assert_eq!(started.elapsed(), Duration::from_millis(20));
    assert_eq!(hangs.calls.load(Ordering::SeqCst), 1);
}

/// A pooled model's seats each dispatch under their own member's tier, with
/// the pool beneath it and `[retry]` beneath that: the per-attempt values are
/// what resolving the seat's member entry with the model's first-content
/// override gives, so a seat member's own value and the pool's value each
/// reach a dispatched attempt.
#[tokio::test(start_paused = true)]
async fn each_pooled_seat_dispatches_under_its_member_tier() {
    // Arrange
    const MEMBER_A: &str = "member-a";
    const MEMBER_B: &str = "member-b";
    let mut config: Config = toml::from_str(&format!(
        r#"
[providers.{MEMBER_A}]
kind = "anthropic-api"
api_key_ref = "literal:k"
request_timeout_ms = 20

[providers.{MEMBER_B}]
kind = "anthropic-api"
api_key_ref = "literal:k"

[pools.pool]
members = ["{MEMBER_A}", "{MEMBER_B}"]
request_timeout_ms = 40

[retry]
max_attempts = 1
request_timeout_ms = 80
"#
    ))
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Chain(vec!["opus".into()]));
    let seat_a = Arc::new(HangingProvider::default());
    let seat_b = Arc::new(HangingProvider::default());
    let seats = vec![
        crate::seat_pool::SeatTarget::new(MEMBER_A.into(), seat_a.clone(), None, false),
        crate::seat_pool::SeatTarget::new(MEMBER_B.into(), seat_b.clone(), None, false),
    ];
    let model = ResolvedModel::new("opus", "pool", seat_a.clone(), "up")
        .with_stream_first_byte_timeout_ms(7_000)
        .with_seats(Arc::from(seats));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([("opus".to_string(), Arc::new(model))]));
    let targets = router.dispatch_chain(ALIAS, None).expect("chain resolves");
    let started = tokio::time::Instant::now();

    // Act
    let dispatched = tokio::time::timeout(
        OUTER_BOUND,
        Box::pin(router.complete_with_options(request(), RouterOptions::default())),
    )
    .await
    .expect("the attempt deadlines end both hanging seats");

    // Assert
    let stamps: Vec<_> = targets.iter().map(|t| *router.tuning_for(t)).collect();
    let per_member: Vec<_> = [MEMBER_A, MEMBER_B]
        .iter()
        .map(|member| crate::config::resolve_tuning(&router.config, Some(member), Some(7_000)))
        .collect();
    assert_eq!(stamps, per_member);
    match dispatched.result {
        Err(Error::Upstream { provider, body, .. }) => {
            assert_eq!(provider, MEMBER_B);
            assert_eq!(body, "request timed out after 40ms");
        }
        other => panic!("expected the last seat's deadline error, got {other:?}"),
    }
    assert_eq!(started.elapsed(), Duration::from_millis(20 + 40));
    assert_eq!(seat_a.calls.load(Ordering::SeqCst), 1);
    assert_eq!(seat_b.calls.load(Ordering::SeqCst), 1);
}

/// The shipped per-attempt request deadline, in milliseconds.
const SHIPPED_REQUEST_TIMEOUT_MS: u64 = 1_800_000;
/// Past the shipped request deadline, so a walk that never enforces it
/// fails here rather than hanging the test.
const PAST_SHIPPED_DEADLINE: Duration = Duration::from_hours(1);

/// A provider whose stream delivers one content chunk and then idles
/// forever without ending.
struct ContentThenIdle;

#[async_trait::async_trait]
impl Provider for ContentThenIdle {
    fn id(&self) -> &'static str {
        "content-then-idle"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("content-then-idle", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("content-then-idle", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let content = ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("hi".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(futures::stream::iter([Ok(content)])
            .chain(futures::stream::pending())
            .boxed())
    }
}

/// A router whose alias names one seat, with one attempt and no tier
/// setting either timeout, so the shipped defaults are in force.
fn untuned_router_over(nickname: &'static str, provider: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    let provider_name = provider_of(nickname);
    config.providers.insert(
        provider_name.clone(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Chain(vec![nickname.into()]));
    let model = ResolvedModel::new(nickname, provider_name, provider, format!("up-{nickname}"));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(nickname.to_string(), Arc::new(model))]));
    router
}

#[tokio::test(start_paused = true)]
async fn an_untuned_completion_times_out_at_the_shipped_request_deadline() {
    // Arrange
    let hangs = Arc::new(HangingProvider::default());
    let router = untuned_router_over(HANGS, hangs.clone());
    let started = tokio::time::Instant::now();

    // Act
    let dispatched = tokio::time::timeout(
        PAST_SHIPPED_DEADLINE,
        Box::pin(router.complete_with_options(request(), RouterOptions::default())),
    )
    .await
    .expect("the shipped deadline ends the hanging attempt");

    // Assert
    match dispatched.result {
        Err(Error::Upstream { status, body, .. }) => {
            assert_eq!(status, 0);
            assert_eq!(
                body,
                format!("request timed out after {SHIPPED_REQUEST_TIMEOUT_MS}ms")
            );
        }
        other => panic!("expected the deadline's upstream error, got {other:?}"),
    }
    assert_eq!(
        started.elapsed(),
        Duration::from_millis(SHIPPED_REQUEST_TIMEOUT_MS)
    );
    assert_eq!(hangs.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn committed_stream_is_not_cut_at_request_timeout() {
    // Arrange
    const IDLES: &str = "idles";
    let router = untuned_router_over(IDLES, Arc::new(ContentThenIdle));
    let targets = router.dispatch_chain(ALIAS, None).expect("chain resolves");
    assert_eq!(
        router.tuning_for(&targets[0]).request_timeout_ms.value,
        Some(SHIPPED_REQUEST_TIMEOUT_MS),
        "premise: the shipped request deadline is in force on this seat"
    );
    let mut stream = router
        .stream(request())
        .await
        .expect("the stream commits at its first content");
    let first = stream.next().await;
    assert!(matches!(first, Some(Ok(_))), "premise: content delivered");
    let committed_at = tokio::time::Instant::now();
    let idle = Duration::from_millis(SHIPPED_REQUEST_TIMEOUT_MS) + Duration::from_mins(1);

    // Act
    let next = tokio::time::timeout(idle, stream.next()).await;

    // Assert
    assert!(
        next.is_err(),
        "the router ended or errored the committed stream: {next:?}"
    );
    assert_eq!(committed_at.elapsed(), idle);
}
