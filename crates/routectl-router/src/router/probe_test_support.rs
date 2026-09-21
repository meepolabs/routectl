//! Shared fixtures for the bounded lazy probe-scheduler test sidecars.
//!
//! One home for the router builders and provider doubles the probe
//! sidecars share, so a fixture change cannot leave one sidecar
//! describing a router the others no longer build.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, Usage,
};

use super::Router;
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;
use crate::resolved::ResolvedModel;

/// The one grounded closed-table path, on the acting lane.
pub(super) const GROUNDED_PATH: &str = "thinking.enabled.display";

/// A request grounding the closed-table surface. Without this the
/// activation seam has no capability identity and every test below would
/// pass vacuously, so `a_request_grounding_nothing_activates_no_lane`
/// pins the converse.
pub(super) fn grounding_request() -> ChatRequest {
    let mut req = ChatRequest::default();
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    req
}

/// A provider whose `complete` / `stream` / `count_tokens` all fail with
/// a fixed upstream status, counting calls. Proves activation does not
/// depend on the upstream having SUCCEEDED.
pub(super) struct FailingProvider {
    pub(super) status: u16,
    pub(super) calls: AtomicUsize,
    /// `count_tokens` calls only -- the PROBE dial.
    ///
    /// Separate from `calls`, which every method bumps: a test asserting "the
    /// probe dialed once" against the shared counter would be reading the
    /// admitted dispatches too, and so would pass on an unbounded probe
    /// whenever the request count happened to match.
    pub(super) count_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for FailingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream("p1", self.status, "body"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream("p1", self.status, "body"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.count_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream("p1", self.status, "body"))
    }
}

pub(super) type BoxStreamAlias = futures::stream::BoxStream<'static, Result<ChatChunk>>;

