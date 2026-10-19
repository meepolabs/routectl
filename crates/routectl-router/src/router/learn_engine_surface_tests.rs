//! What the count surface changes about the engine, driven through the engine
//! directly: it mints no generic verdict, counts a rejection the field parser
//! did not localize, settles a repaired replay carry by drop, and admits its
//! field carry in whatever settlement mode its caller names. Each count case
//! runs beside a complete control on the identical target and rejection, so
//! the surface is the only input that differs.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::{
    BetaRepairReport, CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Error, Message,
    MessageContent, Provider, ReasoningDetail, ReasoningDetailKind, ReplayScheme, Result, Role,
    ToolDef,
};
use serde_json::json;

use super::super::super::capability_observe::UntranslatedCapabilities;
use super::super::super::class_observe::DispatchSurface;
use super::super::super::field_repair::{FieldSettlementMode, TEST_UNSUPPORTED_SENTINEL};
use super::super::super::repair_budget::RepairBudget;
use super::super::super::runtime_gate::LearnedProbeGuard;
use super::super::{AttemptLearning, FailureDecision, Rejection};
use crate::config::Config;
use crate::learned_capability::RoutingDecision;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, DispatchTarget, Router};

const NICKNAME: &str = "m1";
/// The learned lane the fixture's one model keys its verdicts on.
const WIRE_LANE: &str = "p1#wire-model";
const OPENAI_COMPAT: &str = "[providers.p1]\nkind = \"openai-compat\"\n\
    base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
const ANTHROPIC: &str = "[providers.p1]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
const OPENAI_RESPONSES: &str = "[providers.p1]\nkind = \"openai-responses\"\n\
    api_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n";
/// The proven replay-rejection body a mantle lane answers a foreign artifact
/// with.
const REPLAY_REJECT_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

const SURFACES: [DispatchSurface; 2] = [DispatchSurface::Complete, DispatchSurface::CountTokens];

/// A provider the engine never dispatches through; it only declares the
/// replay lane the carry admission reads.
struct LaneProvider(ReplayScheme);

#[async_trait::async_trait]
impl Provider for LaneProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    fn replay_lane(&self) -> ReplayScheme {
        self.0
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("p1", "unused"))
    }
}

fn router(toml_text: &str, lane: ReplayScheme) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(LaneProvider(lane));
    let model = ResolvedModel::new(NICKNAME, "p1", provider, "wire-model");
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

/// The one target `req` walks, built for the complete surface on every row so
/// the chain itself cannot differ between the surfaces under comparison.
fn target(router: &Router, req: &ChatRequest) -> DispatchTarget {
    router
        .dispatch_chain_for_request(req, DispatchSurface::Complete)
        .expect("the fixture chain resolves")
        .0
        .pop()
        .expect("one target")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: NICKNAME.into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// A self-identifying openai-compat refusal naming `web_search`, which the
/// generic capability resolver attributes.
fn web_search_rejection() -> Error {
    Error::upstream_full(
        "p1",
        400,
        r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search","message":"Unsupported parameter."}}"#,
        None,
        Some("invalid_request_error".into()),
        Some("unsupported_parameter".into()),
    )
}

/// An anthropic-api 400 whose message names no closed-table path.
fn unlocalized_rejection() -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": "the request is malformed"},
    });
    Error::upstream("p1", 400, body.to_string())
}

fn assistant_with_codex_artifact() -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text("prior answer".into()),
        reasoning: None,
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some("rs_1".into()),
            format: Some(CODEX_OAUTH.to_string()),
            index: None,
            payload: json!({"encrypted_content": "opaque"}),
        }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// Arbitrate `err` and then run the generic observation, as the walk's error
/// path does after a rejection that took no repair.
fn reject_and_observe(
    learning: &mut AttemptLearning<'_>,
    err: &Error,
    target: &DispatchTarget,
    req: &ChatRequest,
    meta: &mut DispatchMeta,
) {
    let mut attempt_req = req.clone();
    let cf = learning.classify(err, target.provider_kind);
    let native = cf.class.clone();
    let rejection = Rejection {
        err,
        class: &cf,
        native_class: &native,
        target,
        client_req: req,
        site: None,
        transcript_rejection: false,
    };
    let decision = learning.on_rejection(
        rejection,
        &mut attempt_req,
        meta,
        &mut RepairBudget::per_request(),
    );
    assert!(
        matches!(decision, FailureDecision::Proceed),
        "premise: the rejection takes no repair"
    );
    learning.observe_generic(
        err,
        &cf,
        false,
        target,
        req,
        &UntranslatedCapabilities::default(),
        &mut HashSet::new(),
        meta,
    );
}

