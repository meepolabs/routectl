//! End-to-end coverage for the learn-event capture point
//! (`observe_for_learning`): the full eligibility gate, per-request
//! dedupe, registry wiring, structured WARN, and the
//! `DispatchMeta.learned_capabilities` ride-along -- driven through the
//! REAL dispatch error arms (complete + stream), so the classifier,
//! matcher, guardrail, and registry all participate.

use super::*;
use routectl_core::ForwardedBearer;
use routectl_core::ToolDef;
use routectl_core::capability::SignalTier;
use routectl_testkit::{CapturedEvent, with_capture};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A provider whose complete/stream always fail with a fixed upstream
/// status plus optional type/code, so a test drives a precise
/// classifier outcome (a self-identifying token or an inferred body).
/// Counts calls so a test can prove a same-provider retry did fire.
struct CapabilityRejectingProvider {
    id: &'static str,
    status: u16,
    body: String,
    upstream_type: Option<String>,
    upstream_code: Option<String>,
    calls: AtomicUsize,
}

impl CapabilityRejectingProvider {
    fn err(&self) -> Error {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Error::upstream_full(
            self.id,
            self.status,
            self.body.clone(),
            None,
            self.upstream_type.clone(),
            self.upstream_code.clone(),
        )
    }
}

#[async_trait::async_trait]
impl Provider for CapabilityRejectingProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.err())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.err())
    }
}

/// Self-identifying openai-compat rejection: a 400 whose `error.code`
/// is `unsupported_parameter` (the classifier lifts it to
/// `FeatureUnsupported`) and whose `/error/param` names the real
/// capability `web_search` -- the field the shared resolver reads.
fn self_identifying_provider() -> Arc<CapabilityRejectingProvider> {
    Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search","message":"Unsupported parameter."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
}

/// A self-identifying openai-compat 400 whose `/error/param` resolves to
/// `web_search` once the resolver trims it, but whose RAW field is an
/// oversized, control-char-laden blob (`web_search` followed by 80
/// newlines). Models a buggy or adversarial upstream: the closed-set
/// resolver still attributes the capability, yet the raw param must never
/// reach the operator log verbatim.
fn oversized_param_provider() -> Arc<CapabilityRejectingProvider> {
    let param = format!("web_search{}", "\n".repeat(80));
    let body = json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": param,
            "message": "Unsupported parameter."
        }
    })
    .to_string();
    Arc::new(CapabilityRejectingProvider {
        id: "p1",
        status: 400,
        body,
        upstream_type: Some("invalid_request_error".into()),
        upstream_code: Some("unsupported_parameter".into()),
        calls: AtomicUsize::new(0),
    })
}

/// A self-identifying openai-compat 400 whose `error.code` lifts to
/// `FeatureUnsupported` but whose body carries NO `/error/param`: the
/// resolver can attribute no capability, so nothing is learned.
fn paramless_unsupported_provider() -> Arc<CapabilityRejectingProvider> {
    Arc::new(CapabilityRejectingProvider {
        id: "p1",
        status: 400,
        body: "{}".into(),
        upstream_type: Some("invalid_request_error".into()),
        upstream_code: Some("unsupported_parameter".into()),
        calls: AtomicUsize::new(0),
    })
}

/// A self-identifying openai-compat 400 whose `/error/param` canonicalizes
/// to the well-known tool-type key `web_search` -- but the triggering
/// request carries NO web_search tool. Models a misbehaving or compromised
/// upstream naming an off-request capability.
fn poisoned_param_provider() -> Arc<CapabilityRejectingProvider> {
    Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search_20250305","message":"Unsupported parameter."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: Some("unsupported_parameter".into()),
            calls: AtomicUsize::new(0),
        })
}

/// An anthropic-api 400 carrying the verbatim prefill-unsupported phrase
/// in free-text `error.message` -- a generic BadRequest the resolver's
/// inferred arm maps to `prefill`.
fn prefill_inferred_provider() -> Arc<CapabilityRejectingProvider> {
    Arc::new(CapabilityRejectingProvider {
            id: "p1",
            status: 400,
            body: r#"{"type":"error","error":{"type":"invalid_request_error","message":"Prefilling assistant messages is not supported for this model."}}"#.into(),
            upstream_type: Some("invalid_request_error".into()),
            upstream_code: None,
            calls: AtomicUsize::new(0),
        })
}

