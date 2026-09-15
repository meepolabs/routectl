//! THE cross-walk contract for the per-request repair ceiling: every
//! dispatch walk must spend at most `REPAIRS_PER_REQUEST` repairs for one
//! logical client request, however many targets its fallback chain offers.
//!
//! Why one file rather than per-walk tests: each walk's own repair gate
//! passes in isolation while the request-scoped ceiling is broken -- a
//! per-target budget lets an N-target chain pay N repairs, and every
//! per-walk test still sees "one repair per target". Only an N-target
//! fixture measured against the SHARED ceiling can fail on that, and only
//! a single site asserting the walks together keeps a later edit to one
//! walk from silently desyncing the others.
//!
//! The N-target fixtures are deliberately more targets than the ceiling
//! permits, every one of them repair-eligible and every one rejecting even
//! the repaired variant, so the observed repair count is bounded by the
//! ceiling and by nothing else. With a per-target budget the same fixture
//! yields one repair per target; the assertions below distinguish the two.
//!
//! ## Which walks are covered BEHAVIORALLY, and why not all of them
//!
//! `complete` and `stream` pre-content are covered end-to-end: a repair
//! genuinely fires in them, so the repair count is observable and the
//! ceiling is what bounds it.
//!
//! `count_tokens` is NOT, and the difference is a property of the current
//! code rather than a gap in the fixture. It admits only `anthropic-api`
//! seats and Anthropic-family `bedrock` seats (`seat_can_count_tokens`),
//! while the classifier's replay-rejection lift is closed over the kinds
//! with a captured envelope -- today only `openai-responses`. Both sets
//! read the SAME `DispatchTarget::provider_kind`, so they are disjoint: no
//! seat the token-count walk can dispatch to can reach the replay class,
//! and the walk's repair arm is therefore INERT for the only repair kind
//! that exists today. That is verified here rather than assumed
//! (`the_replay_lift_and_count_tokens_capable_kinds_are_disjoint_today`),
//! because it is exactly the kind of premise that rots.
//!
//! So for `count_tokens` this file asserts only what a run can OBSERVE:
//! that every count-capable seat is visited once, and that the current
//! provider/classifier disjointness yields zero repairs. It deliberately
//! does NOT claim the budget is threaded -- a threaded budget and a
//! per-seat budget both produce zero repairs against an unreachable class,
//! so no fixture here can distinguish them. Every wiring claim for that
//! walk (threading, condition order, the calibration re-stamp, probe-slot
//! ordering, the body scrub, the absent settlement) belongs solely to the
//! source guards in `count_tokens_repair_structure_tests`, which are
//! mutation-verified against exactly those edits. When a repair kind lands
//! whose lane a capable seat reaches, `count_tokens` gets a real N-seat
//! behavioral test here and those structural guards are deleted rather
//! than kept alongside it.

use super::super::Router;
use super::repair_budget::{REPAIRS_PER_REQUEST, RepairBudget};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::BoxStream;
use routectl_core::failure_class::{ReplayAttempt, classify_with_attempt};
use routectl_core::{
    CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent,
    Provider, ReasoningDetail, ReasoningDetailKind, Result, Role, TokenCount,
};
use serde_json::json;

use crate::config::{AliasValue, Config};
use crate::resolved::ResolvedModel;

/// The pinned replay-rejection body, byte-exact -- the same fixture the
/// replay-repair coverage uses, so the classifier lifts it to the proven
/// replay-rejection class on every walk. Carries no secret.
const REPLAY_REJECT_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// Chain length, chosen strictly greater than the ceiling so a per-target
/// budget and the shared per-request budget give DIFFERENT repair counts on
/// the same fixture. A per-target budget would permit one repair per target
/// (four); the shared ceiling permits `REPAIRS_PER_REQUEST` (two).
const CHAIN_TARGETS: usize = 4;

/// A mock lane whose replay validator always rejects: the carried variant
/// AND the repaired (stripped) variant both draw the proven rejection, so
/// every target in the chain is repair-eligible and none can succeed. That
/// makes the total repair count a pure read of whatever budget bounds it.
///
/// `calls` counts every upstream touch on this seat; `repairs` counts only
/// the ones arriving with the artifacts already stripped, i.e. the repair
/// re-dispatches.
struct AlwaysRejectingLane {
    calls: AtomicUsize,
    repairs: AtomicUsize,
}

