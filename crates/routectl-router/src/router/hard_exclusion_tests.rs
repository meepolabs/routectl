//! A capability every chain target refuses outright fails the request with
//! `CapabilityUnsupportedEverywhere` before any re-probe slot is claimed.
//!
//! The filter-level rows drive `filter_chain_by_features` over two lanes and
//! read the registry's `in_flight` slots afterwards; the router-level row
//! learns its negative through a real dispatch and walks it through decay,
//! one probe, and a same-capability rejection.

use super::capability_precedence_matrix_tests::{KIND, base_config, lane_of, seed_learned};
use super::feature_filter_read_tests::seed_lapsed;
use super::*;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, ToolDef};
use routectl_testkit::{CapturedEvent, capture_events};
use serde_json::json;

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

/// The beta token a `context_management` strip removes; pinning it on a
/// lane rules the strip out there.
const CONTEXT_MANAGEMENT_PIN: &str = "context-management-2025-06-27";

/// A provider that refuses every call with one fixed upstream error and
/// counts the calls, so a test reads how many requests reached the wire.
struct RefusingProvider {
    calls: AtomicUsize,
}

impl RefusingProvider {
    /// A self-identifying openai-compat 400 naming `web_search` in
    /// `/error/param`: the classifier lifts it to a feature rejection and the
    /// resolver attributes it to the capability.
    fn refusal(&self) -> Error {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Error::upstream_full(
            "p1",
            400,
            r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search","message":"Unsupported parameter."}}"#,
            None,
            Some("invalid_request_error".into()),
            Some("unsupported_parameter".into()),
        )
    }
}

#[async_trait::async_trait]
impl Provider for RefusingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.refusal())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.refusal())
    }
}

/// A target on provider `p` for `nickname`, its own upstream (lane
/// `p#<nickname>`), pinning [`CONTEXT_MANAGEMENT_PIN`] through the model's
/// `header_extras` when `pinned`. The filter rows never dispatch it.
fn lane_target(router: &Router, nickname: &str, pinned: bool) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(RefusingProvider {
        calls: AtomicUsize::new(0),
    });
    let mut header_extras = BTreeMap::new();
    if pinned {
        header_extras.insert("anthropic-beta".to_string(), CONTEXT_MANAGEMENT_PIN.into());
    }
    let model =
        ResolvedModel::new(nickname, "p", provider, nickname).with_header_extras(header_extras);
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

/// A corroborated inferred negative: it acts, from two observations.
fn seed_inferred(router: &Router, nickname: &str, feature: &str) {
    for _ in 0..2 {
        router.learned_capabilities.observe(
            &lane_of(nickname),
            feature,
            KIND,
            SignalTier::Inferred,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            Instant::now(),
        );
    }
}

/// A lapsed negative whose single re-probe slot another request already
/// claimed.
fn seed_lapsed_held(router: &Router, nickname: &str, feature: &str) {
    seed_lapsed(router, nickname, feature);
    let claimed = router.learned_capabilities.acting_negative_for(
        &lane_of(nickname),
        feature,
        KIND,
        Instant::now(),
    );
    assert_eq!(
        claimed,
        crate::learned_capability::RoutingDecision::ProbeAdmitted,
        "the fixture's other request holds the slot",
    );
}

/// Whether the resident entry for `(nickname, feature)` holds its slot.
fn in_flight(router: &Router, nickname: &str, feature: &str) -> bool {
    router
        .learned_capabilities
        .export_entries()
        .into_iter()
        .find(|entry| entry.state_key == lane_of(nickname) && entry.feature_key == feature)
        .expect("the seeded entry is resident")
        .in_flight
}

/// How lane `a` is seeded in a two-lane row; lane `b` always carries an
/// eligible acting self-identifying negative unless the row says otherwise.
#[derive(Clone, Copy)]
enum LaneA {
    Acting(FailurePhase),
    Inferred,
    Pinned(FailurePhase),
    LapsedHeld,
    LapsedFree,
}