fn router_with(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// A minimal openai-compat provider config (capability subsystem left
/// at its default: enabled).
const OPENAI_P1: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

/// A minimal anthropic-api provider config (capability subsystem left at
/// its default: enabled). Serves the inferred-arm dormancy test.
const ANTHROPIC_P1: &str = r#"
[providers.p1]
kind = "anthropic-api"
"#;

fn req_with_tool(tool_type: &str) -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![],
        tools: Some(vec![ToolDef::Other(json!({ "type": tool_type }))]),
        ..Default::default()
    }
}

fn learn_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == "learned-capability negative observed")
        .collect()
}

#[tokio::test]
async fn eligible_self_identifying_records_warns_and_populates_meta() {
    // Arrange: a self-identifying 400 whose capability token is also
    // the request's derived feature -> the guardrail admits it.
    let router = router_with(OPENAI_P1, self_identifying_provider());

    // Act
    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    // Assert: the request still fails (learning never changes the
    // per-request outcome), but the meta ride-along carries the event.
    assert!(dispatched.result.is_err());
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    let ev = &dispatched.meta.learned_capabilities[0];
    assert_eq!(ev.state_key, "m1");
    assert_eq!(ev.capability_key, "web_search");
    assert_eq!(ev.provider_kind, "openai-compat");
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.observations, 1);
    assert_eq!(ev.upstream_status, 400);
    assert!(!ev.remapped);
    assert_eq!(ev.request_features, vec!["web_search".to_string()]);

    // The structured WARN carries only the safe fields.
    let warns = learn_warns(&events);
    assert_eq!(warns.len(), 1);
    let warn = warns[0];
    assert_eq!(warn.field("event"), Some("learn"));
    assert_eq!(warn.field("state_key"), Some("m1"));
    assert_eq!(warn.field("capability_key"), Some("web_search"));
    assert_eq!(warn.field("provider_kind"), Some("openai-compat"));
    assert_eq!(warn.field("upstream_status"), Some("400"));
    assert_eq!(warn.field("upstream_code"), Some("unsupported_parameter"));
    assert_eq!(warn.field("upstream_param"), Some("web_search"));
    assert_eq!(warn.field("signal_tier"), Some("self-identifying"));
    assert_eq!(warn.field("observations"), Some("1"));
    assert_eq!(warn.field("acting"), Some("true"));
    assert_eq!(warn.field("body"), None, "no body/message/prompt fields");
    assert_eq!(warn.field("message"), None);

    // The registry now holds an acting negative for the target.
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
    );
}

#[tokio::test]
async fn oversized_upstream_param_is_dropped_from_the_learn_warn() {
    // Arrange: a self-identifying 400 whose raw `/error/param` is oversized
    // and control-char-laden but trims to the canonical `web_search` key.
    let router = router_with(OPENAI_P1, oversized_param_provider());

    // Act
    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    // Assert: the capability is still learned (the resolver trims the raw
    // field to a closed-set key), so the safe fields log unchanged.
    assert!(dispatched.result.is_err());
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    assert_eq!(
        dispatched.meta.learned_capabilities[0].capability_key,
        "web_search"
    );
    let warns = learn_warns(&events);
    assert_eq!(warns.len(), 1);
    let warn = warns[0];
    assert_eq!(warn.field("capability_key"), Some("web_search"));
    assert_eq!(warn.field("upstream_code"), Some("unsupported_parameter"));
    // The unbounded, control-char-laden raw param is dropped entirely --
    // the field is absent, not blank, so no injected text reaches the log.
    assert_eq!(warn.field("upstream_param"), None);
}

#[tokio::test]
async fn eligible_self_identifying_captures_on_the_stream_arm_too() {
    // The stream error arm is wired identically to the complete arm.
    let router = router_with(OPENAI_P1, self_identifying_provider());

    let (dispatched, events) = with_capture(
        router.stream_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.result.is_err());
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    assert_eq!(learn_warns(&events).len(), 1);
}