/// A provider that succeeds, for the streaming and count_tokens
/// activation boundaries.
pub(super) struct OkProvider {
    pub(super) count_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for OkProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Ok(Box::pin(stream::iter(vec![Ok(ChatChunk::default())])))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.count_calls.fetch_add(1, Ordering::SeqCst);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

/// A provider whose `count_tokens` answers with a well-formed ZERO -- the
/// shape that spends a free step without settling the question.
pub(super) struct ZeroCountProvider {
    pub(super) calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for ZeroCountProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

/// A provider that RECORDS the probe body it was handed and rejects a body
/// missing the closed-table field under test.
///
/// The rejection is what makes the field's presence load-bearing: a fixture
/// that merely recorded the body would let a mutation removing the field
/// pass, because a plain count succeeds on any healthy lane.
pub(super) struct BodyAssertingProvider {
    pub(super) seen: parking_lot::Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl Provider for BodyAssertingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let carries_field = req.routectl_internal.anthropic_thinking_display.is_some();
        self.seen.lock().push(req);
        if !carries_field {
            // The upstream this stands in for answers a field-less count
            // perfectly well; refusing here is what turns "the probe forgot
            // the field" from a silent pass into a red test.
            return Err(Error::upstream(
                "p1",
                400,
                "probe body carried no field under test",
            ));
        }
        Ok(TokenCount {
            input_tokens: 5,
            extras: serde_json::Map::new(),
        })
    }
}

/// Plant the ONE verdict state the pre-flight planner may act on for
/// `state_key`: a resident ACTING field negative whose own incarnation carries
/// an acknowledged confirmation.
///
/// Both halves go through the registry's OWN seams -- the carry-over import for
/// the verdict, the canary cold-rebuild seed for the confirmation -- rather than
/// by reaching into resident state, so the planted shape is one live traffic can
/// actually produce. Asserts its own premise: a fixture that silently failed to
/// make the verdict eligible would make every "pre-flight stripped it" test
/// pass for the wrong reason.
pub(super) fn plant_eligible_verdict(router: &Router, state_key: &str) {
    let stamped = std::time::Instant::now();
    let feature_key = crate::field_capability::field_capability_key(GROUNDED_PATH)
        .expect("the grounded path is a well-formed qualified path");
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: feature_key.clone(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + std::time::Duration::from_hours(1),
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    let key = FieldVerdictKey::new(state_key, GROUNDED_PATH, "anthropic-api")
        .expect("a qualified path mints a key");
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        state_key,
        &feature_key,
        "anthropic-api",
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, 1, false);
    assert!(
        router.field_verdicts().preflight_eligible(
            &key,
            router.registry_generation(),
            std::time::Instant::now(),
        ),
        "fixture premise: {state_key} must be pre-flight eligible",
    );
}

/// Drive `count` identities each to a TERMINAL settlement, one at a time.
/// A well-formed zero count spends the one-step free plan, which tombstones
/// the identity -- the only way to reach terminal state through the real
/// worker rather than by poking the scheduler.
///
/// Every lane `m0..mN` must RESOLVE for that to be what happens: an
/// unresolvable identity settles `Unavailable` -> `Abandoned` instead. Both
/// paths lay a tombstone, so the `tombstoned` count -- which is what the
/// capacity and saturation tests assert -- comes out the SAME either way. That
/// is precisely why resolvability has to be arranged rather than assumed: those
/// assertions cannot detect the wrong path, and the tests that can
/// (`an_exhausted_identity_is_tombstoned_for_its_incarnation`,
/// `a_spent_free_plan_surfaces_a_paid_candidate_and_never_dials_one`) read
/// `free_exhausted_total` and paid candidacy, which an abandonment leaves at
/// zero. `remote_router_with_lanes` installs one resolved model per lane.
pub(super) async fn settle_terminally(router: &Router, count: usize) {
    for n in 0..count {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
        router.run_due_probes().await;
    }
}

/// Build a single-leg router on `base_url` with `provider` installed
/// under nickname `m1`, alias `default`.
pub(super) fn router_on_base(base_url: &str, provider: Arc<dyn Provider>) -> Router {
    router_on_base_with_failure_threshold(base_url, provider, Some(1))
}

/// `router_on_base` with an explicit breaker failure threshold.
///
/// `circuit_failures` is load-bearing for every test whose subject is the
/// breaker: at the default (`None`) the breaker is DISABLED, so a mutation
/// that made a probe debit the lane could not trip anything and a
/// "must not open the breaker" assertion would pass vacuously. `Some(1)`
/// means one recorded failure opens it, which is what makes those
/// assertions falsifiable. A test whose subject is call counts rather than
/// the breaker passes `None` deliberately, so an unrelated trip cannot
/// suppress its dials.
pub(super) fn router_on_base_with_failure_threshold(
    base_url: &str,
    provider: Arc<dyn Provider>,
    circuit_failures: Option<u32>,
) -> Router {
    build_router(base_url, provider, circuit_failures, None, 1)
}

pub(super) fn remote_router(provider: Arc<dyn Provider>) -> Router {
    router_on_base("https://api.anthropic.com", provider)
}

/// A remote router with an explicit RPM limit AND breaker threshold, for the
/// tests whose subject is what a declined probe does NOT consume.
///
/// The RPM limit is load-bearing: at the default (`None`) the token bucket is
/// disabled, `rpm_available` reads `None`, and an assertion that a deferral
/// charged no token would pass against code that charged one. Set BEFORE
/// `Router::new`, since that is where the per-provider gate is provisioned from
/// the config.
pub(super) fn remote_router_with_rpm(
    provider: Arc<dyn Provider>,
    rpm_limit: u32,
    circuit_failures: Option<u32>,
) -> Router {
    build_router(
        "https://api.anthropic.com",
        provider,
        circuit_failures,
        Some(rpm_limit),
        1,
    )
}

/// A remote router with `lanes` RESOLVABLE nicknames `m0..m{lanes-1}`, for the
/// terminal-capacity tests that drive one identity per lane.
///
/// Resolvability is the whole point: an identity with no resolved model settles
/// `Unavailable` -> `Abandoned`, which lays no tombstone, so driving those
/// tests against a single-lane router would measure abandonment while claiming
/// to measure exhaustion.
pub(super) fn remote_router_with_lanes(provider: Arc<dyn Provider>, lanes: usize) -> Router {
    build_router("https://api.anthropic.com", provider, Some(1), None, lanes)
}

/// THE single router builder every fixture above composes: one anthropic-api
/// provider entry `p1` on `base_url`, `lanes` models `m0..m{lanes-1}` all
/// resolved to `provider`, and `default` aliased to the first.
fn build_router(
    base_url: &str,
    provider: Arc<dyn Provider>,
    circuit_failures: Option<u32>,
    rpm_limit: Option<u32>,
    lanes: usize,
) -> Router {
    let mut config = Config::default();
    let mut entry = ProviderEntry::anthropic_api("literal:k");
    if let ProviderEntry::AnthropicApi {
        base_url: b,
        runtime,
        ..
    } = &mut entry
    {
        *b = base_url.to_string();
        runtime.circuit_failures = circuit_failures;
        runtime.rpm_limit = rpm_limit;
    }
    config.providers.insert("p1".to_string(), entry);
    // `m1` first so a single-lane fixture keeps the nickname every existing
    // test names, and `m0..` are added for the multi-lane fixture on top.
    let nicknames: Vec<String> = if lanes <= 1 {
        vec!["m1".to_string()]
    } else {
        (0..lanes).map(|n| format!("m{n}")).collect()
    };
    for nickname in &nicknames {
        config
            .models
            .insert(nickname.clone(), ModelEntry::new("p1", "claude-sonnet-4-5"));
    }
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(nicknames[0].clone()),
    );
    let mut router = Router::new(Arc::new(config));
    let mut models = std::collections::BTreeMap::new();
    for nickname in &nicknames {
        models.insert(
            nickname.clone(),
            Arc::new(ResolvedModel::new(
                nickname,
                "p1",
                Arc::clone(&provider),
                "claude-sonnet-4-5",
            )),
        );
    }
    router.install_resolved_models(models);
    router
}

