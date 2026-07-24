//! Router-level tests for the 401 -> `provider.on_auth_failure()`
//! -> retry-once dispatch path. The OAuth store has its own
//! lower-level tests for `refresh_under_lock` semantics; these
//! tests pin the router-side wiring: that a 401 from a provider
//! actually triggers `on_auth_failure`, that the retry happens
//! exactly once, and that a refresh failure propagates without
//! walking the fallback chain.
use super::*;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mock provider that returns `Error::Upstream { status: 401, .. }`
/// on its first `complete()` call and a 200-shaped success on
/// every subsequent call. `on_auth_failure_calls` increments on
/// each `on_auth_failure()` invocation so the test can assert the
/// router actually dispatched through the trait method.
struct Recovering401Provider {
    id: String,
    complete_calls: AtomicUsize,
    on_auth_failure_calls: AtomicUsize,
    /// If set, `on_auth_failure` returns this error string wrapped
    /// in `Error::Auth` (simulating a refresh-token-revoked path).
    refresh_failure: Option<String>,
}

#[async_trait]
impl Provider for Recovering401Provider {
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
        let n = self.complete_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(Error::upstream(&self.id, 401, "stale token"))
        } else {
            Ok(ChatResponse {
                id: format!("ok-{}", self.id),
                model: req.model,
                created: 0,
                choices: vec![Choice {
                    logprobs: None,
                    index: 0,
                    message: Message {
                        refusal: None,
                        role: routectl_core::Role::Assistant,
                        content: routectl_core::MessageContent::Text("ok".into()),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                    },
                    finish_reason: Some("stop".into()),
                    matched_stop_sequence: None,
                }],
                usage: Some(routectl_core::Usage::default()),
                routectl_provider: None,
                extras: Default::default(),
                upstream_meta: None,
            })
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("not exercised by these tests")
    }
    async fn on_auth_failure(&self) -> Result<()> {
        self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(msg) = &self.refresh_failure {
            Err(Error::Auth(msg.clone()))
        } else {
            Ok(())
        }
    }
}

fn build_router_with_provider(provider: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p-recover".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy::default(),
        },
    );
    config
        .aliases
        .insert("alias".into(), AliasValue::Single("recover-model".into()));
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "recover-model".into(),
        Arc::new(ResolvedModel::new(
            "recover-model",
            "p-recover",
            provider,
            "u-recover",
        )),
    );
    router.install_resolved_models(models);
    router
}

fn req_for(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn router_401_triggers_on_auth_failure_and_retries_once() {
    let provider = Arc::new(Recovering401Provider {
        id: "p-recover".into(),
        complete_calls: AtomicUsize::new(0),
        on_auth_failure_calls: AtomicUsize::new(0),
        refresh_failure: None,
    });
    let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

    let resp = router
        .complete(req_for("alias"))
        .await
        .expect("401 -> on_auth_failure -> retry should land on the success branch");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p-recover"));
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        2,
        "complete should be called twice: the 401 attempt and the retry",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure should fire exactly once between the 401 and the retry",
    );
}

#[tokio::test]
async fn router_refresh_failure_propagates_without_fallback() {
    // When provider.on_auth_failure() itself errors (e.g.,
    // invalid_grant from the IdP), the router must surface that
    // error directly rather than walking the fallback chain. The
    // OAuth identity is dead; falling back over a known-broken
    // credential masks the failure.
    let provider = Arc::new(Recovering401Provider {
        id: "p-recover".into(),
        complete_calls: AtomicUsize::new(0),
        on_auth_failure_calls: AtomicUsize::new(0),
        refresh_failure: Some(
            "oauth refresh failed for anthropic: invalid_grant; \
                 re-run `routectl login anthropic`"
                .into(),
        ),
    });
    let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

    let err = router
        .complete(req_for("alias"))
        .await
        .expect_err("refresh failure must surface as an error, not a fallback success");
    match err {
        Error::Auth(msg) => {
            assert!(
                msg.contains("oauth refresh failed"),
                "auth error must carry the refresh-failure message: {msg}",
            );
            assert!(
                msg.contains("re-run"),
                "auth error must carry the actionable hint: {msg}",
            );
        }
        other => panic!("expected Error::Auth, got: {other:?}"),
    }
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        1,
        "no retry should fire when on_auth_failure errors",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires exactly once before the auth error propagates",
    );
}

#[tokio::test]
async fn router_second_consecutive_401_does_not_retry_again() {
    // After a successful refresh, if the SAME chain entry returns
    // 401 again (e.g., the upstream is broken in a way the
    // refresh can't fix), the auth_retry_attempted flag prevents
    // an infinite loop. The second 401 falls through to
    // should_fallback like any other 4xx error.
    struct AlwaysReturns401 {
        id: String,
        complete_calls: AtomicUsize,
        on_auth_failure_calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for AlwaysReturns401 {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            self.complete_calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::upstream(&self.id, 401, "still 401"))
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
        async fn on_auth_failure(&self) -> Result<()> {
            self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let provider = Arc::new(AlwaysReturns401 {
        id: "p-recover".into(),
        complete_calls: AtomicUsize::new(0),
        on_auth_failure_calls: AtomicUsize::new(0),
    });
    let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);

    let _ = router
        .complete(req_for("alias"))
        .await
        .expect_err("perpetual 401 must surface as an error after the one retry");
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        2,
        "exactly two completes: the original 401 and the post-refresh 401 retry",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires once; the auth_retry_attempted flag blocks the second call",
    );
}
