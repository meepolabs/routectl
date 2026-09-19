//! Behavioral coverage of the canonical pre-flight planner: the one
//! eligibility shape that acts, every ambiguity that falls open, and the
//! wiring through all three dispatch walks.
//!
//! # Why eligibility is PLANTED rather than earned
//!
//! A verdict becomes pre-flight eligible only when it is resident and ACTING
//! and its own incarnation carries an acknowledged confirmation. Live traffic
//! cannot produce the confirmation half today -- the durable-writer ack that
//! advances it lands separately, so the only production writer is the
//! cold-rebuild seed. These tests therefore plant both halves through the
//! registries' OWN seams (`import_entries` for the verdict, `seed_from_rebuild`
//! for the confirmation), the same way the capability tests plant learned
//! negatives, and never by reaching into the lifecycle.
//!
//! # Why the planner is exercised BOTH directly and through dispatch
//!
//! The direct cases pin the decision itself -- which reason token each
//! ambiguity resolves to, and that the caller's request is untouched. The
//! dispatch cases pin the WIRING: that each of the three walks consults the
//! planner at its own seam, dispatches the planned body, and plans each
//! fallback target from its own base rather than from a sibling's clone.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error, Message,
    MessageContent, Provider, ReasoningConfig, Result, Role, TokenCount,
};
use serde_json::json;

use super::super::class_observe::DispatchSurface;
use super::super::repair_budget::RepairBudget;
use super::super::{DispatchTarget, FieldPreflight, Router, RouterOptions};
use super::FIELD_PREFLIGHT_AMBIGUOUS_MUTATION;
use super::{
    FIELD_PREFLIGHT_ACTION_DROP, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
    FIELD_PREFLIGHT_NO_GROUNDED_FIELD, FIELD_PREFLIGHT_NOT_ELIGIBLE,
    FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET, FIELD_PREFLIGHT_UNSUPPORTED_LANE,
};

use crate::config::{AliasValue, Config};
use crate::field_verdict::FieldVerdictKey;
use crate::resolved::ResolvedModel;

/// The one grounded row in the closed table: the qualified dotted path the
/// planner can act on.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// The lane this stage acts on.
const ANTHROPIC: &str = "anthropic-api";

/// The alias every dispatch fixture routes through. Deliberately not a model
/// name: an alias that is also a chain member re-resolves through itself.
const ALIAS: &str = "field-preflight-alias";

/// How long a planted verdict stays unexpired. Any window comfortably longer
/// than a test run works; the point is only that the entry is NOT lapsed.
const NOT_LAPSED: Duration = Duration::from_hours(1);

/// The field capability key the grounded path mints. Minted through the
/// namespace owner rather than spelled out, so the prefix keeps its single
/// compiled spelling.
fn grounded_key() -> String {
    crate::field_capability::field_capability_key(GROUNDED_PATH)
        .expect("the grounded path is a well-formed qualified path")
}

/// Whether a request still emits the grounded wire field through EITHER
/// canonical carrier. Both are checked because clearing only one leaves the
/// egress emitting the field from the other.
fn carries_field(req: &ChatRequest) -> bool {
    req.routectl_internal.anthropic_thinking_display.is_some()
        || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
}

/// A request carrying the grounded field through BOTH canonical carriers,
/// plus an active thinking request so a drop can be shown to remove the field
/// WITHOUT disabling the feature.
fn req_on(alias: &str) -> ChatRequest {
    let mut req = ChatRequest {
        model: alias.into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(2048),
            exclude: Some(true),
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("updates".into());
    req
}

/// The same request with NEITHER carrier set: nothing in the closed table is
/// present, so the planner has nothing to act on.
fn req_without_field(alias: &str) -> ChatRequest {
    let mut req = req_on(alias);
    req.routectl_internal.anthropic_thinking_display = None;
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(2048),
        exclude: None,
        enabled: Some(true),
    });
    assert!(
        !carries_field(&req),
        "fixture premise: this request carries no closed-table field",
    );
    req
}

// --- routers and targets -----------------------------------------------

/// Config text for an N-leg chain of `kind` providers, each leg on its own
/// provider entry so per-seat verdict keys are independent.
fn chain_config(alias: &str, seats: usize, kind: &str) -> Config {
    let mut toml_text = String::new();
    let mut chain: Vec<String> = Vec::with_capacity(seats);
    for idx in 0..seats {
        toml_text.push_str(&format!(
            "\n[providers.p{idx}]\nkind = \"{kind}\"\napi_key_ref = \"literal:k\"\n"
        ));
        chain.push(format!("m{idx}"));
    }
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(alias.to_string(), AliasValue::Chain(chain));
    config
}

/// Install `seats` mocks answering `answer` onto `config`'s resolved models.
fn install(config: Config, seats: usize, answer: Answer) -> (Router, Vec<Arc<Observed>>) {
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut observed: Vec<Arc<Observed>> = Vec::with_capacity(seats);
    for idx in 0..seats {
        let seen = Arc::new(Observed::default());
        observed.push(seen.clone());
        let mock: Arc<dyn Provider> = Arc::new(MockSeat::new(answer, seen));
        models.insert(
            format!("m{idx}"),
            Arc::new(ResolvedModel::new(
                format!("m{idx}"),
                format!("p{idx}"),
                mock,
                format!("wire-{idx}"),
            )),
        );
    }
    router.install_resolved_models(models);
    (router, observed)
}

/// A one-seat anthropic-api chain whose provider entry carries `base_url`,
/// built through the SAME toml parse as every other fixture so the entry is
/// shaped exactly as an operator's would be (a hand-mutated variant could
/// carry a combination config validation refuses).
fn config_with_base_url(base_url: &str) -> Config {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"{ANTHROPIC}\"\napi_key_ref = \"literal:k\"\nbase_url = \"{base_url}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    config
}

fn chain_of(seats: usize, answer: Answer) -> (Router, Vec<Arc<Observed>>) {
    install(chain_config(ALIAS, seats, ANTHROPIC), seats, answer)
}

fn single_seat(answer: Answer) -> (Router, Arc<Observed>) {
    let (router, mut observed) = chain_of(1, answer);
    (router, observed.remove(0))
}

/// The real `DispatchTarget` for seat `nickname` on `router`, built through
/// the router's own chain expansion so `provider_kind` and the
/// forwarded-credential flag come from config exactly as a dispatch walk
/// would see them.
fn target_for(router: &Router, nickname: &str, provider_name: &str) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(MockSeat::new(
        Answer::ServeImmediately,
        Arc::new(Observed::default()),
    ));
    let model = ResolvedModel::new(nickname, provider_name, provider, "upstream");
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

// --- eligibility planting ----------------------------------------------

fn verdict_key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(state_key, GROUNDED_PATH, ANTHROPIC).expect("a qualified path mints a key")
}

