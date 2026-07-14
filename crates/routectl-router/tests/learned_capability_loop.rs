//! End-to-end learned-capability loop, driven at the router level against
//! wiremock upstreams.
//!
//! A wiremock provider 400s a named capability on a request that carries
//! it; the router learns the negative (self-identifying tier), the chain
//! filter de-prioritizes that target on subsequent matching requests, an
//! expiry admits exactly one re-probe, and a 2xx clears the entry. The
//! never-learn guards (operator-remapped classification, health-status
//! errors) and the D17 route-away-with-floor invariant are pinned in the
//! same shape.
//!
//! The loop is observed through the PUBLIC router surface only:
//! `Dispatched.meta.learned_capabilities` (the per-request learn events),
//! `Dispatched.meta.served_provider`, the dispatch result, and wiremock
//! per-server hit counts. Each upstream gets its own `MockServer`, so a
//! hit count is exactly that target's dial count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::SignalTier;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role, ToolDef};
use routectl_router::class_policy::ConfigFailureClass;
use routectl_router::{
    AliasValue, BuildOptions, Config, ModelEntry, ProviderEntry, ProviderRuntimePolicy,
    RetryPolicy, Router, RouterOptions, build_resolved_models,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The openai-compat self-identifying `error.code` token the classifier
/// lifts to a `FeatureUnsupported` capability. It doubles as the request's
/// derived feature key (carried as a tool `type`) so the capture gate's
/// "capability was in flight" check passes.
const CAP_TOKEN: &str = "unsupported_parameter";

/// A second self-identifying token, distinct from [`CAP_TOKEN`], so one
/// target can carry two independent learned negatives.
const CAP_TOKEN_2: &str = "unsupported_value";

/// A wiremock responder that walks a fixed sequence of `(status, body)`
/// steps across successive calls, repeating the last step once the
/// sequence is exhausted. Deterministic and order-independent (a single
/// mounted mock), so response sequencing does not depend on wiremock's
/// mount-order matching semantics.
struct SequencedResponder {
    calls: AtomicUsize,
    steps: Vec<(u16, Value)>,
}

impl Respond for SequencedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let (status, body) = self.steps.get(i).unwrap_or_else(|| {
            self.steps
                .last()
                .expect("SequencedResponder needs at least one step")
        });
        ResponseTemplate::new(*status)
            .insert_header("content-type", "application/json")
            .set_body_json(body.clone())
    }
}

/// A wiremock upstream that answers `POST /chat/completions` with the
/// given response sequence.
async fn upstream_server(steps: Vec<(u16, Value)>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(SequencedResponder {
            calls: AtomicUsize::new(0),
            steps,
        })
        .mount(&server)
        .await;
    server
}

/// Number of requests this upstream has received.
async fn hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .map_or(0, |reqs| reqs.len())
}

/// One chain member: a `[models.<nickname>]` pointed at a
/// `[providers.<provider_name>]` openai-compat entry whose `base_url` is a
/// wiremock URL, plus the runtime policy for that provider.
struct Upstream {
    nickname: String,
    provider_name: String,
    base_url: String,
    runtime: ProviderRuntimePolicy,
}

impl Upstream {
    fn openai(nickname: &str, provider_name: &str, base_url: &str) -> Self {
        Self {
            nickname: nickname.to_string(),
            provider_name: provider_name.to_string(),
            base_url: base_url.to_string(),
            runtime: ProviderRuntimePolicy::default(),
        }
    }
}

/// Single-attempt, fast-backoff retry policy so a chain walk falls back
/// promptly without wall-clock sleeps.
fn fast_retry() -> RetryPolicy {
    let mut r = RetryPolicy::default();
    r.max_attempts = 1;
    r.initial_backoff_ms = 1;
    r.backoff_multiplier = 1.0;
    r
}