#[tokio::test]
async fn same_request_retry_dedupes_to_one_observation() {
    // Arrange: a non-remapping operator overlay raises the
    // feature-unsupported same-provider retry cap so the ONE request
    // hits the error arm more than once against the SAME (state_key,
    // feature). Per-request dedupe must still count exactly one.
    let toml = r#"
[retry]
max_attempts = 3

[retry.classes.feature-unsupported]
retry = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
    let provider = self_identifying_provider();
    let router = router_with(toml, provider.clone());

    // Act
    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    // Assert: the same provider WAS retried (two dispatches), so the
    // error arm ran twice -- yet only one observation, one WARN, and
    // one meta event survived the dedupe.
    assert!(dispatched.result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "the raised retry cap must drive a same-provider retry",
    );
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    assert_eq!(
        dispatched.meta.learned_capabilities[0].observations, 1,
        "a same-request retry must not manufacture a second observation",
    );
    assert_eq!(learn_warns(&events).len(), 1);
}

#[tokio::test]
async fn kill_switch_off_skips_the_learn_path() {
    let toml = r#"
[capability]
enabled = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
    let router = router_with(toml, self_identifying_provider());

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
    assert!(router.learned_capabilities.is_empty());
}

/// The suppression WARN a masked-cell rejection emits.
const MASK_SUPPRESSION_MSG: &str =
    "force_supported override contradicted: masked capability still rejected upstream";

fn suppression_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == MASK_SUPPRESSION_MSG)
        .collect()
}

#[tokio::test]
async fn masked_reject_emits_suppression_warn_and_counter_and_skips_learn() {
    // A force_supported override masks `web_search` on m1: the
    // mask lets the target dispatch (act side short-circuits to Allow),
    // upstream still rejects it, and the learn side suppresses the observe
    // -- one suppression WARN + one counter, no learned negative, no learn
    // event.
    let toml = format!(
        "{OPENAI_P1}\n\
             [capability.overrides.\"p1:m1\"]\n\
             force_supported = [\"web_search\"]\n"
    );
    let router = router_with(&toml, self_identifying_provider());

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    // The request still fails; nothing was learned and the ordinary learn
    // WARN / meta event never fired.
    assert!(dispatched.result.is_err());
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
    assert!(
        router.learned_capabilities.is_empty(),
        "a masked cell must never create a learned negative",
    );

    // Exactly one suppression WARN, carrying only the safe fields.
    let supp = suppression_warns(&events);
    assert_eq!(supp.len(), 1);
    assert_eq!(supp[0].field("event"), Some("suppression"));
    assert_eq!(supp[0].field("state_key"), Some("m1"));
    assert_eq!(supp[0].field("capability_key"), Some("web_search"));
    assert_eq!(supp[0].field("body"), None, "no body/message/prompt fields");
    assert_eq!(supp[0].field("message"), None);

    // ...and exactly one dedicated counter increment.
    assert_eq!(router.metrics.mask_suppressed_total(), 1);
}

#[tokio::test]
async fn masked_cell_rejection_does_not_refresh_resident_entry() {
    // With an ALREADY-resident learned negative, a masked-cell rejection
    // must neither refresh (expires_at) nor increment (observations) the
    // entry -- its wall-clock decay continues on the original clock.
    let toml = format!(
        "{OPENAI_P1}\n\
             [capability.overrides.\"p1:m1\"]\n\
             force_supported = [\"web_search\"]\n"
    );
    let router = router_with(&toml, self_identifying_provider());

    // Plant a resident acting negative at a fixed instant.
    let t0 = Instant::now();
    router.learned_capabilities.observe(
        "m1",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        t0,
    );
    let before = router.learned_capabilities.snapshot();
    assert_eq!(before.len(), 1);

    let (dispatched, _events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    // No learn event, and the resident entry is untouched: neither
    // incremented nor refreshed by the masked rejection.
    assert!(dispatched.meta.learned_capabilities.is_empty());
    let after = router.learned_capabilities.snapshot();
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].observations, before[0].observations,
        "masked rejection must not increment observations",
    );
    assert_eq!(
        after[0].expires_at, before[0].expires_at,
        "masked rejection must not refresh the decay clock",
    );
    assert_eq!(router.metrics.mask_suppressed_total(), 1);
}

#[tokio::test]
async fn non_request_fault_status_is_never_learned() {
    // A 500 is ServerError, not a 400/422 request fault: the status
    // gate rejects it before the matcher ever runs.
    let provider = Arc::new(CapabilityRejectingProvider {
        id: "p1",
        status: 500,
        body: "{}".into(),
        upstream_type: None,
        upstream_code: None,
        calls: AtomicUsize::new(0),
    });
    let router = router_with(OPENAI_P1, provider);

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
}