/// Plant a resident field negative for `state_key` through the registry's own
/// carry-over import seam. `lapsed` stamps the expiry at the planting instant,
/// which is already past for every later read; otherwise the entry is ACTING.
fn plant_verdict(router: &Router, state_key: &str, lapsed: bool) {
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: grounded_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: if lapsed {
                stamped
            } else {
                stamped + NOT_LAPSED
            },
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

/// The resident entry's own incarnation for `state_key`, the value a canary
/// seed must match to back the verdict.
fn resident_incarnation(router: &Router, state_key: &str) -> u64 {
    router.learned_capabilities.resident_incarnation_for_tests(
        state_key,
        &grounded_key(),
        ANTHROPIC,
    )
}

/// Acknowledge `confirmations` for `state_key` at `incarnation`, through the
/// canary registry's own cold-rebuild seed -- the only writer live traffic has
/// for the confirmation half today.
fn acknowledge(router: &Router, state_key: &str, incarnation: u64, confirmations: u32) {
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key(state_key),
        incarnation,
        confirmations,
        false,
    );
}

/// Plant the ONE state the planner may act on: a resident ACTING verdict whose
/// own incarnation carries an acknowledged confirmation.
fn plant_eligible(router: &Router, state_key: &str) {
    plant_verdict(router, state_key, false);
    let incarnation = resident_incarnation(router, state_key);
    acknowledge(router, state_key, incarnation, 1);
    assert!(
        router.field_verdicts().preflight_eligible(
            &verdict_key(state_key),
            router.registry_generation(),
            Instant::now(),
        ),
        "fixture premise: {state_key} must be pre-flight eligible",
    );
}

// --- mock seat ----------------------------------------------------------

/// What one mock seat observed, keeping each attempt's request so an assertion
/// reads the BYTES that went upstream.
#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    dispatched: Mutex<Vec<ChatRequest>>,
}

impl Observed {
    fn record(&self, req: &ChatRequest) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.dispatched.lock().push(req.clone());
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Per attempt, in order: whether it still carried the grounded field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.dispatched.lock().iter().map(carries_field).collect()
    }

    fn attempt(&self, idx: usize) -> ChatRequest {
        self.dispatched.lock()[idx].clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Serve every attempt: the walk stops at the first seat, so what the
    /// planner did is read off that seat's dispatched bytes.
    ServeImmediately,
    /// Reject with a 503 carrying no field attribution, so the walk advances
    /// to the next chain leg without the reactive repair arm engaging.
    Unavailable,
}

struct MockSeat {
    answer: Answer,
    observed: Arc<Observed>,
}

impl MockSeat {
    const fn new(answer: Answer, observed: Arc<Observed>) -> Self {
        Self { answer, observed }
    }

    fn answer_for(&self, req: &ChatRequest) -> Result<()> {
        self.observed.record(req);
        match self.answer {
            Answer::ServeImmediately => Ok(()),
            Answer::Unavailable => Err(Error::upstream(
                "preflight-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockSeat {
    fn id(&self) -> &'static str {
        "preflight-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("preflight-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer_for(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer_for(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.answer_for(&req).map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// A CONTENT-BEARING chunk: the streaming walk commits on first content, so a
/// content-free chunk would read as a closed stream.
fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
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

/// Plan one decision for `state_key`'s target, from `req`.
///
/// The returned plan is DROPPED immediately, which is correct for every case
/// here: these tests assert the decision, and a canary plan's own settlement is
/// the subject of `field_canary_settlement_tests` instead. A cadence that came
/// due inside one of these calls therefore settles inconclusive on the drop,
/// which moves no verdict.
fn plan(router: &Router, req: &ChatRequest, state_key: &str) -> (ChatRequest, FieldPreflight) {
    let target = target_for(router, state_key, "p0");
    let budget = RepairBudget::per_request();
    let (planned, decision, _plan) =
        router.plan_field_preflight(req, &target, DispatchSurface::Complete, &budget);
    (planned, decision)
}

/// Drive `fut` to completion on a current-thread runtime WITH timers.
///
/// `capture_events` takes a synchronous closure, so a dispatch asserted through
/// it cannot be `.await`ed. A bare futures executor is not a substitute: the
/// dispatch path arms `tokio` timeouts and a futures-only executor has no
/// reactor to fire them, so the walk hangs rather than failing.
fn block_on_dispatch<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime with timers")
        .block_on(fut)
}

/// The walk's SINGLE pre-flight record, asserting the count on the way: a
/// one-seat walk that recorded two decisions, or none, is a wiring defect that
/// an `unwrap` on the first element would hide.
fn only_record<'m>(meta: &'m super::super::DispatchMeta, why: &str) -> &'m FieldPreflight {
    assert_eq!(
        meta.field_preflight.len(),
        1,
        "{why}: expected exactly one record, got {:?}",
        meta.field_preflight,
    );
    &meta.field_preflight[0]
}

/// Every record's `(state_key, acted, reason)`, in planning order.
fn record_summary(meta: &super::super::DispatchMeta) -> Vec<(String, bool, &'static str)> {
    meta.field_preflight
        .iter()
        .map(|r| (r.state_key.clone(), r.acted, r.reason))
        .collect()
}

// ---------------------------------------------------------------------------
// The one eligible shape acts
// ---------------------------------------------------------------------------

#[test]
fn one_acknowledged_eligible_verdict_drops_the_field_and_leaves_the_original_intact() {
    // Arrange -- the only state a pre-flight rewrite may act on.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let original = req_on(ALIAS);

    // Act
    let (planned, decision) = plan(&router, &original, "m0");

    // Assert -- the planned clone lost the field; the caller's request did not.
    assert!(decision.acted, "an acknowledged eligible verdict acts");
    assert_eq!(decision.field_path, Some(GROUNDED_PATH));
    assert_eq!(decision.reason, FIELD_PREFLIGHT_ACTION_DROP);
    assert!(
        !carries_field(&planned),
        "the planned request must carry NEITHER canonical carrier of the field",
    );
    assert!(
        carries_field(&original),
        "the planner rewrites a clone: the caller's original request is untouched",
    );
    assert_eq!(
        serde_json::to_value(&planned.messages).unwrap(),
        serde_json::to_value(&original.messages).unwrap(),
        "the drop is scoped to the field: the prompt is carried through verbatim",
    );
    assert_eq!(
        planned.reasoning.as_ref().and_then(|r| r.enabled),
        Some(true),
        "the drop removes the display field, never the thinking request itself",
    );
}

// ---------------------------------------------------------------------------
// Every ambiguity falls open, with its own bounded diagnostic
// ---------------------------------------------------------------------------

#[test]
fn a_request_without_a_grounded_field_falls_open() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let original = req_without_field(ALIAS);

    let (planned, decision) = plan(&router, &original, "m0");

    assert!(!decision.acted, "nothing present, nothing to act on");
    assert_eq!(decision.field_path, None);
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NO_GROUNDED_FIELD);
    assert_eq!(
        serde_json::to_value(&planned.reasoning).unwrap(),
        serde_json::to_value(&original.reasoning).unwrap(),
        "a fail-open decision returns the request unchanged",
    );
}

#[test]
fn an_unacknowledged_acting_verdict_falls_open() {
    // A verdict may be resident and ACTING and still not be pre-flight
    // eligible, because no acknowledged confirmation backs its
    // incarnation. Live traffic can only reach this state today.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&router, "m0", false);
    let original = req_on(ALIAS);

    let (planned, decision) = plan(&router, &original, "m0");

    assert!(
        !decision.acted,
        "an acting verdict with no acknowledged confirmation must not act",
    );
    assert_eq!(decision.field_path, Some(GROUNDED_PATH));
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(
        carries_field(&planned),
        "falling open dispatches the field the client sent",
    );
}

#[test]
fn a_resident_canary_state_with_zero_confirmations_falls_open() {
    // The QUORUM boundary, distinct from the case above: there IS resident
    // canary state at the acting entry's own incarnation, so the incarnation
    // comparison succeeds and only the confirmation count refuses. Without
    // this fixture the threshold itself is unpinned -- a state carrying no
    // confirmation is indistinguishable from no state at all.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&router, "m0", false);
    let incarnation = resident_incarnation(&router, "m0");
    acknowledge(&router, "m0", incarnation, 0);
    let snapshot = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key("m0"))
        .expect("fixture premise: canary state IS resident for this key");
    assert_eq!(
        (snapshot.incarnation, snapshot.confirmations),
        (incarnation, 0),
        "fixture premise: resident at the acting incarnation, zero confirmations",
    );

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "zero confirmations is below quorum however resident the state is",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(carries_field(&planned));
}

#[test]
fn a_lapsed_verdict_falls_open() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&router, "m0", true);
    let incarnation = resident_incarnation(&router, "m0");
    acknowledge(&router, "m0", incarnation, 1);

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "a lapsed verdict is not acting, so a confirmation cannot make it eligible",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(carries_field(&planned));
}