impl AlwaysRejectingLane {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            repairs: AtomicUsize::new(0),
        }
    }

    /// Record one dispatch and answer the replay rejection. A request that
    /// arrives with no artifact is the REPAIRED variant of a rejection this
    /// seat already answered.
    fn observe_and_reject(&self, req: &ChatRequest) -> Error {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let carries_artifact = req
            .messages
            .iter()
            .any(|message| !message.reasoning_details.is_empty());
        if !carries_artifact {
            self.repairs.fetch_add(1, Ordering::SeqCst);
        }
        Error::upstream("repair-budget-mock", 400, REPLAY_REJECT_BODY)
    }
}

#[async_trait::async_trait]
impl Provider for AlwaysRejectingLane {
    fn id(&self) -> &'static str {
        "repair-budget-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("repair-budget-mock", "unused"))
    }
    fn replay_lane(&self) -> routectl_core::ReplayScheme {
        routectl_core::ReplayScheme::Mantle
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        Err(self.observe_and_reject(&req))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.observe_and_reject(&req))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        // The token-count walk advances only on a CAPABILITY error (a
        // wire-501), never on a health error -- so this is what makes it
        // visit every seat in the chain. `observe_and_reject` still records
        // the dispatch and whether the request arrived repaired.
        let _ = self.observe_and_reject(&req);
        Err(Error::upstream("repair-budget-mock", 501, "cannot count"))
    }
}

/// A lane that serves the repaired variant, so a walk can be observed
/// SUCCEEDING through a repair rather than only exhausting its budget.
struct RepairingLane {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for RepairingLane {
    fn id(&self) -> &'static str {
        "repairing-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("repairing-mock", "unused"))
    }
    fn replay_lane(&self) -> routectl_core::ReplayScheme {
        routectl_core::ReplayScheme::Mantle
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if carries_artifact(&req) {
            return Err(Error::upstream("repairing-mock", 400, REPLAY_REJECT_BODY));
        }
        Ok(success_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("repairing-mock", 500, "unused"))
    }
}

fn carries_artifact(req: &ChatRequest) -> bool {
    req.messages
        .iter()
        .any(|message| !message.reasoning_details.is_empty())
}

fn success_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        ..Default::default()
    }
}

/// An assistant turn carrying one non-portable reasoning artifact, the
/// input every repair-eligible walk needs.
fn assistant_with_artifact() -> Message {
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

fn req_on(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.into(),
        messages: vec![assistant_with_artifact()].into(),
        ..Default::default()
    }
}

/// An `openai-responses` chain of `CHAIN_TARGETS` legs, each on its own
/// provider entry (so per-seat gates, breakers and lane keys are
/// independent) with its own always-rejecting mock. Returns the router and
/// the per-leg mocks in chain order.
///
/// `openai-responses` is the fixture-backed kind the replay classifier
/// gates on, and each leg carries a DISTINCT provider name so the replay
/// single-flight key differs per leg -- otherwise the second leg would be
/// refused its carry by the first leg's in-flight probe and the fixture
/// would bound the repair count for the wrong reason.
fn rejecting_chain(alias: &str) -> (Router, Vec<Arc<AlwaysRejectingLane>>) {
    let mut toml_text = String::new();
    let mut chain: Vec<String> = Vec::with_capacity(CHAIN_TARGETS);
    for idx in 0..CHAIN_TARGETS {
        toml_text.push_str(&format!(
            "\n[providers.p{idx}]\nkind = \"openai-responses\"\napi_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n"
        ));
        chain.push(format!("m{idx}"));
    }
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(alias.to_string(), AliasValue::Chain(chain));

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut mocks: Vec<Arc<AlwaysRejectingLane>> = Vec::with_capacity(CHAIN_TARGETS);
    for idx in 0..CHAIN_TARGETS {
        let mock = Arc::new(AlwaysRejectingLane::new());
        mocks.push(mock.clone());
        models.insert(
            format!("m{idx}"),
            Arc::new(ResolvedModel::new(
                format!("m{idx}"),
                format!("p{idx}"),
                mock as Arc<dyn Provider>,
                format!("wire-{idx}"),
            )),
        );
    }
    router.install_resolved_models(models);
    (router, mocks)
}

/// Total repair re-dispatches observed across every leg of the chain.
fn repairs_observed(mocks: &[Arc<AlwaysRejectingLane>]) -> usize {
    mocks.iter().map(|m| m.repairs.load(Ordering::SeqCst)).sum()
}

