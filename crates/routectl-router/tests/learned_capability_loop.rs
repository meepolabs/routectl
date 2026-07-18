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
use std::time::Duration;

mod common;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::SignalTier;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role, ToolDef};
use routectl_router::class_policy::{ClassPolicy, ConfigFailureClass};
use routectl_router::{
    AliasValue, BuildOptions, Config, DispatchedStream, ModelEntry, ProviderEntry,
    ProviderRuntimePolicy, RetryPolicy, Router, RouterOptions, build_resolved_models,
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
    // emit an event. The reached same-capability settle also emits one
    // probe-settlement event tagged same_capability (reached_target=true).
    for req in 2..=3 {
        let (d, events) = routectl_testkit::with_capture(complete(&router, "chain")).await;
        assert!(d.result.is_ok(), "re-probe {req} rejected, falls back to B");
        assert!(
            d.meta.learned_capabilities.is_empty(),
            "re-probe {req}: a same-capability settle emits no learn event",
        );
        assert_eq!(hits(&a).await, req, "re-probe {req} dialed A exactly once");
        let ev = events
            .iter()
            .find(|e| {
                e.field("event") == Some("probe_settlement")
                    && e.field("outcome") == Some("same_capability")
            })
            .unwrap_or_else(|| {
                panic!("re-probe {req} must emit a same_capability settlement: {events:?}")
            });
        assert_eq!(ev.field("reached_target"), Some("true"));
        assert_eq!(ev.field("reason"), Some("same_capability"));
        assert_eq!(ev.field("capability_key"), Some(WEB_SEARCH));
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

// ---------------------------------------------------------------------------
// Streaming-egress harness: the SSE mock responder ported from the router
// mock-provider suite, wired onto the SAME wiremock openai-compat egress this
// file already uses so the learned-probe loop runs over `router.stream()`.
//
// A 2xx step serves a byte-accurate `text/event-stream` body the openai-compat
// streaming egress parses into chunks (so `try_stream_with_first_chunk` yields
// a first chunk and the dispatch returns Ok); a non-2xx step serves the JSON
// error envelope verbatim, so the `provider.stream()` open call fails with the
// SAME real classification the complete path learns from. This keeps the
// learned-capability loop identical across surfaces while exercising the
// streaming dispatch body (`stream_inner`) and its `ProbeAdmissionSet`.
// ---------------------------------------------------------------------------

const PROVIDER_KIND: &str = "openai-compat";

/// A minimal, valid openai-compat SSE success body: one content chunk, one
/// terminal chunk carrying `finish_reason`, then the `[DONE]` sentinel. The
/// egress relabels the model, so the wire model string is a placeholder.
fn sse_ok_body() -> String {
    concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"upstream-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string()
}

/// The streaming sibling of [`SequencedResponder`]: walks a fixed status
/// sequence across calls, serving an SSE body (`text/event-stream`) for a 2xx
/// step and the raw JSON error envelope for a non-2xx step. `success_delay`
/// (applied only to a 2xx step) holds the stream open long enough for a
/// cancellation test to drop the dispatch future before the first chunk lands.
struct StreamSequencedResponder {
    calls: AtomicUsize,
    steps: Vec<(u16, String)>,
    success_delay: Option<Duration>,
}

impl Respond for StreamSequencedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let (status, body) = self.steps.get(i).unwrap_or_else(|| {
            self.steps
                .last()
                .expect("StreamSequencedResponder needs at least one step")
        });
        let is_success = (200..300).contains(status);
        let content_type = if is_success {
            "text/event-stream"
        } else {
            "application/json"
        };
        let mut tpl = ResponseTemplate::new(*status)
            .insert_header("content-type", content_type)
            .set_body_string(body.clone());
        if is_success && let Some(delay) = self.success_delay {
            tpl = tpl.set_delay(delay);
        }
        tpl
    }
}

/// A wiremock upstream that answers `POST /chat/completions` streaming: a 2xx
/// step serves the canonical SSE success body, a non-2xx step serves its JSON
/// envelope. Mirrors [`upstream_server`] for the stream surface.
async fn sse_upstream_server(steps: Vec<(u16, Value)>) -> MockServer {
    sse_upstream_server_delayed(steps, None).await
}