#[test]
fn an_absent_verdict_falls_open() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(!decision.acted, "nothing resident, nothing to act on");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(carries_field(&planned));
}

#[test]
fn a_stale_confirmation_from_a_since_relearned_incarnation_falls_open() {
    // The confirmation is compared against the ACTING entry's OWN incarnation.
    // A count left over from an incarnation the entry has since moved past must
    // never be read as backing the current one.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&router, "m0", false);
    let seeded = resident_incarnation(&router, "m0");
    acknowledge(&router, "m0", seeded, 1);
    router
        .learned_capabilities
        .bump_incarnation_for_tests("m0", &grounded_key(), ANTHROPIC);
    assert_ne!(
        resident_incarnation(&router, "m0"),
        seeded,
        "fixture premise: the entry moved past the confirmed incarnation",
    );

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(!decision.acted, "a stale confirmation backs nothing");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(carries_field(&planned));
}

#[test]
fn a_non_anthropic_lane_falls_open_before_any_verdict_is_read() {
    let (router, _seen) = install(
        chain_config(ALIAS, 1, "openai-compat"),
        1,
        Answer::ServeImmediately,
    );
    plant_verdict(&router, "m0", false);
    let incarnation = resident_incarnation(&router, "m0");
    acknowledge(&router, "m0", incarnation, 1);

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(!decision.acted, "this stage acts on one lane only");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNSUPPORTED_LANE);
    assert!(carries_field(&planned));
}

#[test]
fn a_forwarded_credential_target_falls_open() {
    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    let entry = config
        .providers
        .remove("p0")
        .expect("the fixture entry exists")
        .with_credential_source(crate::config::CredentialSource::Forwarded);
    config.providers.insert("p0".to_string(), entry);
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "a target authenticating with a client credential is out of this stage's lane",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNSUPPORTED_LANE);
    assert!(carries_field(&planned));
}

#[test]
fn the_capability_kill_switch_falls_open() {
    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    config.capability.enabled = false;
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "the operator kill switch disables the pre-flight rewrite with everything else",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNSUPPORTED_LANE);
    assert!(carries_field(&planned));
}

// ---------------------------------------------------------------------------
// The planner is the same one in all three walks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_dispatches_the_planned_body_before_any_upstream_touch() {
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "the planned request serves");
    assert_eq!(
        seen.carried_per_attempt(),
        vec![false],
        "the FIRST attempt already lost the field: the rewrite is pre-flight, not a retry",
    );
    let record = only_record(&dispatched.meta, "the walk records one decision per target");
    assert!(record.acted);
    assert_eq!(
        record.state_key, "m0",
        "the record names the planned target"
    );
    assert_eq!(record.reason, FIELD_PREFLIGHT_ACTION_DROP);
    assert!(
        dispatched.meta.field_repair.is_none(),
        "a pre-flight rewrite is not a reactive repair and must not be recorded as one",
    );
}

#[tokio::test]
async fn stream_dispatches_the_planned_body_before_any_upstream_touch() {
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let dispatched = router
        .stream_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "the planned stream opens");
    assert_eq!(
        seen.carried_per_attempt(),
        vec![false],
        "the streaming walk plans at the same seam the completion walk does",
    );
    let record = only_record(
        &dispatched.meta,
        "the streaming walk records its decision too",
    );
    assert!(record.acted);
    assert_eq!(record.reason, FIELD_PREFLIGHT_ACTION_DROP);
}

#[tokio::test]
async fn count_tokens_counts_the_planned_body() {
    // The count is a number for the body that would be SENT. A walk that
    // planned the repair and then counted the unplanned request would return a
    // count for a request nothing dispatches.
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;

    assert!(counted.result.is_ok(), "the planned count serves");
    assert_eq!(seen.calls(), 1, "one seat, one count");
    assert!(
        !carries_field(&seen.attempt(0)),
        "the body whose tokens were counted is the PLANNED one",
    );
    let record = only_record(
        &counted.meta,
        "the token-count walk records its decision too",
    );
    assert!(record.acted);
    assert_eq!(record.reason, FIELD_PREFLIGHT_ACTION_DROP);
}

#[tokio::test]
async fn all_three_walks_produce_the_same_decision_from_one_registry_state() {
    // One router, one planted state, three walks. The planner is READ-ONLY on
    // the verdict and spends no budget, so the three decisions must be
    // identical -- a surface that consumed eligibility would make the second
    // and third walks diverge.
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let completed = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let streamed = router
        .stream_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;

    let decisions: Vec<(bool, Option<&'static str>, &'static str)> =
        [&completed.meta, &streamed.meta, &counted.meta]
            .into_iter()
            .map(|meta| {
                let record = only_record(meta, "every walk records a decision");
                (record.acted, record.field_path, record.reason)
            })
            .collect();

    assert_eq!(
        decisions,
        vec![
            (true, Some(GROUNDED_PATH), FIELD_PREFLIGHT_ACTION_DROP),
            (true, Some(GROUNDED_PATH), FIELD_PREFLIGHT_ACTION_DROP),
            (true, Some(GROUNDED_PATH), FIELD_PREFLIGHT_ACTION_DROP),
        ],
        "one planner, one decision, three walks",
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![false, false, false],
        "and all three dispatched the planned body",
    );
}

