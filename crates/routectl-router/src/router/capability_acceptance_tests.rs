//! Consolidated acceptance scenarios for the response-evidence subsystem,
//! exercising the success-arm observer, the coexistence registry, and the
//! feature filter TOGETHER as one Router. Each scenario is an independent
//! AAA test:
//!
//! - A: a schema-conforming strict response admits a VerifiedWorking cell
//!   through a real dispatch, and routing stays Allow.
//! - B: a forced-search response with no search evidence, observed twice,
//!   acts as an F3+Live suspect while routing STILL allows the target --
//!   the advisory negative never demotes it.
//! - C: a fresh self-identifying negative replaces a resident
//!   VerifiedWorking positive (the settled recency rule).
//! - D: a resident VerifiedWorking positive masks a `Some(false)` catalog
//!   prior, lifting its target above a prior-demoted sibling.
//! - E (R4): length-truncated, refused, and content-filtered successes each
//!   produce no verdict end to end.
//! - F (R2): identical observation sequences plus identical `now` values
//!   yield identical registry state -- the stage-two admission determinism
//!   a later warm-rebuild equivalence test extends.
//!
//! Sibling sidecars pin the per-slice behavior; this module asserts the
//! whole observer -> registry -> filter path.

use super::*;

use routectl_core::capability::{
    EvidenceSource, FailurePhase, STRUCTURED_OUTPUT, SignalTier, Verdict, WEB_SEARCH,
};
use routectl_core::{Choice, Message, MessageContent, Role, ToolDef, Usage};
use serde_json::json;

use crate::capability_detect::ObservationDirection;
use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::Config;
use crate::learned_capability::{LearnedRegistryEntry, ObserveOutcome, RoutingDecision};
use crate::resolved::ResolvedModel;

/// The stable provider-kind these scenarios key observations, priors, and
/// routing on. Matches the `[providers.p1]` kind in [`router`].
const KIND: &str = "openai-compat";

// --- provider stubs ----------------------------------------------------

/// A provider whose non-streaming success arm returns a canned assembled
/// response, so a real dispatch reaches the response-evidence observer.
struct CannedProvider {
    resp: ChatResponse,
}

impl CannedProvider {
    fn arc(resp: ChatResponse) -> Arc<Self> {
        Arc::new(Self { resp })
    }
}

#[async_trait::async_trait]
impl Provider for CannedProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Ok(self.resp.clone())
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(self.resp.clone())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p1", 500, "unused"))
    }
}

/// A provider used only to expand a dispatch target for the observer /
/// filter seams; these scenarios drive those seams directly and never
/// dispatch through it, so none of its methods run.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("acceptance seam targets never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("acceptance seam targets never dispatch")
    }
}

// --- builders ----------------------------------------------------------

/// A router with the capability subsystem enabled and one `openai-compat`
/// provider `p1`. Registry tempo comes from the `[capability]` defaults, so
/// two routers built here share identical decay / inferred-window windows.
fn router() -> Router {
    let toml_text = r#"
version = 3
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
[capability]
enabled = true
"#;
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    Router::new(Arc::new(config))
}

/// [`router`] plus one installed model `m1` backed by `provider`, so a real
/// `complete_with_options` dispatch resolves and reaches the success arm.
fn router_with_provider(provider: Arc<dyn Provider>) -> Router {
    let mut router = router();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// A dispatch target on provider `p1` with `state_key = nickname`, carrying
/// the given catalog capability priors on its baked effective row.
/// `expand_chain_to_targets` fills the provider kind from config, so the
/// learned and prior passes both run.
fn target(router: &Router, nickname: &str, priors: &[(&str, bool)]) -> DispatchTarget {
    let mut row = CatalogRow::sentinel();
    for (key, value) in priors {
        row.capabilities.insert((*key).to_string(), *value);
    }
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = ResolvedModel::new(nickname, "p1", provider, "upstream").with_effective_row(
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "seed".to_string(),
        },
    );
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// An assistant message that refuses -- a populated `refusal` field the
/// clean-stop gate rejects regardless of the finish reason.
fn refusal_message() -> Message {
    Message {
        refusal: Some("cannot help with that".into()),
        ..assistant_text("")
    }
}

/// A clean-stop response: exactly one choice, `finish_reason = "stop"`, no
/// refusal -- the shape the detectors run on.
fn clean_response(message: Message, usage: Option<Usage>) -> ChatResponse {
    response_with(message, "stop", usage)
}

/// A response carrying an explicit `finish_reason`, for the degraded-arm
/// fixtures (`length`, `content_filter`) the clean-stop gate must reject.
fn response_with(message: Message, finish_reason: &str, usage: Option<Usage>) -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some(finish_reason.into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage,
        ..Default::default()
    }
}

/// A request declaring an Anthropic structured-output format whose schema
/// carries the given top-level required keys.
fn structured_output_request(required: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": { "type": "object", "required": required }
                }
            }
        })),
        ..Default::default()
    }
}

/// A request forcing a web-search call (`tool_choice = any` over an offered
/// web-search tool) -- the only shape that yields a suspected-absence
/// observation when the response carries no search evidence.
fn forced_search_request() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({"type": "web_search_20250305"}))]),
        tool_choice: Some(json!({"type": "any"})),
        ..Default::default()
    }
}

