//! End-to-end learned-capability loop, driven at the router level against
//! wiremock upstreams with REAL upstream error envelopes.
//!
//! The headline behavior this suite proves: an openai-compat upstream rejects
//! a capability the request NATURALLY carries AND that actually survives
//! egress onto the wire -- a structured-output request whose
//! `output_config.format` the egress lifts to a top-level `response_format`
//! field -- with a byte-accurate 400 whose `/error/param` names that same
//! wire field. The router translates the rejected param to its canonical key
//! (`structured_output`), the capture-side request-membership gate admits it
//! because the request derived that same key, the negative is learned under
//! it, and the chain filter routes away from the rejecting target on
//! subsequent matching requests. A wire-body assertion on the OUTBOUND request
//! confirms the rejected surface actually crossed the wire -- the guard
//! against a synthetic proof where the rejected capability was dropped at
//! egress and a real upstream could never have rejected it.
//!
//! The loop is observed through the PUBLIC router surface only:
//! `Dispatched.meta.learned_capabilities` (the per-request learn events),
//! `Dispatched.meta.served_provider`, the dispatch result, and wiremock
//! per-server hit counts + captured request bodies. Each upstream gets its
//! own `MockServer`, so a hit count is exactly that target's dial count.

use std::collections::BTreeMap;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::SignalTier;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role, ToolDef};
use routectl_router::class_policy::ConfigFailureClass;
use routectl_router::{
    AliasValue, BuildOptions, Config, ModelEntry, ProviderEntry, ProviderRuntimePolicy,
    RetryPolicy, Router, RouterOptions, build_resolved_models,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A route-away capability the request carries as a built-in tool `type`
/// and the upstream names in `/error/param`. `web_search` is a policy
/// essential (`action_for` -> RouteAway), so a learned negative
/// de-prioritizes the target rather than stripping.
const WEB_SEARCH: &str = "web_search";

/// A second distinct route-away capability, so one target can carry two
/// independent learned negatives.
const COMPUTER_USE: &str = "computer_use";

/// The canonical constrained-decoding capability key. A structured-output
/// request derives this key; the openai wire param is `response_format`, and
/// the resolver's closed-set translation table maps the two.
const STRUCTURED_OUTPUT: &str = "structured_output";

/// A byte-accurate openai-compat `unsupported_parameter` 400 whose
/// `/error/param` is `response_format` -- the top-level wire field the
/// openai-compat egress emits for a structured-output request. This is the
/// surface that SURVIVES egress: a real upstream can genuinely reject it,
/// unlike a built-in tool the egress drops before the wire. The resolver
/// translates `response_format` onto the canonical `structured_output` key
/// the request side derives from `output_config.format`.
const OPENAI_UNSUPPORTED_RESPONSE_FORMAT_400: &str = r#"{"error":{"message":"Unsupported parameter: 'response_format' is not supported with this model.","type":"invalid_request_error","param":"response_format","code":"unsupported_parameter"}}"#;

/// A wiremock responder that walks a fixed sequence of `(status, body)`
/// steps across successive calls, repeating the last step once the
/// sequence is exhausted. Bodies are raw strings so a captured envelope is
/// served byte-for-byte. Deterministic and order-independent (a single
/// mounted mock).
struct SequencedResponder {
    calls: AtomicUsize,
    steps: Vec<(u16, String)>,
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
            .set_body_string(body.clone())
    }
}