/// An `anthropic-api` chain of `CHAIN_TARGETS` legs -- the kind
/// `seat_can_count_tokens` admits, so the token-count walk actually
/// dispatches to every leg. Each leg rejects, so the walk runs to
/// exhaustion and every seat is observed.
fn capable_rejecting_chain(alias: &str) -> (Router, Vec<Arc<AlwaysRejectingLane>>) {
    let mut toml_text = String::new();
    let mut chain: Vec<String> = Vec::with_capacity(CHAIN_TARGETS);
    for idx in 0..CHAIN_TARGETS {
        toml_text.push_str(&format!(
            "\n[providers.p{idx}]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n"
        ));
        chain.push(format!("m{idx}"));
    }
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(alias.to_string(), AliasValue::Chain(chain));

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut mocks: Vec<Arc<AlwaysRejectingLane>> = Vec::with_capacity(CHAIN_TARGETS);
    for idx in 0..CHAIN_TARGETS {
        let mock = Arc::new(AlwaysRejectingLane::new());
        mocks.push(mock.clone());
        models.insert(
            format!("m{idx}"),
            Arc::new(ResolvedModel::new(
                format!("m{idx}"),
                format!("p{idx}"),
                mock as Arc<dyn Provider>,
                format!("wire-{idx}"),
            )),
        );
    }
    router.install_resolved_models(models);
    (router, mocks)
}

/// Total upstream touches observed across every leg of the chain.
fn calls_observed(mocks: &[Arc<AlwaysRejectingLane>]) -> usize {
    mocks.iter().map(|m| m.calls.load(Ordering::SeqCst)).sum()
}

/// The fixture's own premise, asserted so a later change that stops the
/// walk short (a gate refusal, a lane mismatch, a classifier change) fails
/// LOUDLY here instead of making every ceiling assertion below pass
/// vacuously on a chain that never repaired at all.
fn assert_fixture_exercised_every_target(mocks: &[Arc<AlwaysRejectingLane>], surface: &str) {
    assert!(
        CHAIN_TARGETS > usize::from(REPAIRS_PER_REQUEST),
        "the fixture must offer more repair-eligible targets than the ceiling permits, \
         or a per-target budget would produce the same count as the shared one",
    );
    for (idx, mock) in mocks.iter().enumerate() {
        assert!(
            mock.calls.load(Ordering::SeqCst) > 0,
            "{surface}: target {idx} was never dispatched, so the repair ceiling is \
             not what bounded this walk",
        );
    }
}

#[tokio::test]
async fn complete_spends_at_most_the_request_ceiling_across_an_n_target_chain() {
    // Arrange -- four repair-eligible targets, one logical request.
    let (router, mocks) = rejecting_chain("chain");

    // Act
    let result = router.complete(req_on("chain")).await;

    // Assert -- the ceiling, not the target count, bounds the repairs.
    assert!(result.is_err(), "no target can serve the repaired variant");
    assert_fixture_exercised_every_target(&mocks, "complete");
    assert_eq!(
        repairs_observed(&mocks),
        usize::from(REPAIRS_PER_REQUEST),
        "complete must spend the shared per-request ceiling, not one repair per target",
    );
    assert_eq!(
        calls_observed(&mocks),
        CHAIN_TARGETS + usize::from(REPAIRS_PER_REQUEST),
        "one carried attempt per target plus exactly the shared repair allowance",
    );
}

#[tokio::test]
async fn stream_pre_content_spends_at_most_the_request_ceiling_across_an_n_target_chain() {
    // Arrange
    let (router, mocks) = rejecting_chain("chain");

    // Act
    let result = router.stream(req_on("chain")).await;

    // Assert
    assert!(result.is_err(), "no target can serve the repaired variant");
    assert_fixture_exercised_every_target(&mocks, "stream");
    assert_eq!(
        repairs_observed(&mocks),
        usize::from(REPAIRS_PER_REQUEST),
        "stream pre-content must spend the shared per-request ceiling",
    );
    assert_eq!(
        calls_observed(&mocks),
        CHAIN_TARGETS + usize::from(REPAIRS_PER_REQUEST),
        "one carried attempt per target plus exactly the shared repair allowance",
    );
}