/// A request offering web search (no force), so a positive
/// server-tool-use usage counter admits a VerifiedWorking positive.
fn web_search_request() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({"type": "web_search"}))]),
        ..Default::default()
    }
}

fn state_keys(chain: &[DispatchTarget]) -> Vec<&str> {
    chain.iter().map(|t| t.state_key.as_str()).collect()
}

// --- Scenario A: verified cell + routing Allow -------------------------

#[tokio::test]
async fn scenario_a_schema_conforming_success_admits_verified_and_routes_allow() {
    // Arrange: a strict request whose canned success carries a
    // schema-conforming JSON body -- a real dispatch that reaches the
    // success-arm observer.
    let resp = clean_response(assistant_text(r#"{"name":"ok"}"#), None);
    let router = router_with_provider(CannedProvider::arc(resp));

    // Act
    let dispatched = router
        .complete_with_options(
            structured_output_request(&["name"]),
            RouterOptions::default(),
        )
        .await;

    // Assert: the dispatch succeeded, a VerifiedWorking cell rode out and
    // landed in the registry, and routing the same capability still allows
    // the target.
    assert!(dispatched.result.is_ok(), "canned success dispatched");
    assert_eq!(dispatched.meta.capability_observations.len(), 1);
    let ev = &dispatched.meta.capability_observations[0];
    assert_eq!(ev.capability_key, STRUCTURED_OUTPUT);
    assert_eq!(ev.direction, ObservationDirection::Verified);
    assert_eq!(ev.source, EvidenceSource::Live);

    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].verdict, Verdict::VerifiedWorking);
    assert_eq!(
        router.learned_capabilities.acting_negative_for(
            "m1",
            STRUCTURED_OUTPUT,
            KIND,
            Instant::now()
        ),
        RoutingDecision::Allow,
        "a verified positive routes nothing",
    );
}

// --- Scenario B: F3+Live suspect acts but routing STILL allows ---------

#[test]
fn scenario_b_forced_search_absent_acts_f3_yet_routing_stays_allow() {
    // Arrange: a forced-search request whose clean response carries no search
    // evidence, observed on target m1.
    let router = router();
    let t = target(&router, "m1", &[]);
    let req = forced_search_request();
    let resp = clean_response(assistant_text("no search happened"), None);
    let now = Instant::now();

    // Act: a first inferred observation is pending; a second within the window
    // corroborates and acts.
    let mut meta1 = DispatchMeta::for_alias("m1");
    router.observe_capabilities(&req, &resp, &t, &mut meta1, now);
    assert!(
        meta1.capability_observations.is_empty(),
        "a first inferred observation must not act",
    );
    let mut meta2 = DispatchMeta::for_alias("m1");
    router.observe_capabilities(&req, &resp, &t, &mut meta2, now);

    // Assert: the corroborated F3 acted. The registry stores it as the raw
    // negative material -- verdict token `broken`, phase `f3`, source `live`
    // (SuspectIgnored is a presentation mapping, never a stored verdict).
    assert_eq!(meta2.capability_observations.len(), 1);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].verdict.as_str(), "broken");
    assert_eq!(snap[0].verdict.broken_phase(), Some(FailurePhase::F3));
    assert_eq!(snap[0].phase, FailurePhase::F3);
    assert_eq!(snap[0].source, EvidenceSource::Live);

    // Routing STILL allows the target: F3+Live is advisory-only.
    assert_eq!(
        router
            .learned_capabilities
            .acting_negative_for("m1", WEB_SEARCH, KIND, Instant::now()),
        RoutingDecision::Allow,
        "an F3+Live suspect routes nothing",
    );

    // And it never demotes: in a two-target chain m1 keeps its head slot.
    let out = router
        .filter_chain_by_features(
            vec![target(&router, "m1", &[]), target(&router, "m2", &[])],
            &[WEB_SEARCH.to_string()],
            "alias",
            &mut Vec::new(),
        )
        .expect("the advisory suspect never empties the chain");
    assert_eq!(
        state_keys(&out),
        vec!["m1", "m2"],
        "an F3+Live suspect is advisory-only and never demotes its target",
    );
}

// --- Scenario C: fresh self-identifying negative replaces verified -----

#[test]
fn scenario_c_self_identifying_negative_replaces_resident_verified() {
    // Arrange: a resident VerifiedWorking positive on m1 for structured output.
    let router = router();
    let t = target(&router, "m1", &[]);
    let now = Instant::now();
    let mut meta = DispatchMeta::for_alias("m1");
    router.observe_capabilities(
        &structured_output_request(&["name"]),
        &clean_response(assistant_text(r#"{"name":"ok"}"#), None),
        &t,
        &mut meta,
        now,
    );
    assert_eq!(
        router.learned_capabilities.snapshot()[0].verdict,
        Verdict::VerifiedWorking,
    );

    // Act: a fresh self-identifying negative arrives for the same key -- the
    // signal the error-arm learn path mints.
    let outcome = router.learned_capabilities.observe(
        "m1",
        STRUCTURED_OUTPUT,
        KIND,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        now,
    );

    // Assert: the negative replaced the positive and acts at once.
    assert_eq!(outcome, ObserveOutcome::Acting);
    let snap = router.learned_capabilities.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].verdict, Verdict::LearnedBroken(FailurePhase::F1));
    assert_eq!(snap[0].signal_tier, SignalTier::SelfIdentifying);
}