/// A wiremock upstream that answers `POST /chat/completions` with the given
/// raw response sequence.
async fn upstream_server(steps: Vec<(u16, Value)>) -> MockServer {
    let steps = steps
        .into_iter()
        .map(|(status, body)| (status, body.to_string()))
        .collect();
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

/// A wiremock upstream that serves the exact captured `body` string on
/// every call (byte-for-byte), for the byte-accurate real-envelope proof.
async fn raw_upstream_server(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    let body = body.to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(SequencedResponder {
            calls: AtomicUsize::new(0),
            steps: vec![(status, body)],
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

/// The parsed JSON body of the most recent request this upstream received.
/// Used to assert on the OUTBOUND wire: what routectl actually sent, not
/// what the request carried before egress.
async fn last_request_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.expect("received requests");
    let last = reqs.last().expect("at least one request received");
    serde_json::from_slice(&last.body).expect("outbound request body is JSON")
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
/// order), with `[capability]` enabled at `decay_hours`. Providers are real
/// openai-compat egresses pointed at the wiremock URLs.
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
            ProviderEntry::openai_compat(&u.base_url, common::file_ref("test-key"))
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
    req_with_features(alias, &[feature])
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

/// A request against `alias` carrying an Anthropic-shape structured-output
/// `output_config.format`. `derive_feature_keys` yields `[structured_output]`,
/// and the openai-compat egress lifts the format to a top-level
/// `response_format` on the wire body -- so the capability the upstream
/// rejects by `error.param="response_format"` actually crosses the wire.
fn req_with_structured_output(alias: &str) -> ChatRequest {
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
        provider_extras: Some(json!({
            "output_config": { "format": {"type": "json_object"} }
        })),
        ..Default::default()
    }
}

/// An openai-compat 400 whose `/error/param` is `param`; the classifier
/// lifts the `unsupported_parameter` code to `FeatureUnsupported` and the
/// resolver reads `param` as the real capability.
fn unsupported_body_for(param: &str) -> Value {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": param,
            "message": format!("Unsupported parameter: '{param}' is not supported with this model.")
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
    complete_with(router, alias, &[WEB_SEARCH]).await
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
// The headline real-envelope route-away proof. The rejected capability
// (`structured_output` -> `response_format` on the wire) SURVIVES egress, so
// this is a scenario a real openai-compat upstream can genuinely produce -- a
// wire-body assertion guards against the synthetic failure mode where the
// rejected surface was dropped before the wire.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn real_envelope_response_format_400_learns_structured_output_and_routes_away() {
    // A serves a byte-accurate captured openai `unsupported_parameter` 400
    // whose `/error/param` is `response_format`. The request carries
    // `provider_extras.output_config.format`, so `derive_feature_keys` yields
    // `structured_output` AND the openai-compat egress lifts the format to a
    // top-level `response_format` that actually crosses the wire. The resolver
    // translates the rejected param onto `structured_output`; both sides meet
    // on that canonical key and the router learns the negative and routes away
    // from A. B always succeeds.
    let a = raw_upstream_server(400, OPENAI_UNSUPPORTED_RESPONSE_FORMAT_400).await;
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

    // Request 1: A rejects the structured-output surface it received, the
    // router learns it (self-identifying) under the CANONICAL key, and the
    // chain falls back to B.
    let d1 = router
        .complete_with_options(
            req_with_structured_output("chain"),
            RouterOptions::default(),
        )
        .await;
    assert!(
        d1.result.is_ok(),
        "request 1 should fall back to B: {:?}",
        d1.result.err()
    );
    assert_eq!(d1.meta.served_provider.as_deref(), Some("prov_b"));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        1,
        "A's real-envelope rejection must produce exactly one learn event",
    );
    let ev = &d1.meta.learned_capabilities[0];
    assert_eq!(
        ev.capability_key, STRUCTURED_OUTPUT,
        "the learned key is the canonical capability, not the raw wire param",
    );
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.upstream_status, 400);
    assert_eq!(ev.observations, 1);
    assert!(!ev.remapped);
    assert!(
        ev.request_features.iter().any(|f| f == STRUCTURED_OUTPUT),
        "the request naturally derives the learned capability -- the capture \
         membership gate admits it precisely because it is in this set",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // WIRE GUARD: the surface the upstream rejected (`response_format`) was
    // actually present on the OUTBOUND body A received, and the Anthropic-shape
    // `output_config` did not leak. This is the guard against the synthetic
    // failure mode -- a capability dropped at egress that a real upstream could
    // never have rejected.
    let sent = last_request_body(&a).await;
    assert!(
        sent.get("response_format").is_some(),
        "the rejected surface must have crossed the wire; body = {sent}",
    );
    assert!(
        sent.get("output_config").is_none(),
        "the Anthropic-shape output_config must not leak onto the openai wire; body = {sent}",
    );

    // Request 2: A is now an acting learned negative for `structured_output`
    // (an essential -> RouteAway) -> the chain filter de-prioritizes it to the
    // tail. B serves first and A is never re-dialed.
    let d2 = router
        .complete_with_options(
            req_with_structured_output("chain"),
            RouterOptions::default(),
        )
        .await;
    assert!(d2.result.is_ok());
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_b"));
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "the skip must not manufacture a new learn event",
    );
    assert_eq!(
        hits(&a).await,
        1,
        "A must NOT be re-dialed: the learned negative routed away from it",
    );
    assert_eq!(hits(&b).await, 2);
}

// ---------------------------------------------------------------------------
// Leg 1: learn -> de-prioritize the matching target.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn learn_then_deprioritizes_matching_target() {
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    let d1 = complete(&router, "chain").await;
    assert!(d1.result.is_ok(), "request 1 falls back to B");
    assert_eq!(d1.meta.served_provider.as_deref(), Some("prov_b"));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    let ev = &d1.meta.learned_capabilities[0];
    assert_eq!(ev.capability_key, WEB_SEARCH);
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert!(ev.request_features.iter().any(|f| f == WEB_SEARCH));
    assert_eq!(hits(&a).await, 1);

    let d2 = complete(&router, "chain").await;
    assert!(d2.result.is_ok());
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_b"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 1, "learned negative de-prioritized A");
    assert_eq!(hits(&b).await, 2);
}