/// Build a router whose chain resolves `alias` to `chain` (nicknames in
/// order), with `[capability]` enabled at `decay_hours`. Providers are
/// real openai-compat egresses pointed at the wiremock URLs.
async fn build_router(
    upstreams: Vec<Upstream>,
    alias: &str,
    chain: &[&str],
    decay_hours: u64,
) -> Router {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for u in &upstreams {
        providers.insert(
            u.provider_name.clone(),
            ProviderEntry::openai_compat(&u.base_url, "literal:test-key")
                .with_runtime(u.runtime.clone()),
        );
        models.insert(
            u.nickname.clone(),
            ModelEntry::new(&u.provider_name, "upstream-model"),
        );
    }

    let mut aliases = BTreeMap::new();
    let value = if chain.len() == 1 {
        AliasValue::Single(chain[0].to_string())
    } else {
        AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
    };
    aliases.insert(alias.to_string(), value);

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: fast_retry(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = decay_hours;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");

    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

/// A request against `alias` carrying a built-in tool whose `type` is
/// `feature`, so `derive_feature_keys` yields `[feature]`.
fn req_with_feature(alias: &str, feature: &str) -> ChatRequest {
    ChatRequest {
        model: alias.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        tools: Some(vec![ToolDef::Other(json!({"type": feature, "name": "t"}))]),
        ..Default::default()
    }
}

/// A request against `alias` carrying built-in tools whose `type` is each
/// entry of `features`, so `derive_feature_keys` yields `features`.
fn req_with_features(alias: &str, features: &[&str]) -> ChatRequest {
    ChatRequest {
        model: alias.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        tools: Some(
            features
                .iter()
                .map(|f| ToolDef::Other(json!({"type": *f, "name": "t"})))
                .collect(),
        ),
        ..Default::default()
    }
}

/// An openai-compat 400 whose `error.code` names an unsupported parameter:
/// the classifier lifts this to `FeatureUnsupported { capability }`.
fn unsupported_body() -> Value {
    unsupported_body_for(CAP_TOKEN)
}

/// An openai-compat 400 whose `error.code` is `code`; the classifier lifts
/// it to `FeatureUnsupported { capability: code }`.
fn unsupported_body_for(code: &str) -> Value {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": code,
            "message": "The requested parameter is not supported by this model."
        }
    })
}

/// A generic upstream error body (health-status responses).
fn health_body() -> Value {
    json!({"error": {"type": "server_error", "message": "upstream trouble"}})
}

/// A minimal, valid OpenAI chat-completion success body.
fn ok_body() -> Value {
    json!({
        "id": "cmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

async fn complete(router: &Router, alias: &str) -> routectl_router::Dispatched {
    router
        .complete_with_options(req_with_feature(alias, CAP_TOKEN), RouterOptions::default())
        .await
}

/// Dispatch a request against `alias` carrying `features` as derived tool
/// types.
async fn complete_with(
    router: &Router,
    alias: &str,
    features: &[&str],
) -> routectl_router::Dispatched {
    router
        .complete_with_options(req_with_features(alias, features), RouterOptions::default())
        .await
}

// ---------------------------------------------------------------------------
// Leg 1: learn -> de-prioritize the matching target.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn learn_then_deprioritizes_matching_target() {
    // A always 400s the capability; B always succeeds. A large decay keeps
    // the learned negative acting so it de-prioritizes rather than
    // re-probing.
    let a = upstream_server(vec![(400, unsupported_body())]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        48,
    )
    .await;

    // Request 1: A rejects the capability (self-identifying learn), the
    // chain falls back to B.
    let d1 = complete(&router, "chain").await;
    assert!(
        d1.result.is_ok(),
        "request 1 should fall back to B: {:?}",
        d1.result.err()
    );
    assert_eq!(d1.meta.served_provider.as_deref(), Some("prov_b"));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        1,
        "A's rejection must produce exactly one learn event",
    );
    let ev = &d1.meta.learned_capabilities[0];
    assert_eq!(ev.feature_key, CAP_TOKEN);
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.upstream_status, 400);
    assert_eq!(ev.observations, 1);
    assert!(!ev.remapped);
    assert!(
        ev.request_features.iter().any(|f| f == CAP_TOKEN),
        "the learned capability must be in the request's derived feature set",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // Request 2: A is now an acting learned negative -> de-prioritized to
    // the tail. B serves first and A is never re-dialed.
    let d2 = complete(&router, "chain").await;
    assert!(d2.result.is_ok());
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_b"));
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "the skip must not manufacture a new learn event",
    );
    assert_eq!(
        hits(&a).await,
        1,
        "A must NOT be re-dialed: the learned negative de-prioritized it",
    );
    assert_eq!(hits(&b).await, 2);
}