// ---------------------------------------------------------------------------
// Each fallback target plans from its own base
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fallback_target_plans_from_the_original_request_not_a_siblings_clone() {
    // Seat 0 is ineligible and fails with an unattributed 503; seat 1 is
    // eligible. If the walk carried seat 0's planned clone forward, seat 1's
    // plan would start from an already-rewritten body -- and if it carried
    // seat 1's rewrite backward, seat 0 would have dispatched without the
    // field. The two dispatched bodies are what separates those cases.
    let (router, seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m1");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "premise: both seats refuse, so the walk visits both",
    );
    assert!(
        seen[0].carried_per_attempt().iter().all(|carried| *carried),
        "every attempt on the ineligible seat dispatched the ORIGINAL body, field intact: {:?}",
        seen[0].carried_per_attempt(),
    );
    assert!(
        seen[1]
            .carried_per_attempt()
            .iter()
            .all(|carried| !*carried),
        "every attempt on the eligible seat planned its own rewrite from that same original body: {:?}",
        seen[1].carried_per_attempt(),
    );
    assert_eq!(
        serde_json::to_value(&seen[0].attempt(0).messages).unwrap(),
        serde_json::to_value(&seen[1].attempt(0).messages).unwrap(),
        "both targets planned from ONE original request",
    );
}

#[tokio::test]
async fn an_eligible_first_target_leaks_nothing_onto_an_ineligible_fallback() {
    // The mirror of the case above: the rewrite must not persist across the
    // chain hop either.
    let (router, seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: both seats refuse");
    assert!(
        seen[0]
            .carried_per_attempt()
            .iter()
            .all(|carried| !*carried),
        "every attempt on the eligible seat dropped the field for its own attempt: {:?}",
        seen[0].carried_per_attempt(),
    );
    assert!(
        seen[1].carried_per_attempt().iter().all(|carried| *carried),
        "every attempt on the ineligible fallback plans from the original request, which still carries it: {:?}",
        seen[1].carried_per_attempt(),
    );
}

// ---------------------------------------------------------------------------
// The reactive path is untouched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_rewrite_spends_no_reactive_repair_budget() {
    // Pre-flight acts on a verdict that is already resident and settled, so it
    // draws no repair budget and claims no single-flight slot. A draw here
    // would silently halve what the reactive arm may spend later in the same
    // request.
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(seen.calls(), 1, "no retry: the first attempt served");
    assert!(
        dispatched.meta.field_repair.is_none(),
        "no reactive repair record, so no budget draw and no settlement",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "a pre-flight rewrite learns nothing: it acts on what is already learned",
    );
    assert!(
        router.field_verdicts().preflight_eligible(
            &verdict_key("m0"),
            router.registry_generation(),
            Instant::now(),
        ),
        "and the verdict it read is still eligible afterwards -- the read consumes nothing",
    );
}

// ---------------------------------------------------------------------------
// The eligibility read is consistent under a concurrent state change
// ---------------------------------------------------------------------------

#[test]
fn a_verdict_that_moves_between_the_two_eligibility_reads_is_refused() {
    // The race this pins is invisible from outside the predicate: the acting
    // verdict and the canary confirmation take different locks, so a clear or
    // relearn landing BETWEEN them yields an authorization assembled from two
    // states that never coexisted. Spawning threads and hoping to hit that
    // window proves nothing on a green run, so the mutation is landed FROM
    // INSIDE it through the module's own interposition seam -- which makes the
    // interleaving deterministic and the failure reproducible every run.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let key = verdict_key("m0");
    let generation = router.registry_generation();
    assert!(
        router
            .field_verdicts()
            .preflight_eligible(&key, generation, Instant::now()),
        "premise: eligible with nothing interposed",
    );

    // Interpose the state change: the confirmation has already been read and
    // matched at this point, and the entry moves to a new incarnation before
    // the re-read.
    let learned = Arc::clone(&router.learned_capabilities);
    let grounded = grounded_key();
    let landed = Arc::new(AtomicUsize::new(0));
    let landed_in_hook = Arc::clone(&landed);
    let _interposed = crate::field_verdict::eligibility_interpose::install(move || {
        landed_in_hook.fetch_add(1, Ordering::SeqCst);
        learned.bump_incarnation_for_tests("m0", &grounded, ANTHROPIC);
    });

    let eligible = router
        .field_verdicts()
        .preflight_eligible(&key, generation, Instant::now());

    assert_eq!(
        landed.load(Ordering::SeqCst),
        1,
        "premise: the interposition ran, so the window under test was reached",
    );
    assert!(
        !eligible,
        "a confirmation whose incarnation the entry has already left is not authorization",
    );
}

#[tokio::test]
async fn a_walk_whose_verdict_moves_mid_eligibility_read_dispatches_the_original_field() {
    // The same race, observed where it matters: through a real dispatch, on
    // the bytes that reach the upstream. The refusal must fail OPEN -- the
    // field the client sent goes out unchanged -- rather than dispatching a
    // half-planned body.
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let learned = Arc::clone(&router.learned_capabilities);
    let grounded = grounded_key();
    let _interposed = crate::field_verdict::eligibility_interpose::install(move || {
        learned.bump_incarnation_for_tests("m0", &grounded, ANTHROPIC);
    });

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true],
        "the walk fell open: the client's field was dispatched unchanged",
    );
    let record = only_record(&dispatched.meta, "the walk still records its decision");
    assert!(!record.acted);
    assert_eq!(record.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
}

#[test]
fn the_eligibility_read_stays_consistent_under_hostile_concurrency() {
    // The barrier case above is the deterministic pin. This is the companion
    // stress: many threads reading the same identity while a writer churns its
    // incarnation. The property asserted is not a count -- a racing writer
    // makes the outcome legitimately either way -- but that no read ever
    // authorizes while reporting a state the entry does not hold. A read that
    // returns true must be backed by a confirmation at the CURRENT acting
    // incarnation, checked immediately after.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let router = Arc::new(router);
    let key = verdict_key("m0");
    let generation = router.registry_generation();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let churn = {
        let router = Arc::clone(&router);
        let stop = Arc::clone(&stop);
        let grounded = grounded_key();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                router
                    .learned_capabilities
                    .bump_incarnation_for_tests("m0", &grounded, ANTHROPIC);
            }
        })
    };

    let readers: Vec<_> = (0..64)
        .map(|_| {
            let router = Arc::clone(&router);
            let key = key.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if router
                        .field_verdicts()
                        .preflight_eligible(&key, generation, Instant::now())
                    {
                        // An authorizing read must rest on a confirmation for
                        // an incarnation the entry actually holds. Reading the
                        // pair back here can only ever be at least as stale as
                        // the decision, so a mismatch means the decision was
                        // assembled from states that never coexisted.
                        let snap = router.field_verdicts().canaries().snapshot(&key);
                        assert!(
                            snap.is_some_and(|s| s.confirmations >= 1),
                            "an authorizing read must be backed by a resident confirmation",
                        );
                    }
                }
            })
        })
        .collect();

    for reader in readers {
        reader.join().expect("reader thread");
    }
    stop.store(true, Ordering::Relaxed);
    churn.join().expect("churn thread");
}