// --- Scenario D: verified masks a catalog prior ------------------------

#[test]
fn scenario_d_verified_positive_masks_catalog_prior() {
    // Arrange: a resident VerifiedWorking positive on m1 for web search, plus
    // a `Some(false)` catalog prior for that same capability.
    let router = router();
    let seed = target(&router, "m1", &[("web_search", false)]);
    let usage = Usage {
        server_tool_use: Some(json!({"web_search_requests": 1})),
        ..Default::default()
    };
    let mut meta = DispatchMeta::for_alias("m1");
    router.observe_capabilities(
        &web_search_request(),
        &clean_response(assistant_text("done"), Some(usage)),
        &seed,
        &mut meta,
        Instant::now(),
    );
    assert_eq!(
        router.learned_capabilities.snapshot()[0].verdict,
        Verdict::VerifiedWorking,
    );

    // Act: filter a chain whose UNVERIFIED prior-false lane m2 precedes the
    // verified-and-prior-false lane m1.
    let out = router
        .filter_chain_by_features(
            vec![
                target(&router, "m2", &[("web_search", false)]),
                target(&router, "m1", &[("web_search", false)]),
            ],
            &[WEB_SEARCH.to_string()],
            "alias",
            &mut Vec::new(),
        )
        .expect("the chain survives");

    // Assert: m1's verified positive masks its prior, so m1 keeps its
    // supported head slot while m2 (prior-false, unverified) demotes to the
    // prior tail -- the head/tail split proves the mask took effect.
    assert_eq!(
        state_keys(&out),
        vec!["m1", "m2"],
        "a resident verified positive masks m1's prior, lifting it above the prior-demoted m2",
    );
}

// --- Scenario E (R4): degraded successes produce no verdict ------------

#[test]
fn scenario_e_degraded_successes_produce_no_verdict() {
    // Arrange: three degraded shapes that WOULD verify on a clean stop --
    // length-truncated, refused, and content-filtered -- on a strict request.
    let router = router();
    let t = target(&router, "m1", &[]);
    let req = structured_output_request(&["name"]);
    let now = Instant::now();
    let degraded = [
        response_with(assistant_text(r#"{"name":"ok"}"#), "length", None),
        response_with(refusal_message(), "stop", None),
        response_with(assistant_text(r#"{"name":"ok"}"#), "content_filter", None),
    ];

    // Act
    for resp in &degraded {
        let mut meta = DispatchMeta::for_alias("m1");
        router.observe_capabilities(&req, resp, &t, &mut meta, now);
        assert!(
            meta.capability_observations.is_empty(),
            "a degraded response must ride nothing out",
        );
    }

    // Assert: no verdict end to end -- empty registry, no counters bumped.
    assert!(router.learned_capabilities.snapshot().is_empty());
    assert_eq!(router.metrics.verified_working_total(), 0);
    assert_eq!(router.metrics.f3_suspect_total(), 0);
}

// --- Scenario F (R2): identical sequence + now -> identical state ------

#[test]
fn scenario_f_identical_observations_and_now_yield_identical_registry() {
    // Arrange: the same observation sequence (a verified structured-output
    // positive, then a corroborated forced-search F3 suspect) driven into two
    // independent registries with ONE shared `now`.
    let now = Instant::now();
    let drive = |router: &Router| {
        let t = target(router, "m1", &[]);
        router.observe_capabilities(
            &structured_output_request(&["name"]),
            &clean_response(assistant_text(r#"{"name":"ok"}"#), None),
            &t,
            &mut DispatchMeta::for_alias("m1"),
            now,
        );
        let fs_req = forced_search_request();
        let fs_resp = clean_response(assistant_text("no search happened"), None);
        router.observe_capabilities(
            &fs_req,
            &fs_resp,
            &t,
            &mut DispatchMeta::for_alias("m1"),
            now,
        );
        router.observe_capabilities(
            &fs_req,
            &fs_resp,
            &t,
            &mut DispatchMeta::for_alias("m1"),
            now,
        );
    };

    // Act
    let r1 = router();
    drive(&r1);
    let r2 = router();
    drive(&r2);

    // Assert: stage-two admission is pure over its inputs plus `now`, so the
    // two snapshots are identical once ordered.
    fn order(e: &LearnedRegistryEntry) -> (String, String) {
        (e.state_key.clone(), e.feature_key.clone())
    }
    let mut s1 = r1.learned_capabilities.snapshot();
    let mut s2 = r2.learned_capabilities.snapshot();
    s1.sort_by_key(order);
    s2.sort_by_key(order);
    assert_eq!(s1.len(), 2, "one verified positive and one F3 suspect");
    assert_eq!(
        s1, s2,
        "identical observation sequences plus identical now yield identical registry state",
    );
}