// ---------------------------------------------------------------------------
// Leg 2: expiry -> exactly one re-probe -> 2xx clears.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expiry_admits_single_reprobe_then_success_clears() {
    let a = upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // request 1: learn
        (200, ok_body()),                        // request 2: admitted re-probe clears
        (400, unsupported_body_for(WEB_SEARCH)), // request 3: fresh learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = complete(&router, "solo").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    let d2 = complete(&router, "solo").await;
    assert!(
        d2.result.is_ok(),
        "the re-probe must reach A and succeed: {:?}",
        d2.result.err()
    );
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "exactly one re-probe dialed A");

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
// Leg 3: never-learn cases (health statuses, operator-remapped, forwarded).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_status_errors_never_learn() {
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
        assert!(d.result.is_ok(), "status {status}: fall back to B");
        assert_eq!(d.meta.served_provider.as_deref(), Some("prov_b"));
    }
}

#[tokio::test]
async fn remapped_classification_never_learns() {
    // An operator remap of A's 400s to FeatureUnsupported sets `remapped =
    // true`. The 400 body still carries a resolvable `/error/param`, so
    // WITHOUT the remap guard the resolver would learn `web_search`; the
    // `remapped` early return is the only thing that blocks it.
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a
        .class_overrides
        .insert(400, ConfigFailureClass::FeatureUnsupported);
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
    assert!(d.result.is_ok(), "remapped still falls back to B");

    let d2 = complete(&router, "chain").await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(
        hits(&a).await,
        2,
        "no learned negative means A is not de-prioritized",
    );
}

#[tokio::test]
async fn forwarded_request_never_learns() {
    // A request carrying a forwarded bearer is a pass-through; the router
    // must never learn a negative from it (the request is not routectl's own
    // catalog request). Even a resolvable 400 records nothing.
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    let mut req = req_with_feature("chain", WEB_SEARCH);
    req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("fwd-token".to_string()));
    let d = router
        .complete_with_options(req, RouterOptions::default())
        .await;
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "a forwarded request must not learn",
    );
    assert!(d.result.is_ok(), "forwarded still falls back to B");
    assert_eq!(hits(&a).await, 1, "A was dialed (forwarded, not learned)");

    // No negative recorded, so a second forwarded request re-dials A.
    let mut req2 = req_with_feature("chain", WEB_SEARCH);
    req2.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("fwd-token".to_string()));
    let d2 = router
        .complete_with_options(req2, RouterOptions::default())
        .await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "A not de-prioritized");
}

#[tokio::test]
async fn unresolvable_rejection_does_not_learn() {
    // A 400 whose `/error/param` is absent (a paramless `unsupported_value`)
    // names no capability the resolver can attribute -> no learn, and A is
    // never de-prioritized on the next request.
    let paramless = json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_value",
            "message": "Unsupported value."
        }
    });
    let a = upstream_server(vec![(400, paramless)]).await;
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
        "an unresolvable rejection must not learn",
    );
    assert!(d.result.is_ok());

    let d2 = complete(&router, "chain").await;
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "A re-dialed: nothing was learned");
}

// ---------------------------------------------------------------------------
// Leg 4: same-capability probe backoff.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_capability_probe_backs_off_on_repeat_rejection() {
    // decay 0 lapses the negative into a re-probe on each request; every
    // re-probe hits the SAME capability rejection again. Each admission settles
    // via the capped-backoff path -- the probe owns its own observation bump,
    // so it emits NO fresh learn event and keeps the entry acting (never
    // cleared). A cleared-then-relearned entry would instead emit a fresh learn
    // event at observations = 1; the sustained ABSENCE of learn events across
    // repeated re-probes is the proof the entry stayed on the backoff path.
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(200, ok_body())]).await;
    let router = build_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
        0,
    )
    .await;

    // Request 1: A rejects and the negative is learned (one learn event).
    let d1 = complete(&router, "chain").await;
    assert!(d1.result.is_ok(), "request 1 falls back to B");
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    // Requests 2 and 3: the negative lapses (decay 0) and A is admitted for a
    // single re-probe each time. A re-rejects the same capability, so each
    // admission settles as a same-capability backoff refresh -- NO fresh learn
    // event. If the entry had been cleared, this would relearn from scratch and
    // emit an event.
    for req in 2..=3 {
        let d = complete(&router, "chain").await;
        assert!(d.result.is_ok(), "re-probe {req} rejected, falls back to B");
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "re-probe {req}: a same-capability settle emits no learn event",
        );
        assert_eq!(hits(&a).await, req, "re-probe {req} dialed A exactly once");
    }
}