// ---------------------------------------------------------------------------
// The operator force_supported mask is honored before any learned action
// ---------------------------------------------------------------------------

/// `chain_config` plus a `force_supported` override for the grounded field's
/// capability key, at `target_spec` (`p0` for the provider tier, `p0:m0` for
/// the model tier).
fn config_with_force_supported(target_spec: &str) -> Config {
    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    config.capability.overrides.insert(
        target_spec.to_string(),
        crate::config::OverrideEntry {
            unsupported: Vec::new(),
            force_supported: vec![grounded_key()],
        },
    );
    config
}

#[test]
fn a_force_supported_override_masks_the_learned_verdict() {
    // The operator has said to send this field. A learned verdict, however
    // well confirmed, does not overrule that -- and the refusal carries its
    // own token so the mask is visible rather than looking like a plain
    // ineligibility.
    let (router, _seen) = install(
        config_with_force_supported("p0"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "an operator force_supported override outranks a learned verdict",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
    assert!(
        carries_field(&planned),
        "the masked field is dispatched, which is what force_supported means",
    );
}

#[test]
fn a_model_tier_force_supported_override_masks_too() {
    // The mask resolves through the SAME two-tier resolver the act and learn
    // sides use, so the model-scoped spelling must mask as well -- otherwise a
    // mask is honored at one tier and missed at the other.
    let (router, _seen) = install(
        config_with_force_supported("p0:m0"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(!decision.acted, "the model-tier cell masks as well");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
}

#[tokio::test]
async fn all_three_walks_honor_the_force_supported_mask() {
    let (router, mut observed) = install(
        config_with_force_supported("p0"),
        1,
        Answer::ServeImmediately,
    );
    let seen = observed.remove(0);
    plant_eligible(&router, "m0");

    let completed = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let streamed = router
        .stream_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;

    assert!(completed.result.is_ok() && streamed.result.is_ok() && counted.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, true, true],
        "every walk dispatched the masked field: the mask is not per-surface",
    );
    for meta in [&completed.meta, &streamed.meta, &counted.meta] {
        let record = only_record(meta, "every walk records its masked decision");
        assert!(!record.acted);
        assert_eq!(record.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
    }
}

// ---------------------------------------------------------------------------
// Attribution guards mirror the reactive admission
// ---------------------------------------------------------------------------

#[test]
fn a_loopback_target_falls_open_as_unattributable() {
    // A local hop is a target whose rejection this stage has decided it cannot
    // attribute to an upstream. A verdict it must not LEARN from is a verdict
    // it must not ACT on proactively either.
    let (router, _seen) = install(
        config_with_base_url("http://127.0.0.1:8899"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        !decision.acted,
        "a local-hop target is outside this stage's attribution",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
    assert!(carries_field(&planned));
}

#[test]
fn a_localhost_named_target_falls_open_as_unattributable() {
    // The suppression predicate is syntactic over several spellings; the
    // planner must consult it rather than pattern-match an address itself.
    let (router, _seen) = install(
        config_with_base_url("https://localhost:9100"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
}

#[cfg(feature = "bedrock")]
#[test]
fn a_bedrock_mantle_target_falls_open_as_unattributable() {
    // The mantle lane reports NO attributable Anthropic API base URL (its
    // accessor answers None by construction), which is exactly how the
    // reactive admission refuses it. Asserting through the same accessor keeps
    // the two refusals resting on one fact rather than two lookalike checks.
    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    let mut entry = config
        .providers
        .remove("p0")
        .expect("the fixture entry exists");
    if let crate::config::ProviderEntry::AnthropicApi { bedrock_mantle, .. } = &mut entry {
        *bedrock_mantle = Some(crate::config::BedrockMantleConfig {
            region: "us-west-2".to_string(),
            creds: crate::config::BedrockCredsConfig::BearerKey {
                key_ref: crate::test_secret::file_ref("mantle-bearer-key"),
            },
        });
    }
    assert!(
        crate::config::ProviderEntry::anthropic_api_base_url(&entry).is_none(),
        "fixture premise: a mantle entry carries no attributable anthropic-api base URL",
    );
    config.providers.insert("p0".to_string(), entry);
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(!decision.acted, "the mantle lane is not this stage's lane");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
    assert!(carries_field(&planned));
}

// ---------------------------------------------------------------------------
// Fail-open is byte-safe
// ---------------------------------------------------------------------------

#[test]
fn a_partially_mutating_transform_returns_a_byte_equivalent_original() {
    // THE byte-safety pin. `drop_from` mutates in place, so a transform applied
    // to the request the planner is about to RETURN leaves a half-mutated body
    // behind on its false branch -- reporting "did nothing" while having
    // removed a carrier.
    //
    // The surface driven here is the one that reproduces that: it drops a
    // carrier and THEN returns false. The divergent surface cannot pin this,
    // because it removes nothing, so an in-place transform and a scratch clone
    // are indistinguishable under it -- measured, by reverting the scratch
    // clone and watching every assertion stay green.
    let original = req_on(ALIAS);
    assert!(
        original
            .routectl_internal
            .anthropic_thinking_display
            .is_some(),
        "fixture premise: the carrier the partial drop removes IS present, so a \
         leaked mutation would be observable",
    );

    let (planned, decision) = super::plan_transform_for_tests(
        &original,
        super::super::field_repair::FieldSurface::PartialDropForTests,
        "m0",
    );

    assert!(
        !decision.acted,
        "a transform reporting failure must not claim an action",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_AMBIGUOUS_MUTATION);
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        serde_json::to_value(&original).unwrap(),
        "a fail-open decision returns bytes identical to the original request",
    );
    assert!(
        planned
            .routectl_internal
            .anthropic_thinking_display
            .is_some(),
        "and specifically the carrier the failed transform removed is still there",
    );
}

// The divergent surface (removes nothing, reports present) exercises the OTHER
// ambiguous arm. Release-only, matching where that variant is compiled.
#[cfg(not(debug_assertions))]
#[test]
fn a_transform_that_removes_nothing_is_ambiguous_too() {
    let original = req_on(ALIAS);

    let (planned, decision) = super::plan_transform_for_tests(
        &original,
        super::super::field_repair::FieldSurface::DivergentForTests,
        "m0",
    );

    assert!(!decision.acted);
    assert_eq!(decision.reason, FIELD_PREFLIGHT_AMBIGUOUS_MUTATION);
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        serde_json::to_value(&original).unwrap(),
    );
}

// The same entry point on the GROUNDED surface, so the cases above are not the
// only exercise of it: a helper that returned `ambiguous()` unconditionally
// would satisfy every failure case while pinning nothing.
#[test]
fn the_transform_entry_point_still_acts_on_the_grounded_surface() {
    let original = req_on(ALIAS);

    let (planned, decision) = super::plan_transform_for_tests(
        &original,
        super::super::field_repair::FieldSurface::AnthropicThinkingDisplay,
        "m0",
    );

    assert!(decision.acted, "the grounded surface drops and adopts");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_ACTION_DROP);
    assert!(!carries_field(&planned));
    assert!(carries_field(&original), "the original is untouched");
}

#[test]
fn every_fail_open_arm_returns_a_serialized_equal_original() {
    // One assertion per refusal arm, over the WHOLE serialized request rather
    // than the one field each arm happens to be about: a fail-open contract is
    // about the bytes, and an arm that perturbed something unrelated (a model
    // rewrite, an overlay, a dropped carrier) would satisfy a field-scoped
    // check while breaking the contract.
    let original = req_on(ALIAS);
    let bare = req_without_field(ALIAS);

    let mut cases: Vec<(&str, Router, ChatRequest, &'static str)> = Vec::new();

    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    cases.push((
        "no grounded field",
        router,
        bare.clone(),
        FIELD_PREFLIGHT_NO_GROUNDED_FIELD,
    ));

    let (router, _seen) = single_seat(Answer::ServeImmediately);
    cases.push((
        "absent verdict",
        router,
        original.clone(),
        FIELD_PREFLIGHT_NOT_ELIGIBLE,
    ));

    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&router, "m0", true);
    cases.push((
        "lapsed verdict",
        router,
        original.clone(),
        FIELD_PREFLIGHT_NOT_ELIGIBLE,
    ));

    let (router, _seen) = install(
        chain_config(ALIAS, 1, "openai-compat"),
        1,
        Answer::ServeImmediately,
    );
    cases.push((
        "wrong lane",
        router,
        original.clone(),
        FIELD_PREFLIGHT_UNSUPPORTED_LANE,
    ));

    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    config.capability.enabled = false;
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    cases.push((
        "kill switch",
        router,
        original.clone(),
        FIELD_PREFLIGHT_UNSUPPORTED_LANE,
    ));

    let (router, _seen) = install(
        config_with_force_supported("p0"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");
    cases.push((
        "force_supported mask",
        router,
        original.clone(),
        FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
    ));

    let (router, _seen) = install(
        config_with_base_url("http://127.0.0.1:8899"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");
    cases.push((
        "unattributable target",
        router,
        original.clone(),
        FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET,
    ));

    assert_eq!(
        cases.len(),
        7,
        "every refusal arm reachable here is covered"
    );
    for (label, router, req, expected_reason) in cases {
        let (planned, decision) = plan(&router, &req, "m0");
        assert!(!decision.acted, "{label}: must not act");
        assert_eq!(decision.reason, expected_reason, "{label}: reason token");
        assert_eq!(
            serde_json::to_value(&planned).unwrap(),
            serde_json::to_value(&req).unwrap(),
            "{label}: a fail-open decision returns the original request byte-for-byte",
        );
    }
}

// ---------------------------------------------------------------------------
// Diagnostics survive a fallback chain, and stay content-free
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fallback_chain_records_one_decision_per_target() {
    // A single record slot reports only the LAST target, which discards
    // exactly the fallback behavior an operator needs: seat 0 acted, seat 1
    // did not. Both must survive, in planning order, each attributable to its
    // own target.
    let (router, _seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: both seats refuse");
    // Planning ORDER is the chain's order, and an acting field verdict demotes
    // its own target to the tail through the capability pre-filter's
    // learned-demotion partition -- so the eligible seat is visited SECOND.
    // Asserting the order the seats were CONFIGURED in would pin a chain the
    // router deliberately does not dispatch.
    let summary = record_summary(&dispatched.meta);
    assert_eq!(
        summary,
        vec![
            ("m1".to_string(), false, FIELD_PREFLIGHT_NOT_ELIGIBLE),
            ("m0".to_string(), true, FIELD_PREFLIGHT_ACTION_DROP),
        ],
        "both targets' decisions survive, in planning order, each named",
    );
}

/// Capture this request's diagnostics, split into the routine per-decision
/// DEBUG lines and the request-level WARN(s).
fn captured_diagnostics(
    events: &[routectl_testkit::CapturedEvent],
) -> (
    Vec<&routectl_testkit::CapturedEvent>,
    Vec<&routectl_testkit::CapturedEvent>,
) {
    let debugs = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::DEBUG && e.message == "envelope-field pre-flight decision"
        })
        .collect();
    let warns = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN && e.message == super::FIELD_PREFLIGHT_WARN_MESSAGE
        })
        .collect();
    (debugs, warns)
}

/// A field's rendered value on one captured event.
fn field_of<'e>(event: &'e routectl_testkit::CapturedEvent, name: &str) -> Option<&'e str> {
    event
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn one_acting_target_emits_one_debug_and_exactly_one_request_warn() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 1, "one decision, one DEBUG line");
    assert_eq!(
        warns.len(),
        1,
        "a request that rewrote an envelope earns exactly ONE request-level WARN",
    );
    assert_eq!(field_of(warns[0], "targets_acted"), Some("1"));
    assert_eq!(field_of(warns[0], "targets_planned"), Some("1"));
    assert_eq!(
        field_of(warns[0], "action"),
        Some(FIELD_PREFLIGHT_ACTION_DROP),
    );
}

#[test]
fn a_quiet_request_emits_debug_decisions_and_no_warn() {
    // The routine case: not eligible, so nothing was rewritten. The decision
    // is still RETAINED and still reported at DEBUG, but a request that
    // changed nothing must not look faulty in an operator's WARN stream.
    let (router, _seen) = single_seat(Answer::ServeImmediately);

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 1, "the decision is still reported at DEBUG");
    assert_eq!(
        field_of(debugs[0], "reason"),
        Some(FIELD_PREFLIGHT_NOT_ELIGIBLE),
    );
    assert!(
        warns.is_empty(),
        "a request that rewrote nothing earns no WARN: {warns:#?}",
    );
}

#[test]
fn a_non_acting_decision_is_never_labelled_with_the_drop_action() {
    // Labelling a fail-open `field_preflight_drop` would name an action the
    // walk did not take -- a false claim on the surface built to make such
    // claims trustworthy. The non-acting tier carries the reason instead.
    let (router, _seen) = single_seat(Answer::ServeImmediately);

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, _warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 1);
    assert_eq!(field_of(debugs[0], "acted"), Some("false"));
    assert_eq!(
        field_of(debugs[0], "action"),
        None,
        "a non-acting decision carries NO action field at all",
    );
    // Positive control: the acting case DOES carry it, so the assertion above
    // is about the non-acting tier rather than about the field never existing.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let acting_events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });
    let (acting_debugs, _) = captured_diagnostics(&acting_events);
    assert_eq!(acting_debugs.len(), 1);
    assert_eq!(
        field_of(acting_debugs[0], "action"),
        Some(FIELD_PREFLIGHT_ACTION_DROP),
        "control: an ACTING decision does carry the drop action",
    );
}

#[test]
fn a_fallback_chain_emits_one_debug_per_target_and_still_one_warn() {
    // The aggregation contract across a chain: every planned target reports at
    // DEBUG (so the per-target detail an operator needs survives), while the
    // request still earns exactly one WARN.
    let (router, _seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 2, "one DEBUG per planned target");
    assert_eq!(
        warns.len(),
        1,
        "one WARN per REQUEST, not per acting target: {warns:#?}",
    );
    assert_eq!(field_of(warns[0], "targets_acted"), Some("1"));
    assert_eq!(field_of(warns[0], "targets_planned"), Some("2"));
}

#[test]
fn the_preflight_diagnostic_carries_no_request_or_response_content() {
    // The record's fields are closed-set by construction, but a diagnostic is
    // where a content leak actually lands, so the emitted LINES are asserted --
    // against sentinels planted in every caller-controlled slot the request
    // carries. Both tiers are captured, so a leak in the quiet tier cannot hide.
    const PROMPT_SENTINEL: &str = "sentinel-prompt-must-not-be-logged";
    const DISPLAY_SENTINEL: &str = "sentinel-display-value";
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let mut req = req_on(ALIAS);
    req.messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(PROMPT_SENTINEL.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }]
    .into();
    req.routectl_internal.anthropic_thinking_display = Some(DISPLAY_SENTINEL.into());

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req, RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    // Premise: the sentinels are values the request genuinely carried, so a
    // leaking emitter WOULD have them to print. Without this the assertions
    // below could pass against a request that never held them.
    assert_eq!(debugs.len(), 1, "premise: the DEBUG tier fired");
    assert_eq!(warns.len(), 1, "premise: the WARN tier fired");
    let rendered = format!("{events:#?}");
    for sentinel in [PROMPT_SENTINEL, DISPLAY_SENTINEL] {
        assert!(
            !rendered.contains(sentinel),
            "a pre-flight diagnostic must never carry a request value: {sentinel}",
        );
    }
}