// ---------------------------------------------------------------------------
// Leg 2: expiry -> exactly one re-probe -> 2xx clears.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expiry_admits_single_reprobe_then_success_clears() {
    // `decay_hours = 0` lapses a learned negative into a re-probe
    // immediately. A single-provider chain lets the hit count read the
    // re-probe directly.
    let a = upstream_server(vec![
        (400, unsupported_body()), // request 1: learn (observations = 1)
        (200, ok_body()),          // request 2: the admitted re-probe, clears
        (400, unsupported_body()), // request 3: a FRESH learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    // Request 1: A rejects; nothing to fall back to, so the 400 surfaces.
    // The negative is learned at one observation.
    let d1 = complete(&router, "solo").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    // Request 2: the negative has lapsed (decay 0), so this request is
    // admitted as the single re-probe to A. A's 2xx clears the entry.
    let d2 = complete(&router, "solo").await;
    assert!(
        d2.result.is_ok(),
        "the re-probe must reach A and succeed: {:?}",
        d2.result.err()
    );
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "a clearing re-probe emits no learn event",
    );
    assert_eq!(hits(&a).await, 2, "exactly one re-probe dialed A");

    // Request 3: the entry was cleared, so this rejection is a brand-new
    // negative at observations = 1. A stale, still-acting entry would have
    // been reconfirmed at observations >= 2 instead.
    let d3 = complete(&router, "solo").await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d3.meta.learned_capabilities.len(), 1);
    assert_eq!(
        d3.meta.learned_capabilities[0].observations, 1,
        "a cleared entry must relearn from scratch",
    );
    assert_eq!(hits(&a).await, 3);
}

// ---------------------------------------------------------------------------
// Leg 3: never-learn cases (health statuses, operator-remapped class).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_status_errors_never_learn() {
    // 429 / 5xx are health signals, not request faults: the learn gate
    // (400/422 only) excludes them.
    for status in [429u16, 500u16] {
        let a = upstream_server(vec![(status, health_body())]).await;
        let b = upstream_server(vec![(200, ok_body())]).await;
        let router = build_router(
            vec![
                Upstream::openai("m_a", "prov_a", &a.uri()),
                Upstream::openai("m_b", "prov_b", &b.uri()),
            ],
            "chain",
            &["m_a", "m_b"],
            48,
        )
        .await;

        let d = complete(&router, "chain").await;
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "status {status} must not learn",
        );
        assert!(
            d.result.is_ok(),
            "status {status}: chain should fall back to B",
        );
        assert_eq!(d.meta.served_provider.as_deref(), Some("prov_b"));
    }
}

#[tokio::test]
async fn remapped_classification_never_learns() {
    // An operator remap of A's 400s (`class_overrides`) sets `remapped =
    // true`, which the learn gate excludes -- even though the identical
    // body learns in leg 1.
    let a = upstream_server(vec![(400, unsupported_body())]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a
        .class_overrides
        .insert(400, ConfigFailureClass::BadRequest);
    let router = build_router(
        vec![
            Upstream {
                runtime: runtime_a,
                ..Upstream::openai("m_a", "prov_a", &a.uri())
            },
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        48,
    )
    .await;

    let d = complete(&router, "chain").await;
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "a remapped classification must not learn",
    );
    assert_eq!(hits(&a).await, 1, "A was still dialed (and remapped)");
    assert!(
        d.result.is_ok(),
        "remapped bad-request still falls back to B"
    );

    // The negative was not recorded, so a second request re-dials A rather
    // than de-prioritizing it.
    let d2 = complete(&router, "chain").await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(
        hits(&a).await,
        2,
        "no learned negative means A is not de-prioritized",
    );
}

// ---------------------------------------------------------------------------
// Leg 4: D17 route-away-with-floor vs statically-empty 501.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_targets_learned_still_attempts_the_tail() {
    // Both chain members reject the capability, so both become acting
    // learned negatives. A subsequent matching request must still ATTEMPT
    // the de-prioritized tail (D17), never hard-empty into NotImplemented.
    let a = upstream_server(vec![(400, unsupported_body())]).await;
    let b = upstream_server(vec![(400, unsupported_body())]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "d17",
        &["m_a", "m_b"],
        48,
    )
    .await;

    // Request 1: A and B both reject; both are learned (acting).
    let d1 = complete(&router, "d17").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        2,
        "both chain members must learn the negative",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // Request 2: every target is a learned negative -> the whole chain is
    // the de-prioritized tail. D17 still attempts it (a WARN marks the
    // route-away floor) instead of returning NotImplemented.
    let (d2, events) = routectl_testkit::with_capture(complete(&router, "d17")).await;
    assert!(
        matches!(d2.result, Err(Error::Upstream { status: 400, .. })),
        "a learned-only chain must still attempt (not 501): {:?}",
        d2.result,
    );
    assert!(
        !matches!(d2.result, Err(Error::NotImplemented(..))),
        "learned negatives must never hard-empty the chain",
    );
    assert!(
        hits(&a).await >= 2 && hits(&b).await >= 2,
        "the learned tail must be attempted (both targets re-dialed)",
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.message.contains("de-prioritized learned tail")),
        "entering the D17 tail must emit a WARN",
    );
}