/// [`sse_upstream_server`] with an optional first-byte delay on every 2xx step
/// (the cancellation test uses it to keep the first chunk pending).
async fn sse_upstream_server_delayed(
    steps: Vec<(u16, Value)>,
    success_delay: Option<Duration>,
) -> MockServer {
    let steps = steps
        .into_iter()
        .map(|(status, body)| {
            let payload = if (200..300).contains(&status) {
                sse_ok_body()
            } else {
                body.to_string()
            };
            (status, payload)
        })
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(StreamSequencedResponder {
            calls: AtomicUsize::new(0),
            steps,
            success_delay,
        })
        .mount(&server)
        .await;
    server
}

/// Build a streaming-egress router: same openai-compat wiremock providers as
/// [`build_router`], but with an explicit alias table and retry policy so a
/// test can pin a failure class terminal or point two aliases at shared
/// models.
async fn build_stream_router(
    upstreams: Vec<Upstream>,
    aliases_spec: &[(&str, &[&str])],
    decay_hours: u64,
    retry: RetryPolicy,
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
    for (alias, chain) in aliases_spec {
        let value = if chain.len() == 1 {
            AliasValue::Single(chain[0].to_string())
        } else {
            AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
        };
        aliases.insert((*alias).to_string(), value);
    }

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry,
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

/// Dispatch a streaming request against `alias` carrying `features`.
async fn stream_with(router: &Router, alias: &str, features: &[&str]) -> DispatchedStream {
    router
        .stream_with_options(req_with_features(alias, features), RouterOptions::default())
        .await
}

/// Dispatch a streaming `web_search` request against `alias`.
async fn stream(router: &Router, alias: &str) -> DispatchedStream {
    stream_with(router, alias, &[WEB_SEARCH]).await
}

/// Fully consume a dispatched stream so its egress runs to `[DONE]`. A no-op
/// for an error dispatch (nothing to drain).
async fn drain(dispatched: DispatchedStream) {
    if let Ok(mut s) = dispatched.result {
        while s.next().await.is_some() {}
    }
}

/// True when `events` carries a probe-settlement event for an UNREACHED
/// admission of `capability` on the streaming surface -- the observable that
/// the `ProbeAdmissionSet` drop released the `in_flight` slot of an admission
/// the dispatch never reached.
fn has_unreached_stream_settlement(
    events: &[routectl_testkit::CapturedEvent],
    capability: &str,
) -> bool {
    events.iter().any(|e| {
        e.field("event") == Some("probe_settlement")
            && e.field("surface") == Some("stream")
            && e.field("capability_key") == Some(capability)
            && e.field("provider_kind") == Some(PROVIDER_KIND)
            && e.field("outcome") == Some("other_error")
            && e.field("reached_target") == Some("false")
            && e.field("reason") == Some("unreached")
    })
}

// ---------------------------------------------------------------------------
// Stream mirror of Leg 2: expiry -> exactly one re-probe -> 2xx clears, driven
// through `router.stream()`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_expiry_admits_single_reprobe_then_success_clears() {
    let a = sse_upstream_server(vec![
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

    let d1 = stream(&router, "solo").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].observations, 1);
    assert_eq!(hits(&a).await, 1);

    let d2 = stream(&router, "solo").await;
    assert!(
        d2.result.is_ok(),
        "the streaming re-probe must reach A and open a stream",
    );
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d2.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 2, "exactly one re-probe dialed A");
    drain(d2).await;

    let d3 = stream(&router, "solo").await;
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
// Stream mirror of Leg 7: two distinct learned negatives on ONE target both
// re-probe and both settle on the stream surface -- neither admission leaks
// its in_flight slot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_two_expired_negatives_on_one_target_both_reprobe_and_clear() {
    let a = sse_upstream_server(vec![
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

    let d1 = stream_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d1.meta.learned_capabilities.len(), 1);
    assert_eq!(d1.meta.learned_capabilities[0].capability_key, WEB_SEARCH);

    let d2 = stream_with(&router, "solo", &[COMPUTER_USE]).await;
    assert!(matches!(
        d2.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(d2.meta.learned_capabilities.len(), 1);
    assert_eq!(d2.meta.learned_capabilities[0].capability_key, COMPUTER_USE);
    assert_eq!(hits(&a).await, 2);

    let d3 = stream_with(&router, "solo", &[WEB_SEARCH, COMPUTER_USE]).await;
    assert!(
        d3.result.is_ok(),
        "the double streaming re-probe must reach A and open a stream",
    );
    assert_eq!(d3.meta.served_provider.as_deref(), Some("prov_a"));
    assert!(d3.meta.learned_capabilities.is_empty());
    assert_eq!(hits(&a).await, 3, "one dispatch carried both probes");
    drain(d3).await;

    let d4 = stream_with(&router, "solo", &[WEB_SEARCH]).await;
    assert!(matches!(
        d4.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d4.meta.learned_capabilities[0].observations, 1,
        "F1 must relearn from scratch -- its probe slot did not leak",
    );

    let d5 = stream_with(&router, "solo", &[COMPUTER_USE]).await;
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
// Early-exit / cancellation matrix on the stream path. Each case admits a
// re-probe on a target the dispatch never reaches (or a target whose dispatch
// is cancelled), then asserts the `ProbeAdmissionSet` drop released the slot:
// the streaming-surface probe-settlement event for the unreached admission.
// ---------------------------------------------------------------------------

/// success on an earlier target: A (head) re-probes and its stream opens; B
/// (admitted, tail) is never reached and its slot must reset.
#[tokio::test]
async fn stream_success_on_earlier_target_releases_unreached_admission() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
        (200, ok_body()),                        // req 2: re-probe A opens a stream
    ])
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    // req 1: both chain members learn the negative (decay 0 -> both lapse).
    let d1 = stream(&router, "chain").await;
    assert!(matches!(
        d1.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: A re-probes and succeeds at the head; B's admission is unreached.
    let (d2, events) = routectl_testkit::with_capture(stream(&router, "chain")).await;
    assert!(d2.result.is_ok(), "the head re-probe opens a stream");
    assert_eq!(d2.meta.served_provider.as_deref(), Some("prov_a"));
    assert_eq!(hits(&b).await, 1, "B (tail) was never reached");
    drain(d2).await;
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "the unreached tail admission must settle on the stream surface: {events:?}",
    );
}

/// terminal (non-fallbackable) error on the head: A's 400 is pinned terminal
/// so the loop returns without hopping to the admitted tail B.
#[tokio::test]
async fn stream_terminal_error_releases_unreached_admission() {
    // A's plain 400 classifies BadRequest; pinning it `fallback = false` makes
    // it terminal so the loop returns at A without reaching B.
    let mut retry = fast_retry();
    retry.classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let plain_400 = json!({"error": {"type": "invalid_request_error", "message": "bad"}});
    let a = sse_upstream_server(vec![(400, plain_400)]).await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
    let router = build_stream_router(
        vec![
            Upstream::openai("m_a", "prov_a", &a.uri()),
            Upstream::openai("m_b", "prov_b", &b.uri()),
        ],
        &[("learn_b", &["m_b"]), ("chain", &["m_a", "m_b"])],
        0,
        retry,
    )
    .await;

    // Seed B's negative through the solo alias (decay 0 -> lapses to a re-probe).
    let d0 = stream(&router, "learn_b").await;
    assert!(matches!(
        d0.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(hits(&b).await, 1);

    // chain req: A fails terminally; B (admitted, expired) is never reached.
    let (d1, events) = routectl_testkit::with_capture(stream(&router, "chain")).await;
    assert!(
        matches!(d1.result, Err(Error::Upstream { status: 400, .. })),
        "a non-fallbackable terminal error must not fall back",
    );
    assert_eq!(hits(&b).await, 1, "B was never dialed after the terminal A");
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "the terminal early return must settle the unreached admission: {events:?}",
    );
}

/// `disable_fallbacks` breaks the chain before the hop; the admitted tail B is
/// never reached and its slot must reset.
#[tokio::test]
async fn stream_disable_fallbacks_break_releases_unreached_admission() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
        (500, health_body()),                    // req 2: fallbackable, but broken by opts
    ])
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    // req 1: both learn (decay 0 -> both lapse into re-probes).
    let d1 = stream(&router, "chain").await;
    assert!(d1.result.is_err());
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: A errors; disable_fallbacks breaks before the hop to admitted B.
    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;
    let (d2, events) = routectl_testkit::with_capture(
        router.stream_with_options(req_with_feature("chain", WEB_SEARCH), opts),
    )
    .await;
    assert!(
        d2.result.is_err(),
        "disable_fallbacks propagates the head failure",
    );
    assert_eq!(
        hits(&b).await,
        1,
        "B was never reached under disable_fallbacks"
    );
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "a disable_fallbacks break must settle the unreached admission: {events:?}",
    );
}