struct Row {
    name: &'static str,
    extra: &'static str,
    feature: &'static str,
    lane_a: LaneA,
    lane_b_phase: FailurePhase,
    surface: DispatchSurface,
}

/// Build the row's router and two-lane chain, seeded per the row.
fn arrange(row: &Row) -> (Router, Vec<DispatchTarget>) {
    let router = Router::new(Arc::new(base_config(row.extra)));
    let pinned = matches!(row.lane_a, LaneA::Pinned(_));
    let chain = vec![
        lane_target(&router, "a", pinned),
        lane_target(&router, "b", false),
    ];
    match row.lane_a {
        LaneA::Acting(phase) | LaneA::Pinned(phase) => {
            seed_learned(&router, "a", row.feature, phase);
        }
        LaneA::Inferred => seed_inferred(&router, "a", row.feature),
        LaneA::LapsedHeld => seed_lapsed_held(&router, "a", row.feature),
        LaneA::LapsedFree => seed_lapsed(&router, "a", row.feature),
    }
    seed_learned(&router, "b", row.feature, row.lane_b_phase);
    (router, chain)
}

fn run(
    router: &Router,
    chain: Vec<DispatchTarget>,
    features: &[&str],
    surface: DispatchSurface,
) -> (
    Result<Vec<DispatchTarget>>,
    Vec<ProbeAdmission>,
    Vec<CapturedEvent>,
) {
    let features: Vec<String> = features.iter().map(|f| (*f).to_string()).collect();
    let mut admissions = Vec::new();
    let mut result = None;
    let events = capture_events(|| {
        result = Some(router.filter_chain_by_features(
            chain,
            &features,
            "alias",
            surface,
            &mut admissions,
        ));
    });
    (result.expect("the filter ran"), admissions, events)
}

#[test]
fn every_lane_refusing_outright_fails_before_any_claim() {
    let rows = [
        Row {
            name: "F1 on one lane, F2 on the other, route-away key",
            extra: "",
            feature: "web_search",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F2,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "stream surface",
            extra: "",
            feature: "web_search",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Stream,
        },
        Row {
            name: "droppable key the operator marked essential",
            extra: "essential = [\"advisor\"]\n",
            feature: "advisor",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "lapsed lane whose slot another request holds",
            extra: "",
            feature: "web_search",
            lane_a: LaneA::LapsedHeld,
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Complete,
        },
    ];
    for row in rows {
        // Arrange
        let (router, chain) = arrange(&row);
        let held_before = in_flight(&router, "a", row.feature);

        // Act
        let (result, admissions, events) = run(&router, chain, &[row.feature], row.surface);

        // Assert
        let name = row.name;
        let Err(Error::CapabilityUnsupportedEverywhere { capability, lanes }) = result else {
            panic!("{name}: expected the typed refusal, got {:?}", result.err());
        };
        assert_eq!(&*capability, row.feature, "{name}");
        let lanes: Vec<&str> = lanes.iter().map(|lane| &**lane).collect();
        assert_eq!(lanes, vec![lane_of("a"), lane_of("b")], "{name}");
        assert!(admissions.is_empty(), "{name}: nothing claimed");
        assert_eq!(router.metrics.probe_attempts_total(), 0, "{name}");
        assert_eq!(
            in_flight(&router, "a", row.feature),
            held_before,
            "{name}: lane a's slot is as it was",
        );
        assert!(!in_flight(&router, "b", row.feature), "{name}: lane b");
        assert_eq!(router.metrics.capability_hard_excluded_total(), 1, "{name}");
        let warn = events
            .iter()
            .find(|event| event.field("event") == Some("hard_exclusion"))
            .unwrap_or_else(|| panic!("{name}: the refusal WARNs"));
        assert_eq!(warn.level, tracing::Level::WARN, "{name}");
        assert_eq!(warn.field("capability_key"), Some(row.feature), "{name}");
        assert_eq!(warn.field("alias"), Some("alias"), "{name}");
        assert_eq!(
            warn.field("lanes"),
            Some(format!("{}, {}", lane_of("a"), lane_of("b")).as_str()),
            "{name}",
        );
        assert_eq!(warn.field("surface"), Some(row.surface.as_str()), "{name}");
    }
}