#[tokio::test]
async fn statically_unsupported_chain_returns_not_implemented() {
    // A STATIC `unsupported_features` match hard-drops the only chain
    // member, emptying the chain: NotImplemented, and the upstream is
    // never dialed. (Contrast with the learned tail above.)
    let a = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a.unsupported_features = vec![CAP_TOKEN.to_string()];
    let router = build_router(
        vec![Upstream {
            runtime: runtime_a,
            ..Upstream::openai("m_a", "prov_a", &a.uri())
        }],
        "solo",
        &["m_a"],
        48,
    )
    .await;

    let d = complete(&router, "solo").await;
    assert!(
        matches!(d.result, Err(Error::NotImplemented(..))),
        "a statically-empty chain must return NotImplemented: {:?}",
        d.result,
    );
    assert_eq!(
        hits(&a).await,
        0,
        "a statically unsupported target is hard-dropped, never dialed",
    );
}

// ---------------------------------------------------------------------------
// Leg 5: two distinct learned negatives on ONE target both re-probe and
// both settle -- neither admission leaks its in_flight slot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_expired_negatives_on_one_target_both_reprobe_and_clear() {
    // One solo target learns TWO distinct capability negatives (F1, F2).
    // `decay_hours = 0` lapses both immediately, so a request carrying both
    // features admits TWO re-probes on the same target in one dispatch. A
    // 2xx must settle BOTH: each entry clears and neither in_flight slot
    // latches. The fresh relearns in requests 4/5 prove both cleared -- a
    // leaked slot would leave its feature routed-away (reconfirmed at
    // observations >= 2) instead of relearning from scratch.
    let a = upstream_server(vec![
        (400, unsupported_body_for(CAP_TOKEN)),   // req 1: learn F1
        (400, unsupported_body_for(CAP_TOKEN_2)), // req 2: learn F2
        (200, ok_body()),                         // req 3: the double re-probe clears both
        (400, unsupported_body_for(CAP_TOKEN)),   // req 4: fresh learn F1 (proves clear)
        (400, unsupported_body_for(CAP_TOKEN_2)), // req 5: fresh learn F2 (proves clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    // Request 1: A rejects F1; solo chain, so the 400 surfaces. F1 learned.
    let d1 = complete_with(&router, "solo", &[CAP_TOKEN]).await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].feature_key, CAP_TOKEN);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);

    // Request 2: A rejects F2. F2 learned as a second, independent negative.
    let d2 = complete_with(&router, "solo", &[CAP_TOKEN_2]).await;
    assert!(matches!(
        d2.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].feature_key, CAP_TOKEN_2);
    assert_eq!(d2.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 2);

    // Request 3: both negatives have lapsed (decay 0) and the request carries
    // both features, so A is admitted to re-probe BOTH in one dispatch. Its
    // 2xx must clear both entries. A clearing re-probe emits no learn event.
    let d3 = complete_with(&router, "solo", &[CAP_TOKEN, CAP_TOKEN_2]).await;
    assert!(
        d3.result.is_ok(),
        "the double re-probe must reach A and succeed: {:?}",
        d3.result.err()
    );
    assert_eq!(d3.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(
        d3.meta.learned_capabilities.is_empty(),
        "a clearing re-probe emits no learn event",
    );
    assert_eq!(
        hits(&a).await,
        3,
        "exactly one dispatch carried both probes"
    );

    // Request 4: F1's entry was cleared, so this rejection is a brand-new
    // negative at observations = 1. A leaked in_flight slot would have kept
    // F1 routed-away and reconfirmed it at observations >= 2 instead.
    let d4 = complete_with(&router, "solo", &[CAP_TOKEN]).await;
    assert!(matches!(
        d4.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d4.meta.learned_capabilities.len(), 1);
    assert_eq!(d4.meta.learned_capabilities[0].feature_key, CAP_TOKEN);
    assert_eq!(
        d4.meta.learned_capabilities[0].observations, 1,
        "F1 must relearn from scratch -- its probe slot did not leak",
    );

    // Request 5: same proof for F2's cleared entry.
    let d5 = complete_with(&router, "solo", &[CAP_TOKEN_2]).await;
    assert!(matches!(
        d5.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d5.meta.learned_capabilities.len(), 1);
    assert_eq!(d5.meta.learned_capabilities[0].feature_key, CAP_TOKEN_2);
    assert_eq!(
        d5.meta.learned_capabilities[0].observations, 1,
        "F2 must relearn from scratch -- its probe slot did not leak",
    );
    assert_eq!(hits(&a).await, 5);
}

#[tokio::test]
async fn mixed_outcome_settles_matched_probe_and_drops_the_rest() {
    // One solo target holds two expired negatives (F1, F2) and is admitted to
    // re-probe BOTH on a single request. The dispatch rejects the SAME
    // capability for F1 only: that admission must settle as a same-capability
    // refresh (no fresh observe event, its own backoff owns the bump) while
    // the still-held F2 admission drops as OtherError -- releasing F2's
    // in_flight slot without clearing the entry, so F2 stays re-probable.
    let a = upstream_server(vec![
        (400, unsupported_body_for(CAP_TOKEN)),   // req 1: learn F1
        (400, unsupported_body_for(CAP_TOKEN_2)), // req 2: learn F2
        (400, unsupported_body_for(CAP_TOKEN)),   // req 3: double re-probe, F1 rejects again
        (200, ok_body()),                         // req 4: F2's fresh re-probe clears it
        (400, unsupported_body_for(CAP_TOKEN_2)), // req 5: fresh learn F2 (proves clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = complete_with(&router, "solo", &[CAP_TOKEN]).await;
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    let d2 = complete_with(&router, "solo", &[CAP_TOKEN_2]).await;
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].observations, 1);

    // Request 3: both negatives lapsed and both features present, so A is
    // admitted to re-probe both. A re-rejects F1 (same capability). That
    // admission settles via the capped-backoff path -- so NO learn event is
    // emitted (a same-capability probe settle owns the observation bump
    // itself). The unmatched F2 admission is dropped as OtherError.
    let d3 = complete_with(&router, "solo", &[CAP_TOKEN, CAP_TOKEN_2]).await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert!(
        d3.meta.learned_capabilities.is_empty(),
        "F1's rejection must settle its own held probe, not emit a fresh observe event",
    );
    assert_eq!(hits(&a).await, 3);

    // Request 4: F2's admission was released (OtherError, not cleared), so F2
    // is still an expired negative -- this request re-probes it, and the 2xx
    // clears it. A leaked in_flight slot would have kept F2 routed-away.
    let d4 = complete_with(&router, "solo", &[CAP_TOKEN_2]).await;
    assert!(
        d4.result.is_ok(),
        "F2's slot was released, so it re-probes: {:?}",
        d4.result.err()
    );
    assert_eq!(d4.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d4.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 4);

    // Request 5: F2 was cleared by its re-probe, so this rejection relearns
    // from scratch at observations = 1.
    let d5 = complete_with(&router, "solo", &[CAP_TOKEN_2]).await;
    assert!(matches!(
        d5.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d5.meta.learned_capabilities.len(), 1);
    assert_eq!(d5.meta.learned_capabilities[0].feature_key, CAP_TOKEN_2);
    assert_eq!(
        d5.meta.learned_capabilities[0].observations, 1,
        "F2 must relearn from scratch -- its dropped probe released the slot",
    );
    assert_eq!(hits(&a).await, 5);
}