// ---------------------------------------------------------------------------
// Leg 6: D17 route-away-with-floor vs statically-empty 501.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_targets_learned_still_attempts_the_tail() {
    let a = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let b = upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    let d1 = complete(&router, "d17").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d1.meta.learned_capabilities.len(),
        2,
        "both chain members learn the negative",
    );
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

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
    // A STATIC `unsupported_features` match hard-drops the only chain member,
    // emptying the chain: NotImplemented, and the upstream is never dialed.
    let a = upstream_server(vec![(200, ok_body())]).await;
    let mut runtime_a = ProviderRuntimePolicy::default();
    runtime_a.unsupported_features = vec![WEB_SEARCH.to_string()];
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
// Leg 7: two distinct learned negatives on ONE target both re-probe and
// both settle -- neither admission leaks its in_flight slot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_expired_negatives_on_one_target_both_reprobe_and_clear() {
    let a = upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)),   // req 1: learn F1
        (400, unsupported_body_for(COMPUTER_USE)), // req 2: learn F2
        (200, ok_body()),                          // req 3: double re-probe clears both
        (400, unsupported_body_for(WEB_SEARCH)),   // req 4: fresh learn F1 (proves clear)
        (400, unsupported_body_for(COMPUTER_USE)), // req 5: fresh learn F2 (proves clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = complete_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].capability_key, WEB_SEARCH);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);

    let d2 = complete_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d2.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].capability_key, COMPUTER_USE);
    assert_eq!(hits(&a).await, 2);

    let d3 = complete_with(&router, "solo", &[WEB_SEARCH, COMPUTER_USE]).await;
    assert!(
        d3.result.is_ok(),
        "the double re-probe must reach A and succeed: {:?}",
        d3.result.err()
    );
    assert_eq!(d3.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d3.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 3, "one dispatch carried both probes");

    let d4 = complete_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d4.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d4.meta.learned_capabilities[0].observations, 1,
        "F1 must relearn from scratch -- its probe slot did not leak",
    );

    let d5 = complete_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d5.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d5.meta.learned_capabilities[0].observations, 1,
        "F2 must relearn from scratch -- its probe slot did not leak",
    );
    assert_eq!(hits(&a).await, 5);
}

// ---------------------------------------------------------------------------
// Live-network smoke variant (ignored in CI). Run with a real openai-compat
// base URL + key that rejects a structured-output request with a 400 whose
// `/error/param` is `response_format` -- the surface that actually SURVIVES
// egress (a built-in tool the egress drops never crosses the wire, so no real
// upstream could reject it). The resolver translates `response_format` onto
// the canonical `structured_output` key the request derives, and the capture
// membership gate admits it because the request carried that capability:
//   ROUTECTL_LIVE_BASE_URL=... ROUTECTL_LIVE_API_KEY=... \
//     cargo test -p routectl-router --test learned_capability_loop -- --ignored
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live network: requires ROUTECTL_LIVE_BASE_URL + ROUTECTL_LIVE_API_KEY"]
async fn live_openai_unsupported_parameter_is_learned() {
    let (Ok(base_url), Ok(api_key)) = (
        std::env::var("ROUTECTL_LIVE_BASE_URL"),
        std::env::var("ROUTECTL_LIVE_API_KEY"),
    ) else {
        panic!("set ROUTECTL_LIVE_BASE_URL and ROUTECTL_LIVE_API_KEY to run the live smoke");
    };

    let mut providers = BTreeMap::new();
    providers.insert(
        "live".to_string(),
        ProviderEntry::openai_compat(&base_url, common::file_ref(&api_key)),
    );
    let mut models = BTreeMap::new();
    models.insert("m_live".to_string(), ModelEntry::new("live", "gpt-4o-mini"));
    let mut aliases = BTreeMap::new();
    aliases.insert("live".to_string(), AliasValue::Single("m_live".to_string()));

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: fast_retry(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = 48;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);

    let d = router
        .complete_with_options(req_with_structured_output("live"), RouterOptions::default())
        .await;
    assert!(
        !d.meta.learned_capabilities.is_empty(),
        "a real upstream unsupported-parameter 400 must produce a learn event: {:?}",
        d.result,
    );
}