#[test]
fn the_state_key_is_sanitized_on_every_record() {
    // A state key is operator-controlled config text, so it reaches the record
    // through `sanitize_for_log` -- newlines and control bytes would otherwise
    // let a crafted name forge log lines. The key is the resolved model's
    // NICKNAME (`chain.rs` builds it from `m.nickname`), measured rather than
    // assumed: the first version of this fixture planted the bytes on the
    // provider name and the premise assertion below caught it.
    const HOSTILE: &str = "m0\nforged=line\u{1b}[31m";
    let (mut router, _seen) = chain_of(1, Answer::ServeImmediately);
    let provider: Arc<dyn Provider> = Arc::new(MockSeat::new(
        Answer::ServeImmediately,
        Arc::new(Observed::default()),
    ));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        HOSTILE.to_string(),
        Arc::new(ResolvedModel::new(HOSTILE, "p0", provider, "wire-0")),
    );
    router.install_resolved_models(models);

    let target = target_for(&router, HOSTILE, "p0");
    assert!(
        target.state_key.contains('\n') && target.state_key.contains('\u{1b}'),
        "fixture premise: the RAW target state key carries a newline and an ANSI \
         escape, so a missing sanitizer would be observable: {:?}",
        target.state_key,
    );
    let budget = RepairBudget::per_request();

    let (_planned, decision, _plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete, &budget);

    // This record is a NON-acting one (no verdict planted), which is the tier
    // built at the `unchanged` site.
    assert!(
        !decision.acted,
        "premise: this exercises the fail-open site"
    );
    assert!(
        !decision.state_key.contains('\n'),
        "the recorded state key must carry no newline: {:?}",
        decision.state_key,
    );
    assert!(
        !decision.state_key.contains('\u{1b}'),
        "nor an ANSI escape: {:?}",
        decision.state_key,
    );
    assert_eq!(
        decision.state_key,
        routectl_core::sanitize_for_log(&target.state_key),
        "and it equals exactly what the shared sanitizer produces",
    );

    // The ACTING site builds its record separately, so it needs its own
    // assertion -- a sanitizer on one site and not the other is exactly the
    // drift this pins.
    plant_eligible(&router, HOSTILE);
    let (_planned, acting, _acting_plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete, &budget);
    assert!(acting.acted, "premise: the acting site is now reached");
    assert!(!acting.state_key.contains('\n'));
    assert!(!acting.state_key.contains('\u{1b}'));
    assert_eq!(
        acting.state_key,
        routectl_core::sanitize_for_log(&target.state_key),
    );
}