#[test]
fn one_lane_that_does_not_refuse_outright_keeps_the_soft_tail() {
    let rows = [
        Row {
            name: "one lane inferred, corroborated",
            extra: "",
            feature: "web_search",
            lane_a: LaneA::Inferred,
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "one lane pins the capability",
            extra: "essential = [\"context_management\"]\n",
            feature: "context_management",
            lane_a: LaneA::Pinned(FailurePhase::F1),
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "one lane force_supported",
            extra: "[models.a]\nprovider = \"p\"\nupstream = \"a\"\n\
                    [capability.overrides.\"p:a\"]\nforce_supported = [\"web_search\"]\n",
            feature: "web_search",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "one lane strips a droppable it does not route away",
            extra: "",
            feature: "advisor",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F2,
            surface: DispatchSurface::Complete,
        },
        Row {
            name: "count_tokens with every lane negative",
            extra: "",
            feature: "web_search",
            lane_a: LaneA::Acting(FailurePhase::F1),
            lane_b_phase: FailurePhase::F1,
            surface: DispatchSurface::CountTokens,
        },
    ];
    for row in rows {
        // Arrange
        let (router, chain) = arrange(&row);

        // Act
        let (result, admissions, events) = run(&router, chain, &[row.feature], row.surface);

        // Assert
        let name = row.name;
        let out =
            result.unwrap_or_else(|err| panic!("{name}: expected the soft path, got {err:?}"));
        assert_eq!(out.len(), 2, "{name}: both lanes stay in the chain");
        assert!(admissions.is_empty(), "{name}");
        assert_eq!(router.metrics.capability_hard_excluded_total(), 0, "{name}");
        assert!(
            !events
                .iter()
                .any(|event| event.field("event") == Some("hard_exclusion")),
            "{name}: no refusal WARN",
        );
    }
}

#[test]
fn a_lapsed_lane_with_a_free_slot_dispatches_exactly_one_probe() {
    // Arrange
    let row = Row {
        name: "one lane lapsed, slot free",
        extra: "",
        feature: "web_search",
        lane_a: LaneA::LapsedFree,
        lane_b_phase: FailurePhase::F1,
        surface: DispatchSurface::Complete,
    };
    let (router, chain) = arrange(&row);

    // Act
    let (result, admissions, _) = run(&router, chain, &[row.feature], row.surface);

    // Assert -- the probe lane leads; the acting lane is the tail.
    let out = result.expect("a free probe slot keeps the request alive");
    let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
    assert_eq!(order, vec!["a", "b"]);
    assert_eq!(admissions.len(), 1, "exactly one probe admitted");
    assert_eq!(admissions[0].learned_key, lane_of("a"));
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert!(
        in_flight(&router, "a", row.feature),
        "the claiming pass took it"
    );
    assert_eq!(router.metrics.capability_hard_excluded_total(), 0);
}

#[test]
fn a_fail_fast_releases_nothing_it_did_not_claim_and_claims_nothing() {
    // Arrange -- `computer_use` (requested first) is lapsed on both lanes:
    // lane a's slot is held by another request, lane b's is free.
    // `web_search` is refused outright on both lanes, so the request fails.
    // A claiming pass that ran first would take lane b's free slot.
    let router = Router::new(Arc::new(base_config("")));
    let chain = vec![
        lane_target(&router, "a", false),
        lane_target(&router, "b", false),
    ];
    seed_lapsed_held(&router, "a", "computer_use");
    seed_lapsed(&router, "b", "computer_use");
    seed_learned(&router, "a", "web_search", FailurePhase::F1);
    seed_learned(&router, "b", "web_search", FailurePhase::F2);

    // Act
    let (result, admissions, _) = run(
        &router,
        chain,
        &["computer_use", "web_search"],
        DispatchSurface::Complete,
    );

    // Assert
    let Err(Error::CapabilityUnsupportedEverywhere { capability, .. }) = result else {
        panic!("expected the typed refusal, got {:?}", result.err());
    };
    assert_eq!(&*capability, "web_search");
    assert!(admissions.is_empty());
    assert_eq!(router.metrics.probe_attempts_total(), 0);
    assert!(
        in_flight(&router, "a", "computer_use"),
        "the other request's claim on lane a is untouched",
    );
    assert!(
        !in_flight(&router, "b", "computer_use"),
        "lane b's free slot was never claimed",
    );
}

