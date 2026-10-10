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

/// The closed-table path a free token count validates -- the only kind of row
/// a re-check probe is ever built from.
pub(super) const RECHECK_PATH: &str = super::field_repair::TEST_RECHECK_PATH;

/// The value [`grounding_request`] carries on the re-check row. One of the
/// modeled probe values, so the payload bound admits it.
pub(super) const RECHECK_VALUE: &str = "summarized";

/// The upstream every fixture router below gives the model `nickname`.
///
/// `m1` -- the only model of a single-lane fixture -- sends the real wire id;
/// every other nickname gets its own, because a learned lane is
/// `(provider entry, upstream)` and the fixtures share one provider entry, so
/// identical upstreams would fold every nickname onto ONE identity.
pub(super) fn fixture_upstream(nickname: &str) -> String {
    if nickname == "m1" {
        "claude-sonnet-4-5".to_string()
    } else {
        format!("claude-sonnet-4-5-{nickname}")
    }
}

/// The learned lane a seat-chain fixture installs for `mN`: provider entry
/// `pN` sending `wire-N`, the shape the field-repair, pre-flight and canary
/// fixtures build. Any other nickname is its own entry over `wire-<nickname>`,
/// which no fixture dispatches.
pub(super) fn seat_chain_lane(nickname: &str) -> crate::state_key::StateKey {
    let lane = nickname
        .strip_prefix('m')
        .filter(|n| n.parse::<usize>().is_ok())
        .map_or_else(
            || crate::state_key::StateKey::new(nickname, &format!("wire-{nickname}")),
            |n| crate::state_key::StateKey::new(&format!("p{n}"), &format!("wire-{n}")),
        );
    lane.unwrap_or_else(|| {
        crate::state_key::StateKey::new("separator-free", &format!("wire-{nickname}"))
            .expect("a separator-free entry mints a lane")
    })
}

/// The learned lane a fixture router dispatches the model `nickname` on. A
/// nickname the fixture never installed names a lane no model resolves.
pub(super) fn lane(nickname: &str) -> crate::state_key::StateKey {
    crate::state_key::StateKey::new("p1", &fixture_upstream(nickname))
        .expect("the fixture provider entry carries no separator")
}

/// A request carrying the count-validated closed-table row. Without it the
/// activation seam has no field to re-check and every test below would pass
/// vacuously, so `a_request_grounding_nothing_activates_no_lane` pins the
/// converse.
pub(super) fn grounding_request() -> ChatRequest {
    with_recheck_field(ChatRequest::default(), RECHECK_VALUE)
}

/// `req` carrying `value` on the count-validated row.
pub(super) fn with_recheck_field(mut req: ChatRequest, value: &str) -> ChatRequest {
    req.chat_template_kwargs = Some(serde_json::json!({
        super::field_repair::TEST_RECHECK_KWARG: value,
    }));
    req
}

/// The value `req` carries on the count-validated row, if any.
pub(super) fn recheck_value(req: &ChatRequest) -> Option<&str> {
    req.chat_template_kwargs
        .as_ref()
        .and_then(|kwargs| kwargs.get(super::field_repair::TEST_RECHECK_KWARG))
        .and_then(serde_json::Value::as_str)
}

/// Plant a resident ACTING field negative on `path` for `lane`, keyed under
/// `provider_kind` -- the state a re-check probe exists to revisit.
///
/// Through the registry's own carry-over import, so the planted row is one a
/// ledger replay can produce. No confirmation is seeded: the verdict acts, but
/// the pre-flight planner does not apply it, so the dispatch path is
/// unchanged. Asserts its own premise, since a fixture that silently planted
/// nothing would make every "a re-check queues" test pass for the wrong reason.
pub(super) fn plant_acting_negative(
    router: &Router,
    lane: &crate::state_key::StateKey,
    path: &str,
    provider_kind: &str,
) {
    let stamped = std::time::Instant::now();
    let feature_key = crate::field_capability::field_capability_key(path)
        .expect("the fixture path is a well-formed qualified path");
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            provider_kind: provider_kind.to_string(),
            state_key: lane.as_lane_key().to_string(),
            feature_key,
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
    let key =
        FieldVerdictKey::new(lane, path, provider_kind).expect("a qualified path mints a key");
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&key, std::time::Instant::now()),
        "fixture premise: the planted negative must be acting",
    );
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
        let carries_field = recheck_value(&req).is_some();
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
pub(super) fn plant_eligible_verdict(router: &Router, nickname: &str, path: &str) {
    let lane = lane(nickname);
    let state_key = lane.as_lane_key();
    plant_acting_negative(router, &lane, path, "anthropic-api");
    let feature_key = crate::field_capability::field_capability_key(path)
        .expect("the fixture path is a well-formed qualified path");
    let key =
        FieldVerdictKey::new(&lane, path, "anthropic-api").expect("a qualified path mints a key");
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
        "fixture premise: {nickname} must be pre-flight eligible",
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
/// `an_exhausted_free_plan_tombstones_and_dials_nothing_further`) read
/// `free_exhausted_total`, which an abandonment leaves at zero. `remote_router_with_lanes` installs one resolved model per lane.
pub(super) async fn settle_terminally(router: &Router, count: usize) {
    for n in 0..count {
        let key = FieldVerdictKey::new(&lane(&format!("m{n}")), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
        router.run_due_probes().await;
    }
}

/// Build a single-leg router on `base_url` with `provider` installed
/// under nickname `m1`, alias `default`, whose lane holds an acting negative on
/// the re-check row.
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

/// [`remote_router`] with NOTHING learned on its lane, so an admitted request
/// has no negative to re-check.
pub(super) fn remote_router_without_negative(provider: Arc<dyn Provider>) -> Router {
    build_router_learning(
        "https://api.anthropic.com",
        provider,
        Some(1),
        None,
        1,
        false,
    )
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

/// The router builder every fixture above composes: one anthropic-api provider
/// entry `p1` on `base_url`, `lanes` models `m0..m{lanes-1}` all resolved to
/// `provider`, `default` aliased to the first, and every lane holding an acting
/// negative on the re-check row -- the only state a probe ever activates on.
fn build_router(
    base_url: &str,
    provider: Arc<dyn Provider>,
    circuit_failures: Option<u32>,
    rpm_limit: Option<u32>,
    lanes: usize,
) -> Router {
    build_router_learning(base_url, provider, circuit_failures, rpm_limit, lanes, true)
}

/// [`build_router`], planting the re-check negatives only when `with_negative`.
fn build_router_learning(
    base_url: &str,
    provider: Arc<dyn Provider>,
    circuit_failures: Option<u32>,
    rpm_limit: Option<u32>,
    lanes: usize,
    with_negative: bool,
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
        config.models.insert(
            nickname.clone(),
            ModelEntry::new("p1", fixture_upstream(nickname)),
        );
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
                fixture_upstream(nickname),
            )),
        );
    }
    router.install_resolved_models(models);
    if with_negative {
        for nickname in &nicknames {
            plant_acting_negative(&router, &lane(nickname), RECHECK_PATH, "anthropic-api");
        }
    }
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
    plant_acting_negative(&router, &lane("m1"), RECHECK_PATH, "anthropic-api");
    router
}
