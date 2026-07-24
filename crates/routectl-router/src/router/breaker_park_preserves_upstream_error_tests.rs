//! Completion-path guard: when a debiting 429 carries an upstream reset
//! large enough to force-open (park) the provider's breaker, the SAME
//! request must surface the real upstream 429 + Retry-After, NOT the
//! synthetic status-0 "circuit breaker open" gate error. The synthetic
//! error stays reserved for a request blocked BEFORE dispatch (the next
//! request that arrives during the active park).
use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::Result;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
use routectl_testkit::with_capture;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// A reset far above INLOOP_RETRY_AFTER_CAP, so a debiting 429 parks the
// provider rather than bumping the in-loop sleep. Equal to the default
// max_honored_retry_after ceiling, so it is honored unclamped.
const RETRY_AFTER: Duration = Duration::from_hours(1);

/// Sole chain-entry provider: every dispatch fails with a real,
/// debiting 429 carrying a large upstream reset hint. Counts how many
/// times its body is actually reached.
struct ParkingProvider {
    id: String,
    calls: Arc<AtomicUsize>,
}

impl ParkingProvider {
    fn rate_limited(&self) -> Error {
        Error::upstream_with_retry_after(
            &self.id,
            429,
            "rate limited by upstream",
            Some(RETRY_AFTER),
        )
    }
}

#[async_trait]
impl Provider for ParkingProvider {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.rate_limited())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.rate_limited())
    }
}

/// Router with a sole alias entry dispatching to a provider that always
/// 429s with a large reset. The default retry policy caps RateLimited at
/// `max_attempts` (2), so a same-provider retry is admitted at attempt 1
/// -- exactly the branch that used to discard the genuine error. The
/// large reset parks the provider, so that retry re-gates to CircuitOpen.
fn router_with_parking_entry() -> (Router, Arc<AtomicUsize>) {
    let mut config = Config::default();
    config
        .aliases
        .insert("solo".into(), AliasValue::Chain(vec!["seat".into()]));
    config.providers.insert(
        "p".into(),
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
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(1),
                circuit_cooldown_ms: Some(60_000),
                ..Default::default()
            },
        },
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(ParkingProvider {
        id: "p".into(),
        calls: Arc::clone(&calls),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "seat".into(),
        Arc::new(ResolvedModel::new("seat", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    (router, calls)
}

fn solo_req() -> ChatRequest {
    ChatRequest {
        model: "solo".into(),
        messages: vec![],
        ..Default::default()
    }
}

fn upstream_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream { status, .. } => Some(*status),
        _ => None,
    }
}

fn upstream_retry_after(err: &Error) -> Option<Duration> {
    match err {
        Error::Upstream { retry_after, .. } => *retry_after,
        _ => None,
    }
}

#[tokio::test]
async fn parking_request_surfaces_real_429_not_synthetic_gate_error() {
    let (router, _calls) = router_with_parking_entry();
    let err = router
        .complete(solo_req())
        .await
        .expect_err("a parked sole entry still fails the request");
    assert_eq!(
        upstream_status(&err),
        Some(429),
        "client must receive the genuine upstream 429, not the synthetic status-0"
    );
    assert_eq!(
        upstream_retry_after(&err),
        Some(RETRY_AFTER),
        "the upstream Retry-After must be preserved on the parking request"
    );
    assert!(
        !err.to_string().contains("circuit breaker open"),
        "the synthetic gate error must not surface on the parking request, got: {err}"
    );
}

#[tokio::test]
async fn parking_request_does_not_retry_the_self_parked_provider() {
    let (router, calls) = router_with_parking_entry();
    let (_result, events) = with_capture(router.complete(solo_req())).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the provider must be dialed exactly once on the parking attempt"
    );
    assert!(
        !events.iter().any(|e| e.message == "retrying same provider"),
        "no same-provider retry may be attempted once this attempt parked the breaker"
    );
}

#[tokio::test]
async fn next_request_during_park_still_sees_synthetic_circuit_open() {
    let (router, calls) = router_with_parking_entry();
    // First request parks the provider and surfaces the real 429.
    let first = router
        .complete(solo_req())
        .await
        .expect_err("first request fails with the real 429");
    assert_eq!(upstream_status(&first), Some(429));
    let dials_after_first = calls.load(Ordering::SeqCst);

    // Second request during the active park is blocked BEFORE dispatch:
    // it must see the synthetic status-0 gate error, and the provider
    // body must not be reached again.
    let second = router
        .complete(solo_req())
        .await
        .expect_err("second request fails while the breaker is parked");
    assert_eq!(
        upstream_status(&second),
        Some(0),
        "a request blocked before dispatch keeps the synthetic status-0"
    );
    assert!(
        second.to_string().contains("circuit breaker open"),
        "the pre-dispatch block must surface the synthetic gate error, got: {second}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        dials_after_first,
        "the parked provider must not be dialed by the next request"
    );
}

#[tokio::test]
async fn retry_decision_event_carries_full_field_set() {
    let (router, _calls) = router_with_parking_entry();
    let (_result, events) = with_capture(router.complete(solo_req())).await;
    let ev = events
        .iter()
        .find(|e| e.message == "retry decision")
        .expect("a retry-decision event must be emitted on the completion error path");
    assert_eq!(ev.level, tracing::Level::DEBUG);
    assert_eq!(ev.field("provider"), Some("p"));
    assert_eq!(ev.field("state_key"), Some("seat"));
    assert_eq!(ev.field("surface"), Some("complete"));
    assert_eq!(ev.field("attempt"), Some("1"));
    assert_eq!(ev.field("status"), Some("Some(429)"));
    assert_eq!(ev.field("upstream_type"), Some("None"));
    assert_eq!(ev.field("retry_after_ms"), Some("Some(3600000)"));
    assert_eq!(ev.field("breaker_effect"), Some("parked"));
    assert_eq!(ev.field("same_provider_retry"), Some("false"));
    assert_eq!(ev.field("preserved_upstream_error"), Some("true"));
}