#[test]
fn the_test_transform_helper_sanitizes_its_state_key_too() {
    // The test driver builds records at its own site, so it must sanitize as
    // well -- an unsanitized test helper would let a future assertion pass
    // against bytes production would never emit.
    let (_planned, decision) = super::plan_transform_for_tests(
        &req_on(ALIAS),
        super::super::field_repair::FieldSurface::AnthropicThinkingDisplay,
        "m0\nforged",
    );

    assert!(!decision.state_key.contains('\n'));
    assert_eq!(
        decision.state_key,
        routectl_core::sanitize_for_log("m0\nforged"),
    );
}

// ---------------------------------------------------------------------------
// A stale router generation authorizes nothing
// ---------------------------------------------------------------------------

#[test]
fn a_field_verdict_stays_eligible_across_a_generation_boundary() {
    // A `field:` capability key is catalog-INDEPENDENT by construction, so the
    // generation barrier admits it from ANY generation -- an upstream statement
    // about its own request envelope is not invalidated by a catalog revision.
    // This test asserts that DESIGNED property rather than a staleness refusal:
    // measuring the real behavior showed a post-advance read still authorizes,
    // and the barrier's own docs say that is deliberate for this key class.
    // The generation is still threaded (it is what the registry validates), so
    // the read is not generation-blind -- it is generation-TOLERANT for this
    // key class only.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let key = verdict_key("m0");
    let live = router.registry_generation();
    assert!(
        router
            .field_verdicts()
            .preflight_eligible(&key, live, Instant::now()),
        "premise: eligible under the live generation",
    );

    let advanced = router.learned_capabilities.advance_generation();
    assert!(
        advanced > live,
        "premise: the registry generation moved forward",
    );

    assert!(
        router
            .field_verdicts()
            .preflight_eligible(&key, live, Instant::now()),
        "a catalog-independent field verdict survives a generation boundary",
    );
    assert!(
        router
            .field_verdicts()
            .preflight_eligible(&key, advanced, Instant::now()),
        "and reads under the new generation authorize identically",
    );
}

