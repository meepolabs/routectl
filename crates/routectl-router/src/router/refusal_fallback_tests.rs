//! A provider that refuses a request for its shape hands the request to the
//! next target in the chain, on the complete and the stream path, and is
//! attempted once even when the operator raises the bad-request retry cap.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::stream::{BoxStream, StreamExt};
use routectl_core::failure_class::FailureClass;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, RefusalReason,
    Result,
};
use serde_json::{Value, json};

use super::should_retry_same_provider;
use crate::class_policy::{ClassPolicy, ConfigFailureClass};
use crate::config::{AliasValue, Config, ProviderEntry, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::router::{Router, RouterOptions};

const ALIAS: &str = "a";
const REFUSING: &str = "p-refusing";
const SERVING: &str = "p-serving";
const OVERLAY_RETRIES: u32 = 3;

/// A provider that refuses every call because the target has no slot for the
/// request's shape, counting how often it is asked.
struct RefusingProvider {
    calls: AtomicU32,
}

impl RefusingProvider {
    fn refusal() -> Error {
        Error::normalize_request(
            REFUSING,
            "the target has no slot for this content",
            RefusalReason::Unrepresentable,
        )
    }
}

#[async_trait::async_trait]
impl Provider for RefusingProvider {
    fn id(&self) -> &'static str {
        REFUSING
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Err(Self::refusal())
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(REFUSING, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Self::refusal())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Self::refusal())
    }
}

/// A provider that answers every call successfully.
struct ServingProvider;

#[async_trait::async_trait]
impl Provider for ServingProvider {
    fn id(&self) -> &'static str {
        SERVING
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(SERVING, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse::default())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let chunk = ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(futures::stream::iter([Ok(chunk)]).boxed())
    }
}

fn bad_request_overlay() -> BTreeMap<ConfigFailureClass, ClassPolicy> {
    BTreeMap::from([(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: Some(OVERLAY_RETRIES),
            fallback: None,
        },
    )])
}

/// A two-target chain: the refusing seat first, the serving seat second,
/// with the bad-request retry cap raised by the operator overlay.
fn chain_router(refusing: Arc<RefusingProvider>) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    config.retry.initial_backoff_ms = 1;
    config.retry.backoff_multiplier = 1.0;
    config.retry.classes = bad_request_overlay();
    for name in [REFUSING, SERVING] {
        config
            .providers
            .insert(name.into(), ProviderEntry::anthropic_api("literal:k"));
    }
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(vec!["m-refusing".into(), "m-serving".into()]),
    );
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::from([
        (
            "m-refusing".to_string(),
            Arc::new(ResolvedModel::new("m-refusing", REFUSING, refusing, "u-1")),
        ),
        (
            "m-serving".to_string(),
            Arc::new(ResolvedModel::new(
                "m-serving",
                SERVING,
                Arc::new(ServingProvider),
                "u-2",
            )),
        ),
    ]);
    router.install_resolved_models(models);
    router
}

fn refusing_seat() -> Arc<RefusingProvider> {
    Arc::new(RefusingProvider {
        calls: AtomicU32::new(0),
    })
}

fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_provider_refusal_on_complete_walks_to_the_next_target_without_a_retry() {
    let refusing = refusing_seat();
    let router = chain_router(Arc::clone(&refusing));

    let dispatched = Box::pin(router.complete_with_options(request(), RouterOptions::new())).await;

    dispatched.result.expect("the second target serves");
    assert_eq!(dispatched.meta.served_provider.as_deref(), Some(SERVING));
    assert_eq!(dispatched.meta.fallback_count, 1, "one fallback hop");
    assert_eq!(dispatched.meta.attempt_count, 2, "no same-seat retry");
    assert_eq!(
        refusing.calls.load(Ordering::SeqCst),
        1,
        "refused seat asked once"
    );
}

#[tokio::test]
async fn a_provider_refusal_on_stream_walks_to_the_next_target_without_a_retry() {
    let refusing = refusing_seat();
    let router = chain_router(Arc::clone(&refusing));

    let dispatched = Box::pin(router.stream_with_options(request(), RouterOptions::new())).await;

    assert!(dispatched.result.is_ok(), "the second target serves");
    assert_eq!(dispatched.meta.served_provider.as_deref(), Some(SERVING));
    assert_eq!(dispatched.meta.fallback_count, 1, "one fallback hop");
    assert_eq!(dispatched.meta.attempt_count, 2, "no same-seat retry");
    assert_eq!(
        refusing.calls.load(Ordering::SeqCst),
        1,
        "refused seat asked once"
    );
}

#[test]
fn a_refusal_ignores_a_raised_bad_request_retry_cap_and_an_upstream_400_does_not() {
    let policy = RetryPolicy {
        classes: bad_request_overlay(),
        ..RetryPolicy::default()
    };
    let rows: [(&str, Error, bool); 4] = [
        ("normalize refusal", RefusingProvider::refusal(), false),
        (
            "validation refusal",
            Error::Validation("v".into(), RefusalReason::CacheBreakpointCap),
            false,
        ),
        (
            "local refusal",
            Error::LocalRefusal {
                provider: REFUSING.into(),
                detail: "d".into(),
                reason: RefusalReason::CloakRelocation,
            },
            false,
        ),
        (
            "upstream 400 control",
            Error::upstream(REFUSING, 400, "rejected"),
            true,
        ),
    ];

    for (case, err, want) in &rows {
        let retried = should_retry_same_provider(err, &FailureClass::BadRequest, &policy, 0, false);

        assert_eq!(retried, *want, "{case}");
    }
}