#[tokio::test]
async fn unresolvable_openai_rejection_is_not_learned() {
    // A self-identifying 400 whose body carries NO `/error/param`: the
    // resolver can attribute no canonical capability, so nothing is
    // learned. (The old cross-namespace gate is gone; the resolver's
    // no-learn-on-unresolvable is the replacement guardrail.)
    let router = router_with(OPENAI_P1, paramless_unsupported_provider());

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
}

#[tokio::test]
async fn off_request_param_is_not_learned() {
    // Poisoning guard: the upstream names `web_search` in `/error/param`
    // (the resolver canonicalizes the dated variant to the well-known
    // key), but the triggering request carried only `computer_use` -- so
    // `web_search` is NOT in the request's derived feature set. The
    // capture membership gate blocks the learn: a misbehaving upstream
    // cannot teach a capability the request never sent, and no registry
    // entry is planted.
    let router = router_with(OPENAI_P1, poisoned_param_provider());

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("computer_use"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.result.is_err());
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "an off-request param must never produce a learn event",
    );
    assert!(learn_warns(&events).is_empty());
    assert!(
        router.learned_capabilities.is_empty(),
        "an off-request param must never create a registry entry",
    );
}

#[tokio::test]
async fn inferred_prefill_is_dormant_and_not_learned() {
    // The inferred arm resolves the anthropic prefill phrase to `prefill`,
    // but `derive_feature_keys` never produces that key, so a request's
    // derived feature set can never contain it. The capture membership
    // gate blocks the learn end-to-end: the inferred table ships dormant
    // until an act-side derivation for `prefill` exists.
    let router = router_with(ANTHROPIC_P1, prefill_inferred_provider());

    // A prefill request carries no built-in tool / output_config, so its
    // derived feature set is empty.
    let req = ChatRequest {
        model: "m1".into(),
        messages: vec![],
        ..Default::default()
    };

    let (dispatched, events) =
        with_capture(router.complete_with_options(req, RouterOptions::default())).await;

    assert!(dispatched.result.is_err());
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "inferred prefill is dormant: nothing derives it act-side, so it must not learn",
    );
    assert!(learn_warns(&events).is_empty());
    assert!(router.learned_capabilities.is_empty());
}

#[tokio::test]
async fn forwarded_request_is_not_learned() {
    // A request carrying a forwarded client bearer never contributes a
    // learned negative (the forwarded token owns its own retry/backoff).
    let router = router_with(OPENAI_P1, self_identifying_provider());
    let mut req = req_with_tool("web_search");
    req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("t".into()));

    let (dispatched, events) =
        with_capture(router.complete_with_options(req, RouterOptions::default())).await;

    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
}

#[tokio::test]
async fn operator_remapped_class_is_not_learned() {
    // The operator remaps 400 to feature-unsupported: the class is now
    // config-sourced (remapped == true, capability == the operator-remap
    // token), so the learn path skips it -- a synthesized class is not
    // an upstream self-report.
    let toml = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
    let router = router_with(toml, self_identifying_provider());

    let (dispatched, events) = with_capture(
        router.complete_with_options(req_with_tool("web_search"), RouterOptions::default()),
    )
    .await;

    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(learn_warns(&events).is_empty());
}

/// A provider whose `complete` always succeeds with a minimal response,
/// so a re-probe dispatched to it settles as a success.
struct SuccessProvider {
    id: &'static str,
}

#[async_trait::async_trait]
impl Provider for SuccessProvider {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: req.model,
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(self.id, 500, "unused"))
    }
}

/// A generic 400 that names NO capability (openai-compat has no inferred
/// matcher), so the matcher yields `None` and a re-probe against it settles
/// as an OtherError rather than a same-capability rejection.
fn other_error_provider() -> Arc<CapabilityRejectingProvider> {
    Arc::new(CapabilityRejectingProvider {
        id: "p1",
        status: 400,
        body: "{}".into(),
        upstream_type: None,
        upstream_code: None,
        calls: AtomicUsize::new(0),
    })
}

/// Seed an already-expired, still-acting self-identifying negative so the
/// next dispatch's filter claims the single re-probe slot. A past
/// `expires_at` (rather than a zero decay) keeps the registry's real decay
/// intact, so a same-capability settle can be observed backing off.
fn seed_expired_negative(router: &Router, state_key: &str, feature: &str) {
    let past = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("test clock is well past boot");
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.into(),
            feature_key: feature.into(),
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: past,
            last_seen: past,
            expires_at: past,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
}

