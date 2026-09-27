//! RPM buckets are per runtime-state key, not per provider: exhausting one
//! direct model or one pooled seat leaves every sibling key on the same
//! provider dispatchable. Driven through `complete` so the gate lookup the
//! dispatch loop performs is the one under test.

use super::*;
use crate::config::{PoolEntry, ProviderEntry, ProviderRuntimePolicy};
use crate::seat_pool::SeatTarget;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("counting", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            model: req.model,
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("never streamed")
    }
}

/// One request per minute on every provider, so a single dispatch drains a
/// key's bucket.
fn rpm1_provider() -> ProviderEntry {
    ProviderEntry::anthropic_api("env://K").with_runtime(ProviderRuntimePolicy {
        rpm_limit: Some(1),
        ..Default::default()
    })
}

fn req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

fn is_rpm_refusal(err: &Error) -> bool {
    err.to_string().contains("rpm_limit")
}

/// Two direct models `a` and `b` on the one rpm-1 provider `p`.
fn direct_siblings(calls: &Arc<AtomicUsize>) -> Router {
    let mut config = Config::default();
    config.providers.insert("p".into(), rpm1_provider());
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(CountingProvider {
        calls: calls.clone(),
    });
    let models = ["a", "b"]
        .into_iter()
        .map(|nickname| {
            let model = ResolvedModel::new(nickname, "p", provider.clone(), "u");
            (nickname.to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

/// Two pooled models `opus` and `sonnet` on pool `pool`, whose only member is
/// the rpm-1 provider `m1`: seat keys `opus#m1` and `sonnet#m1`.
fn pooled_siblings(calls: &Arc<AtomicUsize>) -> Router {
    let mut config = Config::default();
    config.providers.insert("m1".into(), rpm1_provider());
    config
        .pools
        .insert("pool".into(), PoolEntry::new(vec!["m1".into()]));
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(CountingProvider {
        calls: calls.clone(),
    });
    let models = ["opus", "sonnet"]
        .into_iter()
        .map(|nickname| {
            let seat = SeatTarget {
                provider_name: "m1".into(),
                provider: provider.clone(),
                auth_secret_ref: None,
            };
            let model = ResolvedModel::new(nickname, "pool", provider.clone(), "u")
                .with_seats(Arc::from(vec![seat]));
            (nickname.to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn an_exhausted_direct_model_key_leaves_its_sibling_on_the_same_provider_dispatchable() {
    // Arrange: drain `a`'s bucket and prove it is drained.
    let calls = Arc::new(AtomicUsize::new(0));
    let router = direct_siblings(&calls);
    router.complete(req("a")).await.expect("first `a` serves");
    let refused = router
        .complete(req("a"))
        .await
        .expect_err("`a` has no token left");
    assert!(is_rpm_refusal(&refused), "{refused}");

    // Act
    let sibling = router.complete(req("b")).await;

    // Assert
    assert!(sibling.is_ok(), "`b` owns its own bucket: {sibling:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "a once, b once");
}

#[tokio::test]
async fn an_exhausted_pooled_seat_key_leaves_the_same_members_seat_of_another_model_dispatchable() {
    // Arrange: drain `opus#m1` and prove it is drained.
    let calls = Arc::new(AtomicUsize::new(0));
    let router = pooled_siblings(&calls);
    router
        .complete(req("opus"))
        .await
        .expect("first `opus` serves");
    let refused = router
        .complete(req("opus"))
        .await
        .expect_err("`opus#m1` has no token left");
    assert!(is_rpm_refusal(&refused), "{refused}");

    // Act
    let sibling = router.complete(req("sonnet")).await;

    // Assert
    assert!(
        sibling.is_ok(),
        "`sonnet#m1` owns its own bucket: {sibling:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "opus once, sonnet once");
}