#[test]
fn count_tokens_engine_never_mints_a_generic_verdict() {
    for surface in SURFACES {
        // Arrange
        let router = router(OPENAI_COMPAT, ReplayScheme::Gray);
        let mut req = request();
        req.tools = Some(vec![ToolDef::Other(json!({"type": "web_search"}))]);
        let target = target(&router, &req);
        let mut learning = AttemptLearning::new(&router, surface, LearnedProbeGuard::inert());
        let mut meta = DispatchMeta::for_alias(NICKNAME);

        // Act
        reject_and_observe(
            &mut learning,
            &web_search_rejection(),
            &target,
            &req,
            &mut meta,
        );

        // Assert
        let learned: Vec<&str> = meta
            .learned_capabilities
            .iter()
            .map(|event| event.capability_key.as_str())
            .collect();
        let decision = router.learned_capabilities.acting_negative_for(
            WIRE_LANE,
            "web_search",
            "openai-compat",
            Instant::now(),
        );
        let route_away = RoutingDecision::RouteAway {
            signal: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
        };
        if surface == DispatchSurface::Complete {
            assert_eq!(learned, ["web_search"], "{surface:?}: the control mints");
            assert_eq!(decision, route_away, "{surface:?}: and the negative acts");
        } else {
            assert!(learned.is_empty(), "{surface:?}: no verdict: {learned:?}");
            assert_ne!(decision, route_away, "{surface:?}: and no negative acts");
        }
    }
}

#[test]
fn count_tokens_engine_counts_an_unlocalized_rejection() {
    // Arrange
    let router = router(ANTHROPIC, ReplayScheme::Gray);
    let req = request();
    let target = target(&router, &req);
    let mut learning = AttemptLearning::new(
        &router,
        DispatchSurface::CountTokens,
        LearnedProbeGuard::inert(),
    );
    let mut meta = DispatchMeta::for_alias(NICKNAME);

    // Act
    reject_and_observe(
        &mut learning,
        &unlocalized_rejection(),
        &target,
        &req,
        &mut meta,
    );

    // Assert
    assert_eq!(router.field_repair_counters().parser_unlocalized, 1);
    assert!(meta.learned_capabilities.is_empty());
}

#[test]
fn count_tokens_engine_drops_a_repaired_replay_carry() {
    for surface in SURFACES {
        // Arrange: a codex artifact carried toward a mantle lane, refused with
        // the proven replay rejection.
        let router = router(OPENAI_RESPONSES, ReplayScheme::Mantle);
        let mut req = request();
        req.messages = vec![assistant_with_codex_artifact()].into();
        let target = target(&router, &req);
        let mut learning = AttemptLearning::new(&router, surface, LearnedProbeGuard::inert());
        let mut meta = DispatchMeta::for_alias(NICKNAME);
        let mut attempt_req = req.clone();
        learning.admit_carries(
            &target,
            &mut attempt_req,
            &mut meta,
            FieldSettlementMode::NonSettling,
            Instant::now(),
        );
        let err = Error::upstream("p1", 400, REPLAY_REJECT_BODY);
        let cf = learning.classify(&err, target.provider_kind);
        let native = cf.class.clone();
        let rejection = Rejection {
            err: &err,
            class: &cf,
            native_class: &native,
            target: &target,
            client_req: &req,
            site: None,
            transcript_rejection: false,
        };
        let decision = learning.on_rejection(
            rejection,
            &mut attempt_req,
            &mut meta,
            &mut RepairBudget::per_request(),
        );
        assert!(
            matches!(decision, FailureDecision::Retry { .. }),
            "{surface:?}: premise: the strip repair took"
        );

        // Act
        learning.on_success(
            &BetaRepairReport::default(),
            &UntranslatedCapabilities::default(),
            &target,
            &attempt_req,
            &mut HashSet::new(),
            &mut meta,
        );

        // Assert
        let degradation = meta
            .replay_degradation
            .as_ref()
            .expect("the repair records a degradation");
        assert!(degradation.repair_succeeded, "{surface:?}: repair served");
        let committed = !meta.learned_capabilities.is_empty();
        let resident = !router.learned_capability_snapshot().is_empty();
        if surface == DispatchSurface::Complete {
            assert!(degradation.learned, "{surface:?}: the control learns");
            assert!(committed, "{surface:?}: and commits the negative");
            assert!(resident, "{surface:?}: into the registry");
        } else {
            assert!(!degradation.learned, "{surface:?}: nothing learned");
            assert!(!committed, "{surface:?}: nothing committed");
            assert!(!resident, "{surface:?}: nothing resident");
        }
    }
}

#[test]
fn admit_carries_honors_non_settling_mode() {
    // Arrange: one target whose request carries a closed-table row.
    let router = router(ANTHROPIC, ReplayScheme::Gray);
    let mut req = request();
    req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());
    let target = target(&router, &req);
    let admit = |mode| {
        let mut learning = AttemptLearning::new(
            &router,
            DispatchSurface::Complete,
            LearnedProbeGuard::inert(),
        );
        let mut attempt_req = req.clone();
        let mut meta = DispatchMeta::for_alias(NICKNAME);
        learning.admit_carries(&target, &mut attempt_req, &mut meta, mode, Instant::now());
        learning
    };

    // Act
    let non_settling = admit(FieldSettlementMode::NonSettling);
    let settling = admit(FieldSettlementMode::Settling);
    let sibling = admit(FieldSettlementMode::Settling);

    // Assert
    assert!(
        non_settling.field_plan.is_some(),
        "a non-settling walk still admits the row for its own repair"
    );
    assert!(
        settling.field_plan.is_some(),
        "the non-settling admission held no slot, so a settling one claims it"
    );
    assert!(
        sibling.field_plan.is_none(),
        "the settling admission holds the slot against its sibling"
    );
}