/// A remote router whose MODEL carries an operator `anthropic-beta` floor, the
/// way `[models.X] header_extras` supplies one in production.
///
/// The dispatch overlay composes `routectl_internal.operator_betas` per target
/// from the provider and model `header_extras`, so this is the only faithful
/// way to give a request an operator beta: a value planted on the ingress
/// request is overwritten by that recomposition.
pub(super) fn remote_router_with_model_beta(provider: Arc<dyn Provider>, beta: &str) -> Router {
    let mut config = Config::default();
    let mut entry = ProviderEntry::anthropic_api("literal:k");
    if let ProviderEntry::AnthropicApi {
        base_url: b,
        runtime,
        ..
    } = &mut entry
    {
        *b = "https://api.anthropic.com".to_string();
        runtime.circuit_failures = Some(1);
    }
    config.providers.insert("p1".to_string(), entry);
    let mut extras = std::collections::BTreeMap::new();
    extras.insert("anthropic-beta".to_string(), beta.to_string());
    config.models.insert(
        "m1".to_string(),
        ModelEntry::new("p1", "claude-sonnet-4-5").with_header_extras(extras),
    );
    config
        .aliases
        .insert("default".to_string(), AliasValue::Single("m1".to_string()));
    let mut router = Router::new(Arc::new(config));
    let mut models = std::collections::BTreeMap::new();
    // On the RESOLVED model, which is what the dispatch overlay reads when it
    // composes `operator_betas` -- the config entry alone never reaches it.
    let mut resolved_extras = std::collections::BTreeMap::new();
    resolved_extras.insert("anthropic-beta".to_string(), beta.to_string());
    models.insert(
        "m1".to_string(),
        Arc::new(
            ResolvedModel::new("m1", "p1", provider, "claude-sonnet-4-5")
                .with_header_extras(resolved_extras),
        ),
    );
    router.install_resolved_models(models);
    router
}

/// A remote router with `lanes` resolvable nicknames AND a non-zero
/// paid-probe daily cap, for the candidate-capacity test: both halves are
/// needed, since a zero cap records no candidate at all and an unresolvable
/// lane never exhausts its plan.
pub(super) fn remote_router_with_lanes_and_paid_cap(
    provider: Arc<dyn Provider>,
    lanes: usize,
    cap: u32,
) -> Router {
    let mut router = remote_router_with_lanes(provider, lanes);
    let mut config = (*router.config).clone();
    config
        .fidelity
        .paid_probe_daily_caps
        .insert("p1".to_string(), cap);
    router.config = Arc::new(config);
    router
}

/// A remote router whose provider carries a non-zero paid-probe daily cap.
/// Without the opt-in the cap defaults to zero and no candidate is ever
/// recorded, which is the fail-closed default rather than a defect --
/// `a_zero_cap_router_records_no_candidate` pins that direction.
pub(super) fn remote_router_with_paid_cap(provider: Arc<dyn Provider>, cap: u32) -> Router {
    with_paid_cap(router_on_base("https://api.anthropic.com", provider), cap)
}

/// `remote_router_with_model_beta` plus a non-zero paid-probe daily cap.
///
/// Both halves are needed by the candidate-retention test: the beta floor gives
/// the captured payload a non-default OPERATOR source to retain (an all-empty
/// payload would compare equal to a freshly built one whether retention worked
/// or not), and the cap is what lets an exhausted plan record a candidate at
/// all.
pub(super) fn remote_router_with_model_beta_and_paid_cap(
    provider: Arc<dyn Provider>,
    beta: &str,
    cap: u32,
) -> Router {
    with_paid_cap(remote_router_with_model_beta(provider, beta), cap)
}

/// Install `cap` as `p1`'s paid-probe daily cap on an already-built router.
///
/// One writer for the fidelity knob, so the three fixtures above cannot drift
/// on which provider key the cap lands under.
fn with_paid_cap(mut router: Router, cap: u32) -> Router {
    let mut config = (*router.config).clone();
    config
        .fidelity
        .paid_probe_daily_caps
        .insert("p1".to_string(), cap);
    router.config = Arc::new(config);
    router
}
