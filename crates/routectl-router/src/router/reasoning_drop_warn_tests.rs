//! The reasoning-drop fidelity WARN is emitted router-side, at the
//! dispatch point inside the chain loop, so it observes the per-target
//! (post-overlay) request that is actually dispatched. Two properties are
//! load-bearing and pinned here: it fires AT MOST ONCE per client request
//! (across same-provider retries and fallback hops), and it is
//! target-accurate (a Responses primary that fails over to a
//! non-Responses fallback warns; a Responses-only walk does not).

use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use async_trait::async_trait;
use routectl_testkit::with_capture;

/// Marker substring of the fidelity WARN. Matches the emit in
/// `dispatch.rs`.
const WARN_NEEDLE: &str = "reasoning context/mode dropped";

/// Every provider in these chains fails fallbackably, so the walk always
/// reaches the last entry and every attempt runs the dispatch-point emit.
struct AlwaysFailing {
    id: String,
}

impl AlwaysFailing {
    fn arc(id: &str) -> Arc<dyn Provider> {
        Arc::new(Self { id: id.into() })
    }
}

#[async_trait]
impl Provider for AlwaysFailing {
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
        Err(Error::upstream(&self.id, 503, "unavailable"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(&self.id, 503, "unavailable"))
    }
}

/// Build a router over the given `(model, provider_name, entry)` chain,
/// with `max_attempts` attempts per entry so a same-provider retry is
/// exercised alongside the fallback hops.
fn router_over(entries: Vec<(&str, &str, ProviderEntry)>, max_attempts: u32) -> Router {
    let mut providers = BTreeMap::new();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut chain: Vec<String> = Vec::new();
    for (model, provider_name, entry) in entries {
        providers.insert(provider_name.to_string(), entry);
        models.insert(
            model.to_string(),
            Arc::new(ResolvedModel::new(
                model,
                provider_name,
                AlwaysFailing::arc(provider_name),
                "wire-model",
            )),
        );
        chain.push(model.to_string());
    }
    let config = Config {
        retry: RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        providers,
        aliases: {
            let mut a = BTreeMap::new();
            a.insert("chain".to_string(), AliasValue::Chain(chain));
            a
        },
        ..Config::default()
    };
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn req_carrying_reasoning_dialect() -> ChatRequest {
    ChatRequest {
        model: "chain".into(),
        messages: vec![].into(),
        provider_extras: Some(serde_json::json!({
            "reasoning": {"context": "all_turns", "mode": "pro"}
        })),
        ..Default::default()
    }
}

fn drop_warns(events: &[routectl_testkit::CapturedEvent]) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.contains(WARN_NEEDLE))
        .collect()
}

fn compat_entry() -> ProviderEntry {
    ProviderEntry::openai_compat("https://example.test/v1", "literal:k")
}

#[cfg(feature = "openai-responses")]
fn responses_entry() -> ProviderEntry {
    ProviderEntry::openai_responses("literal:k")
}

// -- the predicate ------------------------------------------------------

#[test]
fn predicate_fires_on_context_or_mode_and_not_on_summary_alone() {
    let mut req = ChatRequest::default();
    assert!(
        !carries_responses_reasoning_dialect(&req),
        "no provider_extras at all carries nothing"
    );

    req.provider_extras = Some(serde_json::json!({"reasoning": {"summary": "concise"}}));
    assert!(
        !carries_responses_reasoning_dialect(&req),
        "a summary-only remainder is a soft downgrade, not a semantic gap"
    );

    req.provider_extras = Some(serde_json::json!({"reasoning": {"context": "all_turns"}}));
    assert!(carries_responses_reasoning_dialect(&req), "context counts");

    req.provider_extras = Some(serde_json::json!({"reasoning": {"mode": "pro"}}));
    assert!(carries_responses_reasoning_dialect(&req), "mode counts");

    req.provider_extras = Some(serde_json::json!({"reasoning": "pro"}));
    assert!(
        !carries_responses_reasoning_dialect(&req),
        "a non-object reasoning value carries no sub-keys"
    );
}