#[tokio::test]
async fn every_count_capable_seat_is_visited_once_and_none_repairs_today() {
    // Arrange -- the token-count walk over a chain of count-capable seats,
    // each answering a capability error so the walk runs to exhaustion.
    //
    // SCOPE: this claims only what the run can OBSERVE. It does not claim the
    // budget is threaded -- a threaded budget and a per-seat budget produce
    // the same zero repairs here, because no capable seat can reach today's
    // only repair class, so this fixture cannot distinguish them. The
    // threading claim belongs solely to the source guards in
    // `count_tokens_repair_structure_tests`, which fail on exactly that
    // mutation. What this DOES establish is the premise those guards rest on:
    // the walk really visits every capable seat, and the observed repair count
    // really is zero under the current provider/classifier disjointness.
    let (router, mocks) = capable_rejecting_chain("chain");

    // Act
    let result = router.count_tokens(req_on("chain")).await;

    // Assert
    assert!(result.is_err(), "no seat can serve the count");
    for (idx, mock) in mocks.iter().enumerate() {
        assert_eq!(
            mock.calls.load(Ordering::SeqCst),
            1,
            "seat {idx} is dispatched exactly once by the walk",
        );
        assert_eq!(
            mock.repairs.load(Ordering::SeqCst),
            0,
            "seat {idx}: today's only repair kind is unreachable from a capable seat, so a \
             repair here would mean the disjointness pinned below has changed",
        );
    }
}

#[test]
fn the_scrub_removes_the_body_and_preserves_the_status_type_and_code() {
    // The token-count walk rebuilds a classified replay rejection body-free
    // before it can return the error. Its POSITION is pinned by a source
    // guard (a behavioral test cannot reach that class through a capable
    // seat); its EFFECT is pinned here, on the same helper the walk calls, so
    // "scrubbed" is a measured property rather than a claim about a call.
    //
    // The fixture carries NON-EMPTY structured classifier tokens on purpose:
    // the scrub's contract is "drop ONLY the body", and with `None` tokens a
    // scrub that also wiped them would read as correct. Preservation is only
    // checkable against tokens that were there to lose.
    let marker = "encrypted content missing recognized prefix";
    let upstream_type = "invalid_request_error";
    let upstream_code = "validation_error";
    assert!(
        REPLAY_REJECT_BODY.contains(marker),
        "premise: the fixture rejection body carries the artifact marker",
    );

    let rejection = Error::upstream_full(
        "p",
        400,
        REPLAY_REJECT_BODY,
        None,
        Some(upstream_type.to_string()),
        Some(upstream_code.to_string()),
    );
    // Premise: the fixture really does carry both tokens, so the
    // preservation assertions below cannot pass on absent values.
    assert!(
        matches!(
            &rejection,
            Error::Upstream { upstream_type: Some(t), upstream_code: Some(c), .. }
                if &**t == upstream_type && &**c == upstream_code
        ),
        "premise: the fixture carries both structured classifier tokens",
    );
    let class = classify_with_attempt(
        &rejection,
        Some("openai-responses"),
        ReplayAttempt::with_gray_artifacts(1),
    )
    .class;

    // Act
    let scrubbed = super::dispatch::replay_rejection_body_free(&rejection, &class, "p")
        .expect("a classified replay rejection must rebuild body-free");

    // Assert -- the body is gone, and the status plus both tokens survive
    // byte-exact (every downstream consumer reads them).
    match &scrubbed {
        Error::Upstream {
            status,
            body,
            upstream_type: got_type,
            upstream_code: got_code,
            ..
        } => {
            assert!(
                body.is_empty(),
                "the scrub must drop the whole body, got: {body}",
            );
            assert_eq!(*status, 400, "the status must survive the rebuild");
            assert_eq!(
                got_type.as_deref(),
                Some(upstream_type),
                "the structured error type must survive the rebuild",
            );
            assert_eq!(
                got_code.as_deref(),
                Some(upstream_code),
                "the structured error code must survive the rebuild",
            );
        }
        other => panic!("the scrub must rebuild an Upstream error, got {other:?}"),
    }
    assert!(
        !format!("{scrubbed:?}").contains(marker),
        "the scrubbed error must not carry the rejection body in any rendering",
    );
    assert!(
        format!("{rejection:?}").contains(marker),
        "control: the UNSCRUBBED error does carry it, so the assertion above is not vacuous",
    );
}

#[test]
fn the_replay_lift_and_count_tokens_capable_kinds_are_disjoint_today() {
    // The premise the test above rests on, asserted rather than assumed: a
    // provider kind the token-count walk admits cannot reach the classifier's
    // replay-rejection class, so the walk's repair arm is inert for today's
    // only repair kind. The positive control is what makes this non-vacuous
    // -- it proves the fixture DOES lift on a kind that is fixture-backed,
    // so the negatives below are about the kinds and not about the body.
    let carried = ReplayAttempt::with_gray_artifacts(1);

    let lifted = classify_with_attempt(
        &Error::upstream("p", 400, REPLAY_REJECT_BODY),
        Some("openai-responses"),
        carried,
    );
    assert!(
        Router::is_replay_rejection_class(&lifted.class),
        "positive control: the fixture body lifts on a fixture-backed kind",
    );

    for kind in ["anthropic-api", "bedrock"] {
        let classified = classify_with_attempt(
            &Error::upstream("p", 400, REPLAY_REJECT_BODY),
            Some(kind),
            carried,
        );
        assert!(
            !Router::is_replay_rejection_class(&classified.class),
            "a count_tokens-capable kind ({kind}) must not reach the replay class, \
             or the walk's inert repair arm would be reachable and needs end-to-end \
             coverage here",
        );
    }
}