#[tokio::test]
async fn a_generation_boundary_mid_walk_does_not_change_the_planned_body() {
    // The dispatch-level companion to the case above. A reload advancing the
    // registry generation between the two seats must leave the field verdict's
    // authority intact, because the key class is catalog-independent -- so the
    // eligible seat still plans its rewrite and the ineligible one still
    // dispatches the client's field.
    let (router, seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");
    router.learned_capabilities.advance_generation();

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: both seats refuse");
    assert!(
        seen[1].carried_per_attempt().iter().all(|carried| *carried),
        "the ineligible seat dispatched the client's field: {:?}",
        seen[1].carried_per_attempt(),
    );
    assert!(
        seen[0]
            .carried_per_attempt()
            .iter()
            .all(|carried| !*carried),
        "the eligible seat still acted across the boundary: {:?}",
        seen[0].carried_per_attempt(),
    );
    let summary = record_summary(&dispatched.meta);
    assert_eq!(
        summary.iter().filter(|(_, acted, _)| *acted).count(),
        1,
        "exactly one target acted, across the boundary: {summary:?}",
    );
}

#[tokio::test]
async fn every_fallback_target_dispatches_a_serialized_equal_original() {
    // The strongest form of the no-leak-across-fallbacks property: compare the
    // WHOLE serialized request each ineligible seat dispatched against the
    // whole original, not just the grounded field. A sibling's rewrite leaking
    // forward, or any unrelated perturbation from planning, fails here while a
    // field-scoped check would pass.
    let (router, seen) = chain_of(3, Answer::Unavailable);
    plant_eligible(&router, "m1");
    let original = req_on(ALIAS);

    let dispatched = router
        .complete_with_options(original.clone(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: every seat refuses");
    // Two fields are rewritten per target by the WALK, downstream of the
    // planner and independently of it: `model` (rewritten to the seat's
    // upstream id) and `cache_control` (stamped by the auto-cache placement
    // that runs after the pre-flight seam). Both were measured, not assumed --
    // the first version of this assertion failed on `cache_control` and reading
    // the diff is what identified it. They are normalized out because they are
    // not the planner's output; EVERY other field must match the original, so a
    // sibling's rewrite leaking forward, or any perturbation the planner
    // introduced, still fails here.
    let normalize = |req: &ChatRequest| {
        let mut req = req.clone();
        req.model = String::new();
        req.cache_control = None;
        serde_json::to_value(&req).expect("a canonical request serializes")
    };
    let expected = normalize(&original);
    // The normalization must not be what makes the comparison pass: the
    // grounded field is NOT normalized away, so an eligible seat's body still
    // differs under it. Asserted below, which is what keeps the equality
    // checks from being vacuous.
    let ineligible = [0usize, 2];
    for idx in ineligible {
        assert_eq!(
            normalize(&seen[idx].attempt(0)),
            expected,
            "ineligible seat {idx} dispatched a request equal to the original in every field \
             the planner owns",
        );
    }
    assert_ne!(
        normalize(&seen[1].attempt(0)),
        expected,
        "the eligible seat's body must still DIFFER under the same normalization, or the \
         comparisons above would pass over three identical bodies",
    );
    assert!(
        !carries_field(&seen[1].attempt(0)),
        "and it differs by exactly the grounded field",
    );
}

#[tokio::test]
async fn an_acting_target_planned_first_leaks_nothing_onto_a_later_target() {
    // THE leak-forward pin, and it needs a carefully built chain to exist at
    // all. An acting verdict DEMOTES its own target to the chain tail through
    // the capability pre-filter, so in every ordinary fixture the eligible
    // target is planned LAST -- and a walk that carried a planned clone forward
    // would have nothing after it to leak onto. Measured: mutating the walk to
    // carry the clone forward left the whole suite green for exactly that
    // reason.
    //
    // The fix is to make BOTH targets acting (so both are demoted and their
    // relative order is preserved by the stable partition) while only the FIRST
    // is confirmed. Seat 0 then acts, seat 1 falls open, and seat 1 is planned
    // after seat 0 -- which is the ordering the leak needs.
    let (router, seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");
    plant_verdict(&router, "m1", false);
    let original = req_on(ALIAS);

    let dispatched = router
        .complete_with_options(original.clone(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise: both seats refuse");
    let summary = record_summary(&dispatched.meta);
    assert_eq!(
        summary,
        vec![
            ("m0".to_string(), true, FIELD_PREFLIGHT_ACTION_DROP),
            ("m1".to_string(), false, FIELD_PREFLIGHT_NOT_ELIGIBLE),
        ],
        "premise: the ACTING target is planned FIRST, so a carried-forward \
         clone would have a later target to leak onto",
    );
    assert!(
        seen[0]
            .carried_per_attempt()
            .iter()
            .all(|carried| !*carried),
        "seat 0 acted on its own plan: {:?}",
        seen[0].carried_per_attempt(),
    );
    assert!(
        seen[1].carried_per_attempt().iter().all(|carried| *carried),
        "seat 1 fell open and must dispatch the CLIENT's field -- a planned \
         clone carried forward from seat 0 would have stripped it: {:?}",
        seen[1].carried_per_attempt(),
    );
    // And the whole body, not just the one field: seat 1's request must equal
    // the original in every field the planner owns.
    let normalize = |req: &ChatRequest| {
        let mut req = req.clone();
        req.model = String::new();
        req.cache_control = None;
        serde_json::to_value(&req).expect("a canonical request serializes")
    };
    assert_eq!(
        normalize(&seen[1].attempt(0)),
        normalize(&original),
        "the later target planned from the ORIGINAL request, not a sibling's clone",
    );
}

#[test]
fn a_transform_claiming_removal_while_the_field_remains_is_ambiguous() {
    // THE post-condition pin. `PartialDropForTests` fails the `drop_from`
    // check and never reaches the post-condition; the grounded surface passes
    // both. This surface is the only one that reaches it: it reports a
    // SUCCESSFUL removal while its presence predicate still reports present.
    //
    // Without the re-check, the planner would adopt that body and record
    // `field_preflight_drop` for a request that still emits the field to the
    // upstream -- a false claim on the exact surface built to make such claims
    // trustworthy.
    let original = req_on(ALIAS);

    let (planned, decision) = super::plan_transform_for_tests(
        &original,
        super::super::field_repair::FieldSurface::ClaimsRemovalForTests,
        "m0",
    );

    assert!(
        !decision.acted,
        "a claimed removal whose field is still present must not be reported as an action",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_AMBIGUOUS_MUTATION);
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        serde_json::to_value(&original).unwrap(),
        "and the returned request is byte-equal to the original",
    );
}