// --- router level: learned through a real dispatch, then decay ---

const ROUTER_TOML: &str = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;

const ROUTER_LANE: &str = "p1#wire-model";

fn refusing_router() -> (Router, Arc<RefusingProvider>) {
    let config: Config = toml::from_str(ROUTER_TOML).expect("valid test toml");
    let provider = Arc::new(RefusingProvider {
        calls: AtomicUsize::new(0),
    });
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            Arc::clone(&provider) as Arc<dyn Provider>,
            "wire-model",
        )),
    );
    router.install_resolved_models(models);
    (router, provider)
}

fn web_search_request() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({ "type": "web_search" }))]),
        ..Default::default()
    }
}

/// Move the lane's negative to the end of its decay window, keeping every
/// other field, so the next read sees it lapsed against the registry's own
/// clock.
fn decay(router: &Router) {
    let lapsed_at = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("test clock is well past boot");
    let entries: Vec<_> = router
        .learned_capabilities
        .export_entries()
        .into_iter()
        .map(|mut entry| {
            entry.expires_at = lapsed_at;
            entry
        })
        .collect();
    router.learned_capabilities.import_entries(entries);
}

fn is_hard_excluded<T>(result: &Result<T>) -> bool {
    matches!(result, Err(Error::CapabilityUnsupportedEverywhere { .. }))
}

#[tokio::test]
async fn a_lapsed_exclusion_dispatches_one_probe_and_a_rejection_restores_it() {
    // Arrange -- the first request learns the negative from the upstream's
    // own rejection.
    let (router, provider) = refusing_router();
    let learned = router
        .complete_with_options(web_search_request(), RouterOptions::default())
        .await;
    assert!(learned.result.is_err());
    let after_learn = provider.calls.load(Ordering::SeqCst);
    assert!(after_learn >= 1, "the learning request reached the wire");

    // Act + Assert -- the acting negative refuses complete and stream
    // without a wire call.
    let refused = router
        .complete_with_options(web_search_request(), RouterOptions::default())
        .await;
    assert!(is_hard_excluded(&refused.result));
    let refused_stream = router
        .stream_with_options(web_search_request(), RouterOptions::default())
        .await;
    assert!(is_hard_excluded(&refused_stream.result));
    assert_eq!(provider.calls.load(Ordering::SeqCst), after_learn);

    // After decay: exactly one probe reaches the wire and is refused again.
    decay(&router);
    let probed = router
        .complete_with_options(web_search_request(), RouterOptions::default())
        .await;
    assert!(probed.result.is_err());
    assert!(!is_hard_excluded(&probed.result), "the probe dispatched");
    assert_eq!(provider.calls.load(Ordering::SeqCst), after_learn + 1);
    assert_eq!(router.metrics.probe_attempts_total(), 1);
    assert_eq!(router.metrics.probe_failures_total(), 1);

    // The same-capability rejection restored the exclusion.
    let restored = router
        .complete_with_options(web_search_request(), RouterOptions::default())
        .await;
    assert!(is_hard_excluded(&restored.result));
    assert_eq!(provider.calls.load(Ordering::SeqCst), after_learn + 1);
    assert!(
        !router
            .learned_capabilities
            .export_entries()
            .iter()
            .any(|entry| entry.state_key == ROUTER_LANE && entry.in_flight),
        "the settled probe left no slot held",
    );
    assert_eq!(router.metrics.capability_hard_excluded_total(), 3);
}