#[test]
fn a_budget_threaded_by_reference_is_shared_while_a_fresh_one_per_seat_is_not() {
    // The seam the token-count walk depends on, pinned directly: the walk
    // passes ONE budget by `&mut` into every seat. This asserts the two
    // shapes differ observably, so "threaded" is a checkable property rather
    // than a claim about the code's appearance.
    //
    // Shared: N seats draw from one allowance and the total is the ceiling.
    fn seat(budget: &mut RepairBudget) -> bool {
        budget.draw()
    }
    let mut shared = RepairBudget::per_request();
    let seats = usize::from(REPAIRS_PER_REQUEST) + 2;
    let shared_total = (0..seats).filter(|_| seat(&mut shared)).count();

    // Per-seat: each seat constructs its own, so every seat repairs.
    let per_seat_total = (0..seats)
        .filter(|_| {
            let mut own = RepairBudget::per_request();
            seat(&mut own)
        })
        .count();

    assert_eq!(
        shared_total,
        usize::from(REPAIRS_PER_REQUEST),
        "one threaded budget bounds the whole walk at the request ceiling",
    );
    assert_eq!(
        per_seat_total, seats,
        "a fresh budget per seat is the defect this threading removes",
    );
}

#[tokio::test]
async fn both_messages_walks_agree_on_one_ceiling() {
    // Arrange -- a fresh chain per walk: the ceiling is PER REQUEST, so each
    // request gets the full allowance and the two counts must come out
    // identical. A walk that kept a per-target budget (or lost its repair
    // position) reads as a different number here.
    //
    // SCOPE: this asserts the two walks a repair can actually be OBSERVED in.
    // `count_tokens` is deliberately absent -- its capable seats cannot reach
    // today's only repair kind, so any number measured for it would be zero
    // for a reason unrelated to the ceiling, and reading a standalone
    // `RepairBudget` here would assert the budget type against itself rather
    // than the walk. Its observable share is covered by
    // `every_count_capable_seat_is_visited_once_and_none_repairs_today`, and
    // its wiring by the structural guards in
    // `count_tokens_repair_structure_tests`; it joins this test once a repair
    // kind exists whose lane a capable seat reaches.
    let (complete_router, complete_mocks) = rejecting_chain("chain");
    let (stream_router, stream_mocks) = rejecting_chain("chain");

    // Act
    let _ = complete_router.complete(req_on("chain")).await;
    let _ = stream_router.stream(req_on("chain")).await;

    // Assert
    let observed = [
        repairs_observed(&complete_mocks),
        repairs_observed(&stream_mocks),
    ];
    assert_eq!(
        observed,
        [usize::from(REPAIRS_PER_REQUEST); 2],
        "complete and stream pre-content must share ONE per-request ceiling",
    );
}

#[tokio::test]
async fn the_repaired_request_is_the_one_a_messages_walk_dispatches() {
    // Arrange -- a single target that rejects the carried variant and serves
    // the repaired one. If the walk repaired but then dispatched the
    // ORIGINAL request, the target would reject again and no success would
    // come back. The same code path (`strip_replay_artifacts_recalibrating`
    // on `attempt_req`, then `continue`) is what the token-count walk's arm
    // uses, so this pins the post-repair dispatch shape.
    let toml_text = r#"
[providers.p0]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"
"#;
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mock = Arc::new(RepairingLane {
        calls: AtomicUsize::new(0),
    });
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0",
            "p0",
            mock.clone() as Arc<dyn Provider>,
            "wire-0",
        )),
    );
    router.install_resolved_models(models);

    // Act
    let served = router.complete(req_on("m0")).await;

    // Assert -- the repaired request served, in exactly two upstream touches
    // (carried rejection plus one repair).
    assert!(
        served.is_ok(),
        "the repaired request must be the one dispatched: {served:?}",
    );
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "one carried attempt plus one repair re-dispatch",
    );
}