#[tokio::test]
async fn probe_success_clears_the_learned_negative() {
    // Arrange: an expired negative for the very feature the request asks
    // for; the target's provider then succeeds on the admitted re-probe.
    let router = router_with(OPENAI_P1, Arc::new(SuccessProvider { id: "p1" }));
    seed_expired_negative(&router, "m1", "web_search");

    // Act
    let dispatched = router
        .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
        .await;

    // Assert: the probe was admitted, the 2xx cleared the entry, and a
    // subsequent lookup now allows the target.
    assert!(dispatched.result.is_ok());
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert!(router.learned_capabilities.is_empty());
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::Allow,
    );
}

#[tokio::test]
async fn probe_same_capability_rejection_refreshes_with_backoff() {
    // Arrange: an expired negative; the probe target re-rejects the SAME
    // capability (self-identifying 400).
    let router = router_with(OPENAI_P1, self_identifying_provider());
    seed_expired_negative(&router, "m1", "web_search");
    let before = router.learned_capabilities.snapshot()[0].expires_at;

    // Act
    let dispatched = router
        .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
        .await;

    // Assert: the request still fails (a probe is a real user request),
    // the probe failure was counted, and the entry re-acts on a fresh,
    // later window with a bumped observation -- in_flight released.
    assert!(dispatched.result.is_err());
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.probe_failures_total(), 1);
    // A probe re-rejection settles the probe; it is not a fresh learn
    // event, so nothing rides the meta ledger channel.
    assert!(dispatched.meta.learned_capabilities.is_empty());

    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].observations, 2);
    assert!(
        snap[0].expires_at > before,
        "same-capability rejection must push expiry into the future with backoff",
    );
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
        "the refreshed negative is non-expired and in_flight released -> route away",
    );
}

#[tokio::test]
async fn probe_other_error_releases_slot_and_re_probes_next_request() {
    // Arrange: an expired negative; the probe target fails with an error
    // that is NOT the same-capability rejection.
    let router = router_with(OPENAI_P1, other_error_provider());
    seed_expired_negative(&router, "m1", "web_search");

    // Act
    let dispatched = router
        .complete_with_options(req_with_tool("web_search"), RouterOptions::default())
        .await;

    // Assert: the probe was admitted but a transient must NOT clear a valid
    // negative or count as a same-capability failure. The entry survives
    // unchanged and expired, so the NEXT request re-probes -- the
    // repeat-re-probe property broken before this wiring.
    assert!(dispatched.result.is_err());
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.probe_failures_total(), 0);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(
        snap[0].observations, 1,
        "OtherError leaves observations untouched"
    );
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::ProbeAdmitted,
        "in_flight released + still expired -> the next request admits a NEW probe",
    );
}

#[tokio::test]
async fn stream_probe_same_capability_rejection_settles_on_the_stream_arm() {
    // The stream loop wires the settle guard identically to the complete
    // loop: a pre-first-chunk same-capability rejection settles the probe.
    let router = router_with(OPENAI_P1, self_identifying_provider());
    seed_expired_negative(&router, "m1", "web_search");
    let before = router.learned_capabilities.snapshot()[0].expires_at;

    let dispatched = router
        .stream_with_options(req_with_tool("web_search"), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_err());
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.probe_failures_total(), 1);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].observations, 2);
    assert!(
        snap[0].expires_at > before,
        "same-capability rejection must push expiry into the future with backoff",
    );
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::RouteAway(SignalTier::SelfIdentifying),
    );
}

#[tokio::test]
async fn count_tokens_releases_admitted_probe_without_latching() {
    // A probe admitted while filtering a count_tokens request is released
    // (OtherError): the token-count path is not a messages-capability test,
    // so the entry must not latch in_flight.
    let router = router_with(OPENAI_P1, self_identifying_provider());
    seed_expired_negative(&router, "m1", "web_search");

    // openai-compat cannot count_tokens, so the walk terminates without
    // touching the provider -- but the filter still admitted the probe.
    let result = router.count_tokens(req_with_tool("web_search")).await;

    assert!(matches!(result, Err(Error::NotImplemented(..))));
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.probe_failures_total(), 0);
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            "web_search",
            "openai-compat",
            Instant::now(),
        ),
        crate::learned_capability::RoutingDecision::ProbeAdmitted,
        "in_flight released -> the next request re-probes rather than latching",
    );
}
