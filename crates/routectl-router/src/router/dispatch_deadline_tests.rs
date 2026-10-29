//! The per-attempt deadline the dispatch walk enforces around a completion:
//! an expired attempt is a status-0 upstream error carrying the exact
//! timeout text, and the walk moves on to the next target.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
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