/// future-drop (cancellation): the dispatch future is dropped mid-first-chunk
/// on the head A; the admitted tail B's slot must reset on the drop.
#[tokio::test]
async fn stream_future_drop_releases_unreached_admission() {
    // A's re-probe stream opens slowly (2s first-byte delay) so the dispatch
    // future is still awaiting the first chunk when it is dropped ~150ms in.
    let a = sse_upstream_server_delayed(
        vec![
            (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn A, fall to B
            (200, ok_body()),                        // req 2: slow re-probe, cancelled
        ],
        Some(Duration::from_secs(2)),
    )
    .await;
    let b = sse_upstream_server(vec![(400, unsupported_body_for(WEB_SEARCH))]).await;
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

    // req 1: both chain members learn.
    let d1 = stream(&router, "chain").await;
    assert!(d1.result.is_err());
    assert_eq!(hits(&a).await, 1);
    assert_eq!(hits(&b).await, 1);

    // req 2: drive the dispatch, then drop it before the first chunk arrives.
    // The drop runs the guard + set destructors on THIS current-thread runtime,
    // so the unreached tail admission settles under the capture subscriber.
    let ((), events) = routectl_testkit::with_capture(async {
        let fut = router.stream_with_options(
            req_with_feature("chain", WEB_SEARCH),
            RouterOptions::default(),
        );
        let cancelled = tokio::time::timeout(Duration::from_millis(150), fut).await;
        assert!(
            cancelled.is_err(),
            "the slow first chunk must keep the future pending until it is dropped",
        );
    })
    .await;
    assert_eq!(
        hits(&b).await,
        1,
        "B was never reached (A's stream never opened)"
    );
    assert!(
        has_unreached_stream_settlement(&events, WEB_SEARCH),
        "dropping the dispatch future must settle the unreached admission: {events:?}",
    );
}

/// no-double-settle: a solo target reached and cleared by its own guard's 2xx
/// is settled EXACTLY ONCE -- one probe-settlement event from the guard
/// (reached_target=true, outcome=success) and NOT a second from the set drop.
#[tokio::test]
async fn stream_reached_admission_settled_by_guard_not_by_set() {
    let a = sse_upstream_server(vec![
        (400, unsupported_body_for(WEB_SEARCH)), // req 1: learn
        (200, ok_body()),                        // req 2: re-probe reaches A and clears
        (400, unsupported_body_for(WEB_SEARCH)), // req 3: fresh learn (proves the clear)
    ])
    .await;
    let router = build_router(
        vec![Upstream::openai("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
        0,
    )
    .await;

    let d1 = stream(&router, "solo").await;
    assert!(d1.result.is_err());

    let (d2, events) = routectl_testkit::with_capture(stream(&router, "solo")).await;
    assert!(
        d2.result.is_ok(),
        "the re-probe reaches A and opens a stream"
    );
    assert!(
        d2.meta.learned_capabilities.is_empty(),
        "a cleared re-probe emits no fresh learn event",
    );
    drain(d2).await;
    let settlements: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("probe_settlement"))
        .collect();
    assert_eq!(
        settlements.len(),
        1,
        "a reached admission settles exactly once (guard only, no set double-settle): {events:?}",
    );
    let ev = settlements[0];
    assert_eq!(ev.field("state_key"), Some("m_a"));
    assert_eq!(ev.field("surface"), Some("stream"));
    assert_eq!(ev.field("outcome"), Some("success"));
    assert_eq!(ev.field("reached_target"), Some("true"));
    assert_eq!(ev.field("reason"), Some("success"));

    // The negative was cleared by the guard: the next request relearns fresh.
    let d3 = stream(&router, "solo").await;
    assert!(matches!(
        d3.result,
        Err(Error::Upstream { status: 400, .. })
    ));
    assert_eq!(
        d3.meta.learned_capabilities[0].observations, 1,
        "the successful re-probe cleared the negative via the target guard",
    );
}