#[test]
fn only_the_responses_kind_preserves_the_dialect() {
    assert!(
        !target_drops_responses_reasoning(Some(RESPONSES_PROVIDER_KIND)),
        "the Responses egress re-emits context/mode"
    );
    assert!(target_drops_responses_reasoning(Some("openai-compat")));
    assert!(target_drops_responses_reasoning(Some("anthropic-api")));
    assert!(
        target_drops_responses_reasoning(None),
        "an unresolved provider kind (legacy direct dispatch) warns -- fail loud"
    );
}

// -- once per client request --------------------------------------------

#[tokio::test]
async fn same_provider_retries_and_fallback_hops_warn_exactly_once() {
    // Arrange: two non-Responses entries, two attempts each -- four
    // dispatch-point passes over a request carrying the dialect.
    let router = router_over(
        vec![("m1", "p1", compat_entry()), ("m2", "p2", compat_entry())],
        2,
    );

    // Act
    let (Dispatched { meta, result }, events) = with_capture(
        router.complete_with_options(req_carrying_reasoning_dialect(), RouterOptions::new()),
    )
    .await;

    // Assert: the whole chain was walked with a retry on each entry, and
    // the WARN fired exactly once across all four attempts.
    result.expect_err("every entry fails");
    assert_eq!(meta.attempt_count, 4, "two attempts on each of two entries");
    assert_eq!(meta.fallback_count, 1, "one fallback hop");
    let warns = drop_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "at most one reasoning-drop WARN per client request; got: {warns:?}"
    );
    assert_eq!(warns[0].field("provider"), Some("p1"));
}

#[tokio::test]
async fn streaming_dispatch_warns_exactly_once_too() {
    let router = router_over(
        vec![("m1", "p1", compat_entry()), ("m2", "p2", compat_entry())],
        2,
    );

    let (dispatched, events) = with_capture(Box::pin(
        router.stream_with_options(req_carrying_reasoning_dialect(), RouterOptions::new()),
    ))
    .await;

    dispatched.result.err().expect("every entry fails");
    let warns = drop_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "the streaming loop is not a second site that repeats the WARN; got: {warns:?}"
    );
}

#[tokio::test]
async fn a_request_without_the_dialect_never_warns() {
    let router = router_over(vec![("m1", "p1", compat_entry())], 1);
    let req = ChatRequest {
        model: "chain".into(),
        messages: vec![].into(),
        ..Default::default()
    };

    let (dispatched, events) =
        with_capture(router.complete_with_options(req, RouterOptions::new())).await;

    dispatched.result.expect_err("the entry fails");
    assert!(drop_warns(&events).is_empty());
}

// -- target accuracy ----------------------------------------------------

/// The case a pre-loop emit off the ORIGINAL request gets wrong in both
/// directions: the primary preserves the dialect, the fallback drops it.
#[cfg(feature = "openai-responses")]
#[tokio::test]
async fn responses_primary_falling_back_to_a_non_responses_target_warns_once() {
    let router = router_over(
        vec![
            ("m1", "p-responses", responses_entry()),
            ("m2", "p-compat", compat_entry()),
        ],
        2,
    );

    let (Dispatched { meta, result }, events) = with_capture(
        router.complete_with_options(req_carrying_reasoning_dialect(), RouterOptions::new()),
    )
    .await;

    result.expect_err("every entry fails");
    assert_eq!(meta.fallback_count, 1, "the chain reached the fallback");
    let warns = drop_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "the non-Responses fallback drops the dialect and must warn -- once; got: {warns:?}"
    );
    assert_eq!(
        warns[0].field("provider"),
        Some("p-compat"),
        "the WARN names the target that actually dropped it, not the primary"
    );
}

#[cfg(feature = "openai-responses")]
#[tokio::test]
async fn a_responses_only_chain_never_warns() {
    let router = router_over(
        vec![
            ("m1", "p-responses-a", responses_entry()),
            ("m2", "p-responses-b", responses_entry()),
        ],
        2,
    );

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_carrying_reasoning_dialect(), RouterOptions::new()),
    )
    .await;

    dispatched.result.expect_err("every entry fails");
    assert!(
        drop_warns(&events).is_empty(),
        "the Responses egress represents context/mode -- nothing is dropped"
    );
}
