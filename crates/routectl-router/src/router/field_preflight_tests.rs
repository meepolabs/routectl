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
    FIELD_PREFLIGHT_ACTION_DROP, FIELD_PREFLIGHT_CANARY_RESTORED,
    FIELD_PREFLIGHT_MASKED_BY_OVERRIDE, FIELD_PREFLIGHT_NO_GROUNDED_FIELD,
    FIELD_PREFLIGHT_NOT_ELIGIBLE, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET,
    FIELD_PREFLIGHT_UNSUPPORTED_LANE,
};

use crate::config::{AliasValue, CANARY_INTERVAL, Config};
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

/// Plan EVERY decision for `state_key`'s target, from `req`.
///
/// The returned plan is DROPPED immediately, which is correct for every case
/// here: these tests assert the decisions, and a canary plan's own settlement is
/// the subject of `field_canary_settlement_tests` instead. A cadence that came
/// due inside one of these calls therefore settles inconclusive on the drop,
/// which moves no verdict.
fn plan_all(
    router: &Router,
    req: &ChatRequest,
    state_key: &str,
) -> (ChatRequest, Vec<FieldPreflight>) {
    let target = target_for(router, state_key, "p0");
    let (planned, records, _plan) =
        router.plan_field_preflight(req, &target, DispatchSurface::Complete);
    (planned, records)
}

/// Plan for a request carrying exactly ONE closed-table row, returning that
/// row's single decision.
///
/// The count is ASSERTED rather than assumed: a fixture that grew a second
/// present row would otherwise have its later decision silently dropped by an
/// index-zero read, and the assertion about "the" decision would then be about
/// whichever row happened to come first in the table.
fn plan(router: &Router, req: &ChatRequest, state_key: &str) -> (ChatRequest, FieldPreflight) {
    let (planned, mut records) = plan_all(router, req, state_key);
    assert_eq!(
        records.len(),
        1,
        "this fixture carries one closed-table row, so it must produce one decision: {records:?}",
    );
    (planned, records.remove(0))
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

/// The walk's SINGLE pre-flight record, asserting the count on the way.
///
/// For a one-seat walk over a fixture carrying ONE closed-table row -- which is
/// the count's actual basis, since the record unit is a row per target rather
/// than a target. A walk that recorded two decisions where one row was present,
/// or none, is a wiring defect that an `unwrap` on the first element would hide.
fn only_record<'m>(meta: &'m super::super::DispatchMeta, why: &str) -> &'m FieldPreflight {
    assert_eq!(
        meta.field_preflight.len(),
        1,
        "{why}: expected exactly one record, got {:?}",
        meta.field_preflight,
    );
    &meta.field_preflight[0]
}

/// The single decision in `records`, asserting the count on the way -- see
/// [`plan`] for why a bare index-zero read is not a substitute.
fn only_decision<'r>(records: &'r [FieldPreflight], why: &str) -> &'r FieldPreflight {
    assert_eq!(
        records.len(),
        1,
        "{why}: expected exactly one decision, got {records:?}",
    );
    &records[0]
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
        "a target authenticating with a client credential is one whose rejections \
         this stage cannot attribute to a routectl-owned seat",
    );
    // Reported as UNATTRIBUTABLE rather than off-lane: the forwarded refusal
    // lives in the shared attributability decision, which every field stage
    // consults, rather than in a per-stage lane check that could drift from it.
    // The refusal itself is unchanged -- the planner still falls open and the
    // field still reaches the upstream.
    assert_eq!(decision.reason, FIELD_PREFLIGHT_UNATTRIBUTABLE_TARGET);
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
    // One decision here because this fixture carries ONE closed-table row, not
    // because a target contributes one: the unit is a row per target.
    let record = only_record(&dispatched.meta, "one row on one target, so one decision");
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
        "one planner and one present row, so one decision on each of three walks",
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
async fn a_fallback_chain_records_every_targets_decisions_in_planning_order() {
    // A single record slot reports only the LAST target, which discards
    // exactly the fallback behavior an operator needs: seat 0 acted, seat 1
    // did not. Both must survive, in planning order, each attributable to its
    // own target.
    //
    // This fixture carries ONE closed-table row, so the per-target count here
    // happens to be one -- the record unit is a row per target, and a fixture
    // carrying two rows would record two per target. What the chain contract
    // fixes is that NO target's decisions are overwritten, not the count.
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
    assert_eq!(
        debugs.len(),
        1,
        "one present row on one target, so one decision and one DEBUG line",
    );
    assert_eq!(
        warns.len(),
        1,
        "a request that rewrote an envelope earns exactly ONE request-level WARN",
    );
    assert_eq!(field_of(warns[0], "decisions_acted"), Some("1"));
    assert_eq!(field_of(warns[0], "decisions_planned"), Some("1"));
    assert_eq!(
        field_of(warns[0], "action"),
        Some(FIELD_PREFLIGHT_ACTION_DROP),
    );
    assert_eq!(
        field_of(warns[0], "transform_class"),
        Some("envelope"),
        "the WARN names the acting decision's transform class, so an operator \
         can tell an envelope rewrite from a content one at a glance",
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
fn a_fallback_chain_emits_a_debug_per_decision_and_still_one_warn() {
    // The aggregation contract across a chain: every planned decision reports at
    // DEBUG (so the per-target detail an operator needs survives), while the
    // request still earns exactly one WARN. Two DEBUG lines here because this
    // fixture's two targets each carry ONE closed-table row -- the DEBUG tier is
    // per DECISION, so a two-row fixture on two targets would emit four.
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
    assert_eq!(field_of(warns[0], "decisions_acted"), Some("1"));
    assert_eq!(field_of(warns[0], "decisions_planned"), Some("2"));
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
    let (_planned, records, _plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);
    let decision = only_decision(&records, "one row, one decision");

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
    let (_planned, acting_records, _acting_plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);
    let acting = only_decision(&acting_records, "one row, one decision");
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

// ===========================================================================
// The content gate: prefix-impacting opt-in and repair quorum
// ===========================================================================
//
// The closed table ships one ENVELOPE row and no prefix-impacting one, so the
// prefix-impacting class is driven through the test-only row the table adds in
// a test build (`TEST_PREFIX_IMPACTING_PATH` / the system-prompt surface).
// Without it every branch of the quorum and opt-in gates would be unreachable,
// and an unreachable gate is one whose removal no test would notice.
//
// The two classes' surfaces are DISJOINT (envelope metadata carriers vs the
// top-level system prompt), which is what lets one request carry both and pin
// that each row is gated on its own terms rather than on the request's.

/// The test-only prefix-impacting row's path, and the capability key it mints.
const PREFIX_PATH: &str = super::super::field_repair::TEST_PREFIX_IMPACTING_PATH;

fn prefix_key() -> String {
    crate::field_capability::field_capability_key(PREFIX_PATH)
        .expect("the test prefix-impacting path is a well-formed qualified path")
}

fn prefix_verdict_key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(state_key, PREFIX_PATH, ANTHROPIC).expect("a qualified path mints a key")
}

/// The EXACT system-prompt text the prefix-impacting surface recognizes.
///
/// The surface's presence predicate matches this string and nothing else (see
/// `FieldSurface::PrefixImpactingSystemForTests`), so only a fixture that opts
/// in by setting it carries the test-only row. That is what keeps every other
/// fixture in the suite -- here and in every sibling module -- free of a
/// closed-table row production cannot produce.
const SYSTEM_PROMPT: &str = super::super::field_repair::TEST_PREFIX_SENTINEL;

/// A request carrying ONLY the prefix-impacting row: the system prompt, with
/// neither envelope carrier set.
fn req_prefix_only(alias: &str) -> ChatRequest {
    let mut req = req_without_field(alias);
    req.system = Some(routectl_core::SystemContent::Text(SYSTEM_PROMPT.into()));
    req
}

/// A request carrying BOTH rows: the envelope carriers and the system prompt.
fn req_both_rows(alias: &str) -> ChatRequest {
    let mut req = req_on(alias);
    req.system = Some(routectl_core::SystemContent::Text(SYSTEM_PROMPT.into()));
    req
}

/// Whether a request still carries the prefix-impacting row's surface -- read
/// through the SURFACE's own predicate rather than `req.system.is_some()`, so
/// this helper cannot claim the row is present for a prompt the surface does not
/// recognize.
fn carries_system(req: &ChatRequest) -> bool {
    super::super::field_repair::FieldSurface::PrefixImpactingSystemForTests.present_in(req)
}

/// Plant a resident ACTING verdict for the PREFIX-IMPACTING row's identity on
/// `state_key`, acknowledged at `confirmations`.
///
/// Both halves go through the registries' own seams, the same way
/// `plant_eligible` plants the envelope row's -- the confirmation count's only
/// production writer today is the cold-rebuild seed.
fn plant_prefix_verdict(router: &Router, state_key: &str, confirmations: u32) {
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: prefix_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        state_key,
        &prefix_key(),
        ANTHROPIC,
    );
    router.field_verdicts().canaries().seed_from_rebuild(
        &prefix_verdict_key(state_key),
        incarnation,
        confirmations,
        false,
    );
    assert_eq!(
        router.field_verdicts().preflight_eligible(
            &prefix_verdict_key(state_key),
            router.registry_generation(),
            Instant::now(),
        ),
        confirmations >= 1,
        "fixture premise: {confirmations} confirmation(s) makes the prefix row \
         eligible exactly when it is at least one",
    );
}

/// `chain_config` plus a `[fidelity] prefix_impact_opt_in` list.
fn config_with_opt_in(specs: &[&str]) -> Config {
    let mut config = chain_config(ALIAS, 1, ANTHROPIC);
    config.fidelity.prefix_impact_opt_in = specs.iter().map(|spec| (*spec).to_string()).collect();
    config
}

/// The single decision for the prefix row in `records`.
fn prefix_decision(records: &[FieldPreflight]) -> &FieldPreflight {
    let matching: Vec<&FieldPreflight> = records
        .iter()
        .filter(|r| r.field_path == Some(PREFIX_PATH))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one decision for the prefix-impacting row: {records:?}",
    );
    matching[0]
}

/// The single decision for the grounded envelope row in `records`.
fn envelope_decision(records: &[FieldPreflight]) -> &FieldPreflight {
    let matching: Vec<&FieldPreflight> = records
        .iter()
        .filter(|r| r.field_path == Some(GROUNDED_PATH))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one decision for the envelope row: {records:?}",
    );
    matching[0]
}

#[test]
fn a_prefix_impacting_row_at_one_confirmation_is_blocked_even_with_opt_in() {
    // THE quorum pin. One confirmation is what an ENVELOPE row acts on; a
    // prefix-impacting rewrite costs a full cache-prefix recompute per
    // request, so it needs two confirmed cycles. The target IS opted in here,
    // which is what makes the refusal attributable to the quorum alone.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", 1);
    let original = req_prefix_only(ALIAS);

    let (planned, records) = plan_all(&router, &original, "m0");

    let decision = prefix_decision(&records);
    assert!(
        !decision.acted,
        "one confirmation is below the prefix-impacting quorum of two",
    );
    assert_eq!(decision.reason, super::FIELD_PREFLIGHT_BELOW_QUORUM);
    assert_eq!(decision.transform_class, Some("prefix_impacting"));
    assert!(
        carries_system(&planned),
        "the blocked row's content is dispatched exactly as the client sent it",
    );
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        serde_json::to_value(&original).unwrap(),
        "and the whole request is byte-equal to the original",
    );
}

#[test]
fn a_prefix_impacting_row_at_quorum_without_opt_in_is_blocked() {
    // THE opt-in pin. The verdict is confirmed twice over -- the quorum gate
    // is satisfied -- and the rewrite still must not fire, because no operator
    // has named this target in `[fidelity]`. A content rewrite stays dormant
    // until the operator activates it, however well confirmed.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let original = req_prefix_only(ALIAS);

    let (planned, records) = plan_all(&router, &original, "m0");

    let decision = prefix_decision(&records);
    assert!(
        !decision.acted,
        "quorum alone does not authorize a content rewrite"
    );
    assert_eq!(decision.reason, super::FIELD_PREFLIGHT_NO_TARGET_OPT_IN);
    assert!(carries_system(&planned));
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        serde_json::to_value(&original).unwrap(),
    );
}

#[test]
fn a_prefix_impacting_row_acts_at_quorum_plus_opt_in() {
    // The ONE state a prefix-impacting rewrite may act on: two confirmed
    // cycles AND an explicit target opt-in. Both other gates were asserted
    // above with this one satisfied, so this case is what makes those two
    // refusals about their own gate rather than about the row never acting.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let original = req_prefix_only(ALIAS);

    let (planned, records) = plan_all(&router, &original, "m0");

    let decision = prefix_decision(&records);
    assert!(decision.acted, "quorum plus opt-in authorizes the rewrite");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_ACTION_DROP);
    assert_eq!(decision.transform_class, Some("prefix_impacting"));
    assert!(
        !carries_system(&planned),
        "the planned request lost the prefix-impacting surface",
    );
    assert!(
        carries_system(&original),
        "the planner rewrites a clone: the caller's request keeps its prompt",
    );
}

#[test]
fn an_envelope_row_acts_at_one_confirmation_without_any_opt_in() {
    // The counterpart that keeps the two classes' gates distinct: the same
    // single confirmation that BLOCKS a prefix-impacting row acts on an
    // envelope row, and no `[fidelity]` entry exists for this target. Without
    // this case a planner that simply required quorum two and an opt-in for
    // EVERYTHING would satisfy every prefix test above.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    assert!(
        router.config.fidelity.prefix_impact_opt_in.is_empty(),
        "fixture premise: no target is opted into prefix-impacting pre-flight",
    );

    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(
        decision.acted,
        "one confirmation is the envelope class's quorum"
    );
    assert_eq!(decision.transform_class, Some("envelope"));
    assert!(!carries_field(&planned));
}

#[test]
fn an_opted_in_target_still_needs_its_verdict_confirmed_twice() {
    // The opt-in is not a bypass: an opted-in target with an unconfirmed
    // verdict reports the VERDICT gate, so an operator reading the record
    // cannot mistake the opt-in for the only thing between a target and a
    // content rewrite.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", 0);

    let (planned, records) = plan_all(&router, &req_prefix_only(ALIAS), "m0");

    let decision = prefix_decision(&records);
    assert!(!decision.acted, "an opt-in authorizes nothing on its own");
    assert_eq!(
        decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE,
        "zero confirmations is not eligible at all, which is a different \
         operator situation from eligible-but-short-of-quorum",
    );
    assert!(carries_system(&planned));
}

#[test]
fn opt_in_is_membership_in_either_tier_of_the_existing_target_spec_grammar() {
    // The opt-in reuses `[capability.overrides]`'s target-spec GRAMMAR, not its
    // precedence rule -- and there is no precedence to reuse: that surface has
    // two verdicts (`unsupported` / `force_supported`) and so needs a rule for a
    // target both tiers name, while this list has one -- listed, or not -- so a
    // target is opted in when EITHER tier matches and nothing arbitrates between
    // them. What the grammar gives is the SHAPE of a match: a bare provider spec
    // covers every model dispatched through that provider, a `provider:nickname`
    // spec covers exactly that model, and a model-scoped spec for a DIFFERENT
    // model matches neither tier. Driven as a table so the cases cannot drift
    // apart, and every case runs with quorum satisfied so the opt-in is the only
    // variable.
    for (specs, expected_acted, label) in [
        (vec!["p0"], true, "provider-scoped spec opts in every model"),
        (vec!["p0:m0"], true, "model-scoped spec opts in that model"),
        (
            vec!["p0:other"],
            false,
            "a model-scoped spec for a different model opts this one in for neither tier",
        ),
        (
            vec!["other"],
            false,
            "a spec naming another provider opts nothing in here",
        ),
        (vec![], false, "an empty list opts nothing in"),
    ] {
        let (router, _seen) = install(config_with_opt_in(&specs), 1, Answer::ServeImmediately);
        plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

        let (planned, records) = plan_all(&router, &req_prefix_only(ALIAS), "m0");

        let decision = prefix_decision(&records);
        assert_eq!(decision.acted, expected_acted, "{label}: {decision:?}");
        assert_eq!(
            carries_system(&planned),
            !expected_acted,
            "{label}: the dispatched body must match the decision",
        );
        if !expected_acted {
            assert_eq!(
                decision.reason,
                super::FIELD_PREFLIGHT_NO_TARGET_OPT_IN,
                "{label}: the refusal names the opt-in gate, since quorum is satisfied",
            );
        }
    }
}

#[test]
fn a_force_supported_override_masks_a_prefix_impacting_row_at_quorum_and_opt_in() {
    // The operator mask outranks the learned verdict for a content row too,
    // and it is reported as the mask rather than as a gate shortfall -- every
    // other gate is satisfied here, so the mask is the operative reason.
    let mut config = config_with_opt_in(&["p0"]);
    config.capability.overrides.insert(
        "p0".to_string(),
        crate::config::OverrideEntry {
            unsupported: Vec::new(),
            force_supported: vec![prefix_key()],
        },
    );
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (planned, records) = plan_all(&router, &req_prefix_only(ALIAS), "m0");

    let decision = prefix_decision(&records);
    assert!(!decision.acted, "the operator said to send this content");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE);
    assert!(
        carries_system(&planned),
        "the masked surface is dispatched, which is what force_supported means",
    );
}

#[test]
fn a_force_supported_override_does_not_delete_the_masked_verdict() {
    // A mask suppresses the ACTION, never the state. An implementation that
    // cleared the verdict instead would look identical on the masked request
    // and then, the moment the operator removed the override, would have
    // silently discarded the confirmed evidence.
    let mut config = config_with_opt_in(&["p0"]);
    config.capability.overrides.insert(
        "p0".to_string(),
        crate::config::OverrideEntry {
            unsupported: Vec::new(),
            force_supported: vec![prefix_key()],
        },
    );
    let (router, _seen) = install(config, 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (_planned, records) = plan_all(&router, &req_prefix_only(ALIAS), "m0");
    assert_eq!(
        prefix_decision(&records).reason,
        FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
        "premise: the mask is what refused",
    );

    let key = prefix_verdict_key("m0");
    let authorization = router
        .field_verdicts()
        .preflight_authorization(&key, router.registry_generation(), Instant::now())
        .expect("the masked verdict is still pre-flight eligible -- the mask deleted nothing");
    assert!(
        authorization.confirmations >= crate::config::PREFIX_QUORUM,
        "and it is still confirmed to quorum: {authorization:?}",
    );
}

#[test]
fn each_present_row_is_gated_on_its_own_terms_in_one_request() {
    // The composition property. One request carries BOTH rows, the envelope
    // row is confirmed once (its quorum) and the prefix row twice with no
    // opt-in. The envelope row must act and the prefix row must not, on the
    // same request: a planner that gated per REQUEST rather than per ROW would
    // either block both or act on both.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let original = req_both_rows(ALIAS);

    let (planned, records) = plan_all(&router, &original, "m0");

    assert_eq!(
        records.len(),
        2,
        "one decision per considered row: {records:?}"
    );
    assert!(
        envelope_decision(&records).acted,
        "the envelope row clears its own quorum: {records:?}",
    );
    assert!(
        !prefix_decision(&records).acted,
        "the prefix row has no opt-in: {records:?}",
    );
    assert_eq!(
        prefix_decision(&records).reason,
        super::FIELD_PREFLIGHT_NO_TARGET_OPT_IN,
    );
    assert!(
        !carries_field(&planned),
        "the acting envelope row's rewrite is adopted",
    );
    assert!(
        carries_system(&planned),
        "and the blocked prefix row's content survives on the SAME body",
    );
}

#[test]
fn two_authorized_rows_compose_onto_one_planned_body() {
    // The converse: both rows authorized, so both rewrites land on one body.
    // A planner that returned each row's rewrite from a fresh clone of the
    // original would drop whichever row it planned first.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (planned, records) = plan_all(&router, &req_both_rows(ALIAS), "m0");

    assert!(
        records.iter().all(|r| r.acted),
        "both rows are authorized: {records:?}",
    );
    assert!(!carries_field(&planned), "the envelope rewrite landed");
    assert!(!carries_system(&planned), "and so did the content rewrite");
}

#[test]
fn the_decision_set_is_independent_of_closed_table_order() {
    // Table order is the ENUMERATION order and nothing else: each decision is
    // computed from its own row's class, verdict, and gates. So the SET of
    // (path, acted, reason) triples a request produces must not depend on the
    // order the rows are scanned in -- asserted by comparing the planner's
    // output against its own order-insensitive sort, over a request carrying
    // both rows under mixed authorization.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (_planned, records) = plan_all(&router, &req_both_rows(ALIAS), "m0");

    let mut produced: Vec<(Option<&'static str>, bool, &'static str)> = records
        .iter()
        .map(|r| (r.field_path, r.acted, r.reason))
        .collect();
    produced.sort_unstable();
    let mut expected = vec![
        (Some(GROUNDED_PATH), true, FIELD_PREFLIGHT_ACTION_DROP),
        (
            Some(PREFIX_PATH),
            false,
            super::FIELD_PREFLIGHT_NO_TARGET_OPT_IN,
        ),
    ];
    expected.sort_unstable();
    assert_eq!(
        produced, expected,
        "each row's verdict is its own, whatever order the table is scanned in",
    );
}

#[test]
fn a_purge_moves_learned_state_only_and_cannot_touch_the_compiled_strip_table() {
    // THE CONTRACT, stated as it actually is rather than as a provenance claim
    // this build does not have: `capability_strip`'s table is COMPILED -- a pure
    // `match` over feature keys, holding no state, expiring nothing, and exposing
    // no mutator at all. There is no baked field-verdict provenance surface to
    // test, and inventing one would be testing a thing that does not exist.
    //
    // So what is testable, and what this pins, is the SEPARATION: a purge is a
    // mutation of LEARNED state, it demonstrably moves that state, and the
    // compiled answer for the same key is identical before and after. The
    // before/after comparison is deliberately NOT the whole test -- a compiled
    // `match` cannot change at runtime, so that half alone would pass against any
    // implementation. It is the LEARNED half, shown to have really moved, that
    // makes the pairing meaningful.
    //
    // Scope, stated because it is easy to overread: the field key's compiled
    // answer is the fail-closed `RouteAway` default, because the strip table
    // names no field row. That is an observation about today's table rather than
    // a property of field keys. A future table naming one would change this
    // value and SHOULD update this test -- what must not change is that a purge
    // cannot be what changes it.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    let capability_key = grounded_key();
    let compiled_before = crate::capability_strip::action_for(&capability_key);
    assert!(
        !crate::capability_strip::strippable_keys().any(|key| key == capability_key),
        "premise: the compiled strip table names no row for this field key, which is \
         why its answer is the fail-closed default",
    );

    // The LEARNED half moves, through the landed two-phase purge protocol.
    let reserved = match router.reserve_learned_capability_purge("m0", &capability_key) {
        super::super::PurgeOutcome::Reserved(reserved) => reserved,
        _ => panic!("premise: a resident entry under a live generation reserves"),
    };
    let _settlement = reserved.settlement();
    assert!(
        router.finalize_learned_capability_purge(reserved),
        "premise: the purge removed the learned entry",
    );
    assert!(
        !router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "the purged verdict leaves no acting entry resident -- the RESIDENCE check is \
         what makes this about the purge rather than about the canary reset that \
         rides with it",
    );
    let (planned, decision) = plan(&router, &req_on(ALIAS), "m0");
    assert!(
        !decision.acted,
        "and the planner that acted before now has nothing to act on",
    );
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(
        carries_field(&planned),
        "so the client's field goes out unchanged",
    );

    // The COMPILED half is untouched by that same mutation.
    assert_eq!(
        crate::capability_strip::action_for(&capability_key),
        compiled_before,
        "a purge of learned state cannot edit the compiled action for its key: the two \
         are different stores, and only one of them has a mutator",
    );
    // And it still answers Strip for a key the table DOES name, which is the
    // control: without it, this pair would only show that one default is stable.
    let strippable = crate::capability_strip::strippable_keys()
        .next()
        .expect("the compiled table names at least one strippable key");
    assert!(
        matches!(
            crate::capability_strip::action_for(strippable),
            crate::capability_strip::CapabilityAction::Strip(_)
        ),
        "control: a key the compiled table DOES name still answers Strip after the \
         purge, so the assertion above is about the table being immutable rather \
         than about every key answering one default",
    );
}

// ---------------------------------------------------------------------------
// The content gate holds through a real dispatch, on all three walks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_three_walks_block_a_prefix_impacting_row_without_opt_in() {
    // The gates are asserted on the DISPATCHED bytes, not only on the decision
    // record: a planner that recorded a refusal while still adopting the
    // rewrite would satisfy every record-level assertion above.
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let completed = router
        .complete_with_options(req_prefix_only(ALIAS), RouterOptions::new())
        .await;
    let streamed = router
        .stream_with_options(req_prefix_only(ALIAS), RouterOptions::new())
        .await;
    let counted = router.count_tokens_with_meta(req_prefix_only(ALIAS)).await;

    assert!(completed.result.is_ok() && streamed.result.is_ok() && counted.result.is_ok());
    assert_eq!(
        seen.dispatched
            .lock()
            .iter()
            .map(carries_system)
            .collect::<Vec<bool>>(),
        vec![true, true, true],
        "every walk dispatched the client's content: the gate is not per-surface",
    );
    for meta in [&completed.meta, &streamed.meta, &counted.meta] {
        let decision = prefix_decision(&meta.field_preflight);
        assert!(!decision.acted);
        assert_eq!(decision.reason, super::FIELD_PREFLIGHT_NO_TARGET_OPT_IN);
    }
}

#[tokio::test]
async fn all_three_walks_dispatch_the_content_rewrite_once_opted_in() {
    // The positive control for the case above, on the same three walks: with
    // the opt-in present and the quorum met, every walk sends the rewritten
    // body. Without it, an implementation that blocked unconditionally would
    // pass the refusal assertions.
    let (router, mut observed) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    let seen = observed.remove(0);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let completed = router
        .complete_with_options(req_prefix_only(ALIAS), RouterOptions::new())
        .await;
    let streamed = router
        .stream_with_options(req_prefix_only(ALIAS), RouterOptions::new())
        .await;
    let counted = router.count_tokens_with_meta(req_prefix_only(ALIAS)).await;

    assert!(completed.result.is_ok() && streamed.result.is_ok() && counted.result.is_ok());
    assert_eq!(
        seen.dispatched
            .lock()
            .iter()
            .map(carries_system)
            .collect::<Vec<bool>>(),
        vec![false, false, false],
        "and the token count measured the SAME rewritten body the messages walks sent",
    );
    for meta in [&completed.meta, &streamed.meta, &counted.meta] {
        assert!(prefix_decision(&meta.field_preflight).acted);
    }
}

// ---------------------------------------------------------------------------
// The authorization read is consistent under a concurrent state change
// ---------------------------------------------------------------------------

#[test]
fn a_verdict_that_moves_between_the_two_authorization_reads_is_refused() {
    // The quorum is read from the SAME authorization the eligibility decision
    // returns, so it carries the same race and the same close: a relearn
    // landing between the two reads would let a confirmation count authorize an
    // incarnation the entry had already left. Landed deterministically through
    // the module's own interposition seam -- a thread-spawning test that passes
    // proves nothing about a window it may never have reached.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let key = prefix_verdict_key("m0");
    let generation = router.registry_generation();
    assert!(
        router
            .field_verdicts()
            .preflight_authorization(&key, generation, Instant::now())
            .is_some_and(|auth| auth.confirmations >= crate::config::PREFIX_QUORUM),
        "premise: quorum is reached with nothing interposed",
    );

    let learned = Arc::clone(&router.learned_capabilities);
    let prefix = prefix_key();
    let landed = Arc::new(AtomicUsize::new(0));
    let landed_in_hook = Arc::clone(&landed);
    let _interposed = crate::field_verdict::eligibility_interpose::install(move || {
        landed_in_hook.fetch_add(1, Ordering::SeqCst);
        learned.bump_incarnation_for_tests("m0", &prefix, ANTHROPIC);
    });

    let authorization =
        router
            .field_verdicts()
            .preflight_authorization(&key, generation, Instant::now());

    assert_eq!(
        landed.load(Ordering::SeqCst),
        1,
        "premise: the interposition ran, so the window under test was reached",
    );
    assert!(
        authorization.is_none(),
        "a confirmation count whose incarnation the entry has already left is not \
         authorization: {authorization:?}",
    );
}

#[test]
fn the_authorization_read_stays_consistent_under_hostile_concurrency() {
    // The companion stress to the deterministic pin above: many threads reading
    // one identity's authorization while a writer churns it. The property is not
    // a count -- a racing writer makes either outcome legal -- but that an
    // authorization reporting quorum is BACKED by a resident count at least as
    // large, re-read immediately after.
    //
    // THE HAZARD THIS TEST ITSELF HAS, and the reason for the counters below: a
    // writer that only bumped the incarnation would make every read refuse on
    // the incarnation re-check, so the protected branch would execute ZERO times
    // and the test would pass against any implementation of it. So the churn
    // alternates BOTH axes -- it bumps the incarnation and then reconciles a
    // quorum-satisfying count onto the new one -- which keeps authorized windows
    // genuinely reachable, and the branch's execution count is asserted nonzero
    // rather than assumed.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let router = Arc::new(router);
    let key = prefix_verdict_key("m0");
    let generation = router.registry_generation();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // How many times the PROTECTED branch ran: a read that reported quorum and
    // therefore had its backing re-checked. Zero means the window was never
    // reached and every assertion inside it was vacuous.
    let authorized = Arc::new(AtomicUsize::new(0));

    let churn = {
        let router = Arc::clone(&router);
        let stop = Arc::clone(&stop);
        let key = key.clone();
        let prefix = prefix_key();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Move the lifecycle forward, then make the NEW incarnation
                // quorum-satisfying. Without the second step the identity would
                // be permanently unauthorized and the readers below would never
                // enter their protected branch.
                router
                    .learned_capabilities
                    .bump_incarnation_for_tests("m0", &prefix, ANTHROPIC);
                let incarnation = router
                    .learned_capabilities
                    .resident_incarnation_for_tests("m0", &prefix, ANTHROPIC);
                router.field_verdicts().canaries().acknowledge_confirmation(
                    &key,
                    incarnation,
                    crate::config::PREFIX_QUORUM,
                );
            }
        })
    };

    let readers: Vec<_> = (0..64)
        .map(|_| {
            let router = Arc::clone(&router);
            let key = key.clone();
            let authorized = Arc::clone(&authorized);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let Some(auth) = router.field_verdicts().preflight_authorization(
                        &key,
                        generation,
                        Instant::now(),
                    ) else {
                        continue;
                    };
                    if auth.confirmations < crate::config::PREFIX_QUORUM {
                        continue;
                    }
                    authorized.fetch_add(1, Ordering::Relaxed);
                    // The count the authorization reported must be a count some
                    // writer actually WROTE. Only two writers exist here -- the
                    // fixture seed and the churn -- and both write exactly
                    // `PREFIX_QUORUM`, so any other value is fabricated. This is
                    // what a `u32::MAX` (or any invented count) mutation trips,
                    // and it is race-stable: the bound holds whatever the churn
                    // has reached.
                    assert_eq!(
                        auth.confirmations,
                        crate::config::PREFIX_QUORUM,
                        "the only counts any writer in this test produces are \
                         PREFIX_QUORUM, so a different value was fabricated rather \
                         than read from resident state",
                    );
                    // And the pair must be CONSISTENT: while the identity is
                    // still on the incarnation this authorization validated, the
                    // resident count must be the one it reported. Conditional on
                    // the incarnation deliberately -- the churn may have moved
                    // the lifecycle on since, and refusing to check then is
                    // correct rather than a weaker assertion, because the
                    // authorization describes the state it validated and not a
                    // later one.
                    let snap = router
                        .field_verdicts()
                        .canaries()
                        .snapshot(&key)
                        .expect("an authorized identity is resident");
                    if snap.incarnation == auth.incarnation {
                        assert_eq!(
                            auth.confirmations, snap.confirmations,
                            "on the incarnation the authorization validated, its count \
                             must be the resident one",
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

    assert!(
        authorized.load(Ordering::Relaxed) > 0,
        "the protected branch must have executed: zero authorized reads means the \
         churn never left an authorizable window and every assertion inside the \
         branch was vacuous",
    );
}

#[test]
fn each_class_consumes_the_co_located_quorum_constant_for_its_own_tier() {
    // WHAT THIS TEST COVERS, and what it deliberately does not. The quorum
    // ORDER (`MINIMUM_CONFIRMATIONS <= ENVELOPE_QUORUM < PREFIX_QUORUM`) is
    // enforced beside the definitions, by two anonymous
    // `const _: () = assert!(...)` items in `config::schema`. Rustc evaluates the
    // initializer of every `const` item in a crate it compiles, so those need no
    // consumer, no reference, and no name. No test can observe a compile failure,
    // so none is attempted here.
    //
    // What IS testable, and what this pins, is the WIRING: that each class reads
    // its own tier's constant rather than sharing one. The compile-time check
    // cannot see that mistake, because swapping the arms leaves both literals --
    // and therefore the relation between them -- untouched.
    //
    // The three mutation classes and their evidence:
    // - ORDER-VIOLATING literal edit (`PREFIX_QUORUM = 1`, `ENVELOPE_QUORUM = 0`,
    //   `field_verdict::MINIMUM_CONFIRMATIONS = 2` -- the floor assertion reads
    //   that one by PATH rather than as a copy): compile-time RED,
    //   `error[E0080]: evaluation panicked` at the failing assertion. Not a test
    //   failure, so nothing here observes it.
    // - ORDER-PRESERVING literal retune (`PREFIX_QUORUM = 3`,
    //   `MINIMUM_CONFIRMATIONS = 0`): COMPILES, and the inequality assertions stay
    //   green. Caught by `the_three_quorum_values_are_exactly_one_one_and_two`.
    // - CONSUMER mutation (`PrefixImpacting => ENVELOPE_QUORUM`, or either arm
    //   returning a literal): compiles, and goes RED here plus at
    //   `a_prefix_impacting_row_at_one_confirmation_is_blocked_even_with_opt_in`.
    use super::super::field_repair::TransformClass;
    use crate::config::{ENVELOPE_QUORUM, PREFIX_QUORUM};

    assert_eq!(
        TransformClass::Envelope.required_quorum(),
        ENVELOPE_QUORUM,
        "the envelope class must read the envelope constant",
    );
    assert_eq!(
        TransformClass::PrefixImpacting.required_quorum(),
        PREFIX_QUORUM,
        "and the prefix-impacting class its own, not the envelope one",
    );
    assert!(
        TransformClass::Envelope.required_quorum()
            < TransformClass::PrefixImpacting.required_quorum(),
        "read back THROUGH the classes rather than off the constants: an arm \
         wired to the wrong constant satisfies the ordering of the literals \
         while collapsing the two gates into one",
    );
    assert!(
        !TransformClass::Envelope.requires_target_opt_in(),
        "an envelope rewrite needs no [fidelity] entry",
    );
    assert!(
        TransformClass::PrefixImpacting.requires_target_opt_in(),
        "a content rewrite does",
    );
}

#[test]
fn the_three_quorum_values_are_exactly_one_one_and_two() {
    // The EXACT values, which the compile-time assertions cannot reach.
    //
    // WHAT FAILS HOW, stated precisely because an earlier comment here
    // overclaimed it:
    //
    // - `config::schema`'s two anonymous `const _` assertions pin the
    //   INEQUALITIES `MINIMUM_CONFIRMATIONS <= ENVELOPE_QUORUM < PREFIX_QUORUM`.
    //   An edit that VIOLATES one fails the BUILD (`error[E0080]: evaluation
    //   panicked`) -- `PREFIX_QUORUM = 1`, `ENVELOPE_QUORUM = 0`, or
    //   `MINIMUM_CONFIRMATIONS = 2` each do.
    // - An ORDER-PRESERVING retune does NOT. Measured: `PREFIX_QUORUM = 3`
    //   compiles clean and every other test in this module passes, because the
    //   inequality still holds. So does `MINIMUM_CONFIRMATIONS = 0`.
    //
    // That gap is what this test closes. The three numbers are the feature's
    // stated safety parameters -- one confirmation for an envelope rewrite, two
    // confirmed cycles for a content rewrite -- and a silent retune of either
    // would change how much evidence routectl demands before rewriting a
    // client's request, while leaving the relation between them intact and every
    // inequality check green.
    //
    // Asserted against literals rather than against each other, deliberately: a
    // comparison between the constants restates the compile-time check, and a
    // comparison against a named constant would be a tautology.
    let floor = crate::field_verdict::MINIMUM_CONFIRMATIONS;
    let envelope = crate::config::ENVELOPE_QUORUM;
    let prefix = crate::config::PREFIX_QUORUM;

    assert_eq!(
        floor, 1,
        "the shared eligibility floor is ONE acknowledged confirmation: it is what \
         makes `not_eligible` and `below_quorum` distinguishable, since a verdict at \
         zero confirmations is not eligible at all. Got {floor}",
    );
    assert_eq!(
        envelope, 1,
        "an ENVELOPE rewrite acts on ONE acknowledged confirmation -- the feature's \
         stated quorum for a transform that touches no cache prefix. Got {envelope}",
    );
    assert_eq!(
        prefix, 2,
        "a PREFIX-IMPACTING rewrite needs TWO confirmed reject-unrepaired -> \
         accept-repaired cycles. Raising this silently demands more evidence than \
         the feature specifies and lowering it demands less; neither is caught by \
         the inequality assertions, which only require it to exceed the envelope \
         quorum. Got {prefix}",
    );
}

#[test]
fn a_prefix_impacting_decision_leaks_no_system_prompt_into_its_diagnostic() {
    // The prefix-impacting surface IS content, which makes this class the one
    // where a leak actually costs something: the dropped value is a prompt, not
    // an enum token. Both tiers are captured, and the sentinel is asserted to
    // be a value the request genuinely carried, so a leaking emitter would have
    // it to print.
    // The prompt IS the sentinel, because the surface only recognizes that exact
    // text -- a distinct "leak me" string would not carry the row at all, and the
    // assertion would pass against a request the planner never acted on. The
    // premise assertions below are what make that non-vacuous: the acting WARN
    // fired and names the prefix-impacting class, so the value under test is a
    // prompt the planner genuinely dropped.
    const SYSTEM_SENTINEL: &str = SYSTEM_PROMPT;
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let req = req_prefix_only(ALIAS);
    assert!(
        carries_system(&req),
        "premise: the request carries the prefix-impacting surface",
    );

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req, RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 1, "premise: the DEBUG tier fired");
    assert_eq!(warns.len(), 1, "premise: the acting WARN tier fired");
    assert_eq!(
        field_of(warns[0], "transform_class"),
        Some("prefix_impacting"),
        "premise: the acting decision IS the prefix-impacting row, so its \
         dropped value is the prompt this assertion is about",
    );
    let rendered = format!("{events:#?}");
    assert!(
        !rendered.contains(SYSTEM_SENTINEL),
        "a pre-flight diagnostic must never carry the dropped system prompt",
    );
}

// ---------------------------------------------------------------------------
// The reactive arm attributes a rejection to the row that names it
// ---------------------------------------------------------------------------

/// Plan a reactive carry for `state_key`'s target over `req`, admitting every
/// present closed-table row.
fn carry_for<'r>(
    router: &'r Router,
    req: &ChatRequest,
    state_key: &str,
) -> super::super::field_repair::FieldRepairPlan<'r> {
    router
        .plan_field_carry(
            &target_for(router, state_key, "p0"),
            req,
            super::super::field_repair::FieldSettlementMode::Settling,
            Instant::now(),
        )
        .expect("a request carrying closed-table rows admits a plan")
}

#[test]
fn a_rejection_naming_the_later_present_row_repairs_that_row() {
    // THE starvation pin. The request carries both rows and the upstream names
    // the SECOND one in table order. An admission that held only the first
    // present row would find the rejection unnamed, leave it on the ordinary
    // terminal path, and the named field would never be repaired on this lane.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let original = req_both_rows(ALIAS);
    let mut attempt = original.clone();
    let mut plan = carry_for(&router, &original, "m0");
    let mut budget = RepairBudget::per_request();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    let _injection = super::super::field_repair::provisional::inject(PREFIX_PATH);

    let applied = plan.apply(
        &mut attempt,
        &mut meta,
        &mut budget,
        &routectl_core::failure_class::FailureClass::BadRequest,
        &Error::upstream("p", 400, "{}"),
        ANTHROPIC,
    );

    assert!(
        applied.is_some(),
        "the rejection names an admitted row, so the repair must fire",
    );
    assert_eq!(
        plan.repaired_path(),
        Some(PREFIX_PATH),
        "and it must be the row the upstream NAMED, not the first present one",
    );
    assert!(
        !carries_system(&attempt),
        "the named row's surface is what was dropped",
    );
    assert!(
        carries_field(&attempt),
        "and the unnamed row's surface is untouched: one repair per attempt",
    );
}

#[test]
fn a_rejection_naming_the_first_present_row_repairs_that_row() {
    // The order control for the case above: with the FIRST row named, the same
    // plan repairs that one instead. Without this pair, an implementation that
    // always repaired the last admitted row would pass the test above.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let original = req_both_rows(ALIAS);
    let mut attempt = original.clone();
    let mut plan = carry_for(&router, &original, "m0");
    let mut budget = RepairBudget::per_request();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    let _injection = super::super::field_repair::provisional::inject(GROUNDED_PATH);

    let applied = plan.apply(
        &mut attempt,
        &mut meta,
        &mut budget,
        &routectl_core::failure_class::FailureClass::BadRequest,
        &Error::upstream("p", 400, "{}"),
        ANTHROPIC,
    );

    assert!(applied.is_some(), "the named row repairs");
    assert_eq!(plan.repaired_path(), Some(GROUNDED_PATH));
    assert!(!carries_field(&attempt), "the named row's surface is gone");
    assert!(
        carries_system(&attempt),
        "and the other row's content is untouched",
    );
}

#[test]
fn only_one_row_is_repaired_per_attempt_and_the_rest_release_their_slots() {
    // The one-repair ceiling, and the slot hygiene that must accompany it: the
    // unused candidate's single-flight slot is RELEASED by the apply rather
    // than pinned to settlement, so a sibling request that can settle that row
    // is admitted immediately. Measured through a second carry for the same
    // target: the released row admits, the repaired row does not.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let original = req_both_rows(ALIAS);
    let mut attempt = original.clone();
    let mut plan = carry_for(&router, &original, "m0");
    let mut budget = RepairBudget::per_request();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    let _injection = super::super::field_repair::provisional::inject(PREFIX_PATH);

    assert!(
        plan.apply(
            &mut attempt,
            &mut meta,
            &mut budget,
            &routectl_core::failure_class::FailureClass::BadRequest,
            &Error::upstream("p", 400, "{}"),
            ANTHROPIC,
        )
        .is_some(),
        "premise: the named row repaired",
    );

    // A second `apply` on the same plan repairs nothing: the ceiling is one
    // drop per attempt, and a second would mutate a body the arm already
    // re-dispatched.
    let after_first = serde_json::to_value(&attempt).unwrap();
    assert!(
        plan.apply(
            &mut attempt,
            &mut meta,
            &mut budget,
            &routectl_core::failure_class::FailureClass::BadRequest,
            &Error::upstream("p", 400, "{}"),
            ANTHROPIC,
        )
        .is_none(),
        "a plan that already repaired must refuse a second apply",
    );
    assert_eq!(
        serde_json::to_value(&attempt).unwrap(),
        after_first,
        "and must leave the body exactly as the first repair left it",
    );

    // The ENVELOPE row's slot was released, so a sibling carry for a request
    // carrying only that row is admitted while this plan is still alive.
    let sibling = carry_for(&router, &req_on(ALIAS), "m0");
    assert_eq!(
        sibling.repaired_path(),
        None,
        "premise: the sibling has not repaired; it holds the released slot",
    );
    drop(sibling);
    drop(plan);
}

/// Plant a LAPSED resident entry for `path`'s identity on `state_key`: the
/// state where a reactive carry is admitted (the verdict is not acting) AND a
/// resident entry still exists for a clear to remove.
fn plant_lapsed(router: &Router, state_key: &str, path: &str) {
    let stamped = Instant::now();
    let feature_key = crate::field_capability::field_capability_key(path)
        .expect("a qualified path mints a capability key");
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key,
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            // Expiring at the planting instant is already past for every later
            // read, so the entry is resident but LAPSED.
            expires_at: stamped,
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

#[test]
fn an_unrepaired_success_clears_every_admitted_rows_resident_verdict() {
    // The clear side of the settlement, PER ROW: every field the attempt
    // carried was ACCEPTED, so each admitted row's resident verdict is dropped
    // and each removal rides out on its own cleared event. A settlement that
    // cleared only one row would leave a warm rebuild resurrecting the other.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_lapsed(&router, "m0", GROUNDED_PATH);
    plant_lapsed(&router, "m0", PREFIX_PATH);
    let plan = carry_for(&router, &req_both_rows(ALIAS), "m0");

    let cleared = plan.settle_success();

    assert_eq!(
        cleared.len(),
        2,
        "both admitted rows had a resident entry, so both clears must be \
         reported: {cleared:?}",
    );
    let mut keys: Vec<&str> = cleared.iter().map(|e| e.capability_key.as_str()).collect();
    keys.sort_unstable();
    let mut expected = [grounded_key(), prefix_key()];
    expected.sort_unstable();
    assert_eq!(
        keys,
        expected.iter().map(String::as_str).collect::<Vec<&str>>(),
        "one cleared event per row, each naming its own capability key",
    );
}

#[test]
fn a_confirmation_count_moving_mid_read_cannot_produce_a_false_quorum_shortfall() {
    // THE single-snapshot pin. Eligibility and the class quorum are both
    // decided from ONE snapshot of the canary state. A planner that re-read the
    // count after the eligibility decision would answer two questions against
    // two states that never coexisted: a reconciliation landing in between --
    // the same incarnation, a count momentarily observed lower -- would make a
    // verdict that IS confirmed to quorum report `below_quorum` instead, which
    // is a false diagnostic about which gate is holding.
    //
    // Landed deterministically through the module's own interposition seam, for
    // the same reason the incarnation race is: a thread-spawning test that
    // passes proves nothing about a window it may never have reached.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let key = prefix_verdict_key("m0");
    let generation = router.registry_generation();
    let incarnation =
        router
            .learned_capabilities
            .resident_incarnation_for_tests("m0", &prefix_key(), ANTHROPIC);
    assert!(
        router
            .field_verdicts()
            .preflight_authorization(&key, generation, Instant::now())
            .is_some_and(|auth| auth.confirmations >= crate::config::PREFIX_QUORUM),
        "premise: quorum is reached with nothing interposed",
    );

    // The interposition reconciles the SAME incarnation's count down to zero,
    // which is a state movement the registry genuinely permits (the
    // acknowledgment is the count's only writer). The acting incarnation does
    // NOT move, so the re-read below still matches and the authorization is
    // still returned -- what differs is only WHICH count it carries.
    let canaries = Arc::clone(router.field_verdicts().canaries());
    let interposed_key = key.clone();
    let landed = Arc::new(AtomicUsize::new(0));
    let landed_in_hook = Arc::clone(&landed);
    let _interposed = crate::field_verdict::eligibility_interpose::install(move || {
        landed_in_hook.fetch_add(1, Ordering::SeqCst);
        canaries.acknowledge_confirmation(&interposed_key, incarnation, 0);
    });

    let authorization = router
        .field_verdicts()
        .preflight_authorization(&key, generation, Instant::now())
        .expect("the acting verdict did not move, so the authorization still returns");

    assert_eq!(
        landed.load(Ordering::SeqCst),
        1,
        "premise: the interposition ran, so the window under test was reached",
    );
    assert!(
        authorization.confirmations >= crate::config::PREFIX_QUORUM,
        "the authorization must carry the count its own eligibility decision was \
         made from: a re-read would report the interposed zero and gate a \
         confirmed verdict as below_quorum. Got {authorization:?}",
    );
}

// ---------------------------------------------------------------------------
// Cadence fairness: a later row is delayed, never starved
// ---------------------------------------------------------------------------

/// Dispatch `count` complete requests carrying BOTH rows, returning which
/// request index (1-based) each row's canary was restored on.
///
/// Driven through the real planner rather than the registry, because the
/// property is about the PLANNER's per-row cadence handling: the registry alone
/// cannot show that a request whose settlement slot one row owns still advances
/// the other.
fn canary_restorations_over(router: &Router, state_key: &str, count: u32) -> (Vec<u32>, Vec<u32>) {
    let target = target_for(router, state_key, "p0");
    let mut envelope_on = Vec::new();
    let mut prefix_on = Vec::new();
    for request in 1..=count {
        let (_planned, records, plan) =
            router.plan_field_preflight(&req_both_rows(ALIAS), &target, DispatchSurface::Complete);
        for record in &records {
            if record.reason != FIELD_PREFLIGHT_CANARY_RESTORED {
                continue;
            }
            match record.field_path {
                Some(GROUNDED_PATH) => envelope_on.push(request),
                Some(PREFIX_PATH) => prefix_on.push(request),
                other => panic!("unexpected restored path: {other:?}"),
            }
        }
        // Dropping the plan settles any canary INCONCLUSIVE, which releases the
        // claim and leaves both verdicts exactly as they were -- the shape of a
        // walk that reached no outcome. That is what lets this loop run many
        // intervals without a settlement moving a verdict under it.
        drop(plan);
    }
    (envelope_on, prefix_on)
}

#[test]
fn every_eligible_request_advances_both_rows_cadences() {
    // THE STARVATION PIN. Both rows are eligible and both act, so each request
    // offers the sole settlement slot to whichever row reaches it first -- the
    // envelope row, since it is first in the closed table. If a row denied that
    // slot also skipped its own cadence tick, the prefix row would NEVER reach
    // its interval: it is denied on every request, so it would advance zero
    // times across any number of requests.
    //
    // Run past TWO intervals so the assertion is about a repeating cadence
    // rather than about one lucky boundary.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (envelope_on, prefix_on) = canary_restorations_over(&router, "m0", CANARY_INTERVAL * 2 + 2);

    assert!(
        !prefix_on.is_empty(),
        "the LATER row must reach its own canary: a row that skipped its cadence \
         whenever the settlement slot was taken would advance zero times and never \
         be re-verified at all. Envelope restorations were {envelope_on:?}",
    );
    assert!(
        !envelope_on.is_empty(),
        "control: the earlier row reaches its canary too, so the assertion above is \
         about fairness between the rows rather than about canaries never firing",
    );
    // No request restores both: the arm settles one plan, and the claim is
    // offered only while the slot is free.
    let both: Vec<u32> = envelope_on
        .iter()
        .copied()
        .filter(|request| prefix_on.contains(request))
        .collect();
    assert!(
        both.is_empty(),
        "exactly one canary per request, however many rows are due: {both:?}",
    );
    // Each row's own interval is the cadence constant: consecutive restorations
    // of ONE row are CANARY_INTERVAL apart, which is what shows the later row is
    // merely DELAYED rather than running on a different cadence.
    for restorations in [&envelope_on, &prefix_on] {
        for pair in restorations.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                CANARY_INTERVAL,
                "each row keeps the shared interval between its own canaries: \
                 {restorations:?}",
            );
        }
    }
}

#[test]
fn a_row_denied_the_settlement_slot_claims_on_the_next_request() {
    // The DELAY is exactly one request, not another interval. Both rows come due
    // on the same request (their intervals are aligned by planting both at once
    // and driving them together), so the later row is denied once and must claim
    // on the very next eligible request -- the sticky due flag is what makes that
    // hold, and without it the row would wait out a further full interval.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (envelope_on, prefix_on) = canary_restorations_over(&router, "m0", CANARY_INTERVAL + 2);

    assert_eq!(
        envelope_on,
        vec![CANARY_INTERVAL],
        "the earlier row claims on the interval boundary",
    );
    assert_eq!(
        prefix_on,
        vec![CANARY_INTERVAL + 1],
        "and the row it denied claims on the NEXT request -- a postponement of one \
         request. A non-sticky due flag would put this at {} instead, which is a \
         full interval later.",
        CANARY_INTERVAL * 2,
    );
}

#[test]
fn a_denied_row_stays_due_in_the_registry_until_it_claims() {
    // The registry-level half of the same property, read deterministically
    // rather than inferred from dispatch counts: after the request on which both
    // rows came due, the DENIED identity is still flagged due and the one that
    // claimed is not. This is what a mutation clearing `due` on the trip (rather
    // than on the claim) trips directly.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let target = target_for(&router, "m0", "p0");

    // Drive exactly to the interval boundary, holding no plan across requests.
    let mut restored_path = None;
    for _ in 1..=CANARY_INTERVAL {
        let (_planned, records, plan) =
            router.plan_field_preflight(&req_both_rows(ALIAS), &target, DispatchSurface::Complete);
        for record in &records {
            if record.reason == FIELD_PREFLIGHT_CANARY_RESTORED {
                restored_path = record.field_path;
            }
        }
        // Held until after the snapshot reads below on the LAST iteration would
        // change what they see, so the plan is dropped here deliberately: the
        // claim is released, but `due` is a separate flag the claim already
        // consumed, which is exactly the distinction under test.
        drop(plan);
    }
    assert_eq!(
        restored_path,
        Some(GROUNDED_PATH),
        "premise: the earlier row is the one that claimed",
    );

    let claimed = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key("m0"))
        .expect("the envelope identity is resident");
    let denied = router
        .field_verdicts()
        .canaries()
        .snapshot(&prefix_verdict_key("m0"))
        .expect("the prefix identity is resident");

    assert!(
        !claimed.due,
        "the row that CLAIMED has consumed its trip: {claimed:?}",
    );
    assert!(
        denied.due,
        "and the row that was DENIED is still due, so the next eligible request \
         claims it rather than waiting out another interval: {denied:?}",
    );
}

#[test]
fn one_canary_per_request_holds_under_hostile_concurrency() {
    // SCOPE, stated because the name of the thing under stress is easy to
    // overread. This is a PROBABILISTIC stress check of exactly one property:
    // that no single planning call restores two rows, whatever the interleaving
    // of many threads at an interval boundary. A second concurrent canary for one
    // identity is what the claim exists to prevent, and this is the shape that
    // can catch it.
    //
    // It does NOT pin sticky dueness, and must not be read as doing so. Sticky
    // dueness is owned by the DETERMINISTIC tests --
    // `an_unclaimed_due_interval_stays_due_until_a_claim_consumes_it`,
    // `a_denied_row_stays_due_in_the_registry_until_it_claims`, and
    // `a_row_denied_the_settlement_slot_claims_on_the_next_request` -- because a
    // racing thread makes either dueness outcome legal here, so an assertion
    // about it would be unfalsifiable. The restoration count below is a PREMISE
    // (the window was reached at all), not evidence about the cadence.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let router = Arc::new(router);
    // Bring both identities to the brink so every racing thread finds them due.
    for key in [verdict_key("m0"), prefix_verdict_key("m0")] {
        let snapshot = router
            .field_verdicts()
            .canaries()
            .snapshot(&key)
            .expect("resident");
        router.field_verdicts().canaries().seed_from_rebuild(
            &key,
            snapshot.incarnation,
            snapshot.confirmations,
            true,
        );
    }

    let restored_both = Arc::new(AtomicUsize::new(0));
    let restorations = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..64)
        .map(|_| {
            let router = Arc::clone(&router);
            let restored_both = Arc::clone(&restored_both);
            let restorations = Arc::clone(&restorations);
            std::thread::spawn(move || {
                let target = target_for(&router, "m0", "p0");
                let (_planned, records, plan) = router.plan_field_preflight(
                    &req_both_rows(ALIAS),
                    &target,
                    DispatchSurface::Complete,
                );
                let restored = records
                    .iter()
                    .filter(|r| r.reason == FIELD_PREFLIGHT_CANARY_RESTORED)
                    .count();
                if restored > 1 {
                    restored_both.fetch_add(1, Ordering::Relaxed);
                }
                restorations.fetch_add(restored, Ordering::Relaxed);
                drop(plan);
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("planning thread");
    }

    assert_eq!(
        restored_both.load(Ordering::Relaxed),
        0,
        "no request may restore two rows: the arm settles exactly one plan",
    );
    assert!(
        restorations.load(Ordering::Relaxed) >= 1,
        "premise only: at least one canary was claimed, so the ceiling assertion \
         above is about the one-per-request limit rather than about a window no \
         thread ever reached. This count says nothing about the cadence -- see the \
         scope note above for which tests own that",
    );
}

// ---------------------------------------------------------------------------
// Accounting: every acting row's guard outlives the planning call
// ---------------------------------------------------------------------------

#[test]
fn every_acting_rows_accounting_guard_is_held_for_the_request() {
    // THE ACCOUNTING PIN. Two rows act, so two identities each have ONE request
    // in flight while the plan lives -- and zero once it drops, because the
    // in-flight half is what RAII clears. A planner that kept only the first
    // row's guard would show the later identity at zero in flight while its
    // request was outstanding: the exposure an operator reads to size a disproof
    // would be silently short.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let target = target_for(&router, "m0", "p0");
    let keys = [verdict_key("m0"), prefix_verdict_key("m0")];

    let (planned, records, plan) =
        router.plan_field_preflight(&req_both_rows(ALIAS), &target, DispatchSurface::Complete);

    assert!(
        records.iter().all(|r| r.acted),
        "premise: both rows acted, so both owe accounting: {records:?}",
    );
    assert!(!carries_field(&planned) && !carries_system(&planned));
    assert_eq!(
        plan.accounting_len(),
        2,
        "one accounting guard per acting row, both retained",
    );
    for key in &keys {
        let snapshot = router
            .field_verdicts()
            .canaries()
            .snapshot(key)
            .expect("an acting identity is resident");
        assert_eq!(
            snapshot.outstanding, 1,
            "while the plan lives, each acting identity has exactly one request in \
             flight: {snapshot:?}",
        );
        assert_eq!(
            snapshot.modified_since_confirmation, 1,
            "and one request counted against its wrong-repair exposure",
        );
    }

    drop(plan);

    for key in &keys {
        let snapshot = router
            .field_verdicts()
            .canaries()
            .snapshot(key)
            .expect("resident");
        assert_eq!(
            snapshot.outstanding, 0,
            "the guard's Drop clears the IN-FLIGHT half for every row, not just the \
             first: {snapshot:?}",
        );
        assert_eq!(
            snapshot.modified_since_confirmation, 1,
            "but the wrong-repair TALLY survives the drop -- a finished request does \
             not un-modify the body it was sent with, and only a confirmation or a \
             disproof clears it",
        );
    }
}

#[test]
fn two_concurrent_both_rows_requests_each_count_against_both_identities() {
    // The tally is a COUNT, so a single-request test cannot distinguish "counted
    // once per acting row" from "counted once per request". Two simultaneous
    // requests make the two answers differ: each identity must see two in flight
    // and two modified.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let target = target_for(&router, "m0", "p0");

    let (_first_req, first_records, first) =
        router.plan_field_preflight(&req_both_rows(ALIAS), &target, DispatchSurface::Complete);
    let (_second_req, second_records, second) =
        router.plan_field_preflight(&req_both_rows(ALIAS), &target, DispatchSurface::Complete);
    assert!(
        first_records.iter().all(|r| r.acted) && second_records.iter().all(|r| r.acted),
        "premise: both rows acted on both requests",
    );

    for key in [verdict_key("m0"), prefix_verdict_key("m0")] {
        let snapshot = router
            .field_verdicts()
            .canaries()
            .snapshot(&key)
            .expect("resident");
        assert_eq!(
            snapshot.outstanding, 2,
            "two live requests, so two in flight per identity: {snapshot:?}",
        );
        assert_eq!(snapshot.modified_since_confirmation, 2);
    }

    drop(first);
    for key in [verdict_key("m0"), prefix_verdict_key("m0")] {
        let snapshot = router
            .field_verdicts()
            .canaries()
            .snapshot(&key)
            .expect("resident");
        assert_eq!(
            snapshot.outstanding, 1,
            "dropping ONE request's plan clears exactly its own entries: {snapshot:?}",
        );
    }
    drop(second);
}

// ---------------------------------------------------------------------------
// The test-only row is scoped to the fixtures that opt into it
// ---------------------------------------------------------------------------

#[test]
fn an_ordinary_system_prompt_carries_no_test_only_closed_table_row() {
    // The SCOPING pin for the test-only prefix row. Its surface matches one exact
    // sentinel, so a request with any other system prompt -- which is most
    // fixtures in this workspace -- must see the production table alone: one row,
    // one feature key, one decision. A broad `req.system.is_some()` predicate
    // would give every such fixture a closed-table row production cannot produce,
    // and a sibling module's "one decision" assertion would silently be about two.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let mut ordinary = req_on(ALIAS);
    ordinary.system = Some(routectl_core::SystemContent::Text(
        "you are a careful assistant".into(),
    ));
    assert!(
        !carries_system(&ordinary),
        "premise: an ordinary prompt is not the sentinel, so the test-only surface \
         does not recognize it",
    );

    // The FEATURE KEYS the request grounds: the envelope row's alone.
    let keys = super::super::field_repair::grounded_field_feature_keys(&ordinary);
    assert_eq!(
        keys,
        vec![grounded_key()],
        "an ordinary system prompt grounds only the production row's key",
    );
    assert!(
        !keys.contains(&prefix_key()),
        "and never the test-only row's capability key",
    );

    // And the PLANNER produces one decision, for that row.
    let (_planned, records) = plan_all(&router, &ordinary, "m0");
    assert_eq!(
        records.len(),
        1,
        "one present row, one decision: {records:?}",
    );
    assert_eq!(records[0].field_path, Some(GROUNDED_PATH));

    // Positive control: the SENTINEL prompt does carry the row, so the negatives
    // above are about the predicate rather than about a row that never matches.
    let opted_in = req_both_rows(ALIAS);
    assert_eq!(
        super::super::field_repair::grounded_field_feature_keys(&opted_in).len(),
        2,
        "control: the sentinel prompt grounds both rows' keys",
    );
}

// ---------------------------------------------------------------------------
// The double-apply guard refuses a second, differently-named rejection
// ---------------------------------------------------------------------------

#[test]
fn a_second_rejection_naming_a_different_present_row_is_refused() {
    // THE DOUBLE-APPLY PIN, and the shape that makes it non-vacuous: the second
    // rejection names a DIFFERENT row that is still present and still admitted,
    // so every reason to refuse except the one-repair ceiling is absent. A guard
    // removed would repair it -- mutating a body the arm has already
    // re-dispatched, charging a second draw against the request's one allowance,
    // and re-pointing `repaired_path` so the commit would learn the wrong
    // identity.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let original = req_both_rows(ALIAS);
    let mut attempt = original.clone();
    let mut plan = carry_for(&router, &original, "m0");
    let mut budget = RepairBudget::per_request();
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);

    // First apply: the ENVELOPE row, named.
    let first = {
        let _injection = super::super::field_repair::provisional::inject(GROUNDED_PATH);
        plan.apply(
            &mut attempt,
            &mut meta,
            &mut budget,
            &routectl_core::failure_class::FailureClass::BadRequest,
            &Error::upstream("p", 400, "{}"),
            ANTHROPIC,
        )
    };
    assert!(first.is_some(), "premise: the first rejection repaired");
    assert_eq!(plan.repaired_path(), Some(GROUNDED_PATH));
    assert!(
        !carries_field(&attempt),
        "premise: that row's field is gone"
    );
    assert!(
        carries_system(&attempt),
        "premise: the OTHER row's surface is still present, so a second apply has \
         something it could mutate",
    );
    let body_after_first = serde_json::to_value(&attempt).unwrap();
    // Read rather than drained: draining a copy would need a cloneable budget,
    // and a budget a caller can copy is one it can spend twice.
    let remaining_after_first = budget.remaining();

    // Second apply: the PREFIX row, named, present, admitted.
    let second = {
        let _injection = super::super::field_repair::provisional::inject(PREFIX_PATH);
        plan.apply(
            &mut attempt,
            &mut meta,
            &mut budget,
            &routectl_core::failure_class::FailureClass::BadRequest,
            &Error::upstream("p", 400, "{}"),
            ANTHROPIC,
        )
    };

    assert!(
        second.is_none(),
        "one repair per attempt: a plan that already repaired must refuse a second, \
         however well named the rejection",
    );
    assert_eq!(
        serde_json::to_value(&attempt).unwrap(),
        body_after_first,
        "and must mutate NOTHING: the arm has already re-dispatched this body",
    );
    assert!(
        carries_system(&attempt),
        "the second row's surface survives, which is the mutation a removed guard \
         would have made",
    );
    assert_eq!(
        budget.remaining(),
        remaining_after_first,
        "and must draw NO further budget: the refusal precedes the charge",
    );
    assert_eq!(
        plan.repaired_path(),
        Some(GROUNDED_PATH),
        "and the learned attribution still names the FIRST row: a re-pointed path \
         would have the commit persist a verdict for a row this attempt never \
         repaired",
    );
}

// ---------------------------------------------------------------------------
// The request WARN names the highest-impact acting class
// ---------------------------------------------------------------------------

#[test]
fn the_request_warn_names_the_prefix_impacting_class_when_both_rows_act() {
    // One WARN stands for the whole request, so it must name the WORST thing that
    // happened rather than the first. Both rows act here, and the envelope row is
    // FIRST in the closed table -- so an emitter naming the first acting decision
    // would report `envelope` and understate a request that rewrote a cache
    // prefix. The aggregate counts still describe both decisions.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_both_rows(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);
    assert_eq!(debugs.len(), 2, "premise: both rows recorded a decision");
    assert_eq!(warns.len(), 1, "still exactly ONE request-level WARN");
    assert_eq!(
        field_of(warns[0], "transform_class"),
        Some("prefix_impacting"),
        "the WARN names the highest-impact acting class, not the table-first one",
    );
    assert_eq!(
        field_of(warns[0], "field_path"),
        Some(PREFIX_PATH),
        "and the path it names is that decision's own",
    );
    assert_eq!(
        field_of(warns[0], "decisions_acted"),
        Some("2"),
        "the aggregate counts still describe every decision, so naming the worst one \
         hides nothing",
    );
    assert_eq!(field_of(warns[0], "decisions_planned"), Some("2"));
    // No request values at either tier, on the class whose dropped value is a
    // prompt rather than an enum token.
    let rendered = format!("{events:#?}");
    assert!(
        !rendered.contains(SYSTEM_PROMPT),
        "no tier may carry the dropped system prompt",
    );
}

#[test]
fn the_request_warn_names_the_envelope_class_when_it_is_the_only_acting_row() {
    // The control for the case above: with only the envelope row acting, the
    // WARN names `envelope`. Without this, an emitter hard-coded to
    // `prefix_impacting` would satisfy the both-rows assertion.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_both_rows(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (_debugs, warns) = captured_diagnostics(&events);
    assert_eq!(warns.len(), 1);
    assert_eq!(
        field_of(warns[0], "transform_class"),
        Some("envelope"),
        "with no opt-in the prefix row is blocked, so the only ACTING class is the \
         envelope one and the WARN names it",
    );
    assert_eq!(
        field_of(warns[0], "decisions_acted"),
        Some("1"),
        "one acting decision of two planned",
    );
    assert_eq!(field_of(warns[0], "decisions_planned"), Some("2"));
}

#[test]
fn the_warn_impact_rank_orders_the_classes_strictly_and_floors_the_unknown() {
    // The rank function tested DIRECTLY, not through an emitted line. A
    // max-selecting adapter reports the right class for the wrong reason whenever
    // table order happens to favor it: this test was written against a call site
    // that used `max_by_key`, where a rank function flattened to a constant still
    // passed the both-rows trace test because the prefix row came last and
    // `max_by_key` returns the last maximum. The call site now selects through
    // `warn_headline`, which closes that particular hole -- but the general point
    // stands, since any selector's answer depends jointly on the ranks and the
    // order. Read here, the ordering is the whole assertion and a flattened
    // function has nowhere to hide.
    use super::super::field_repair::TransformClass;
    use super::warn_impact_rank;

    let prefix = warn_impact_rank(Some(TransformClass::PrefixImpacting.as_str()));
    let envelope = warn_impact_rank(Some(TransformClass::Envelope.as_str()));
    let unknown_token = warn_impact_rank(Some("some_future_class"));
    let no_class = warn_impact_rank(None);

    assert!(
        prefix > envelope,
        "a content rewrite outranks an envelope rewrite: a wrong prefix verdict \
         degrades every later request on the lane, a wrong envelope verdict costs \
         one field. prefix={prefix}, envelope={envelope}",
    );
    assert!(
        envelope > unknown_token,
        "and a KNOWN class outranks an unrecognized token: a class added without \
         revisiting this function must under-report rather than silently outrank a \
         prefix rewrite. envelope={envelope}, unknown={unknown_token}",
    );
    assert_eq!(
        unknown_token, 0,
        "an unrecognized token ranks at the FLOOR rather than panicking -- a \
         diagnostic must not become the thing that fails",
    );
    assert_eq!(
        no_class, 0,
        "and a decision that considered no row at all ranks the same, since it \
         names no class to rank",
    );
}

#[test]
fn the_warn_headline_takes_the_worst_and_breaks_ties_on_planning_order() {
    // The tie rule, tested directly on the selector, over the ORDERS a dispatch
    // cannot conveniently arrange.
    //
    // A same-class tie is unreachable WITHIN one target -- today's closed table
    // holds one row per class, so one target contributes at most one decision of
    // each. It is entirely reachable ACROSS targets: a two-seat chain whose seats
    // both act on the same class produces one on every such request, which is what
    // `the_request_warn_names_the_first_seat_when_two_seats_act_on_one_class`
    // exercises end to end. This test adds the orders that test cannot vary --
    // reversed slices, and the worst decision in the middle of three.
    //
    // `max_by_key` is documented to return the LAST maximum, so the previous call
    // site was deterministic but named the wrong end: the DEBUG tier emits in
    // planning order, so an operator scanning it for the `state_key` the WARN
    // named should land on the FIRST match.
    use super::super::field_repair::TransformClass;
    use super::warn_headline;

    fn acting(state_key: &str, path: &'static str, class: TransformClass) -> FieldPreflight {
        FieldPreflight {
            acted: true,
            state_key: state_key.to_string(),
            field_path: Some(path),
            // Built THROUGH the enum rather than from a literal, so a renamed
            // token cannot leave this test asserting about a class the emitter no
            // longer produces.
            transform_class: Some(class.as_str()),
            reason: FIELD_PREFLIGHT_ACTION_DROP,
        }
    }

    // A same-class tie: the FIRST planned wins, both orders.
    let first = acting("seat-a", GROUNDED_PATH, TransformClass::Envelope);
    let second = acting("seat-b", GROUNDED_PATH, TransformClass::Envelope);
    let chosen = warn_headline(&[&first, &second]).expect("non-empty");
    assert_eq!(
        chosen.state_key, "seat-a",
        "among equal-impact decisions the FIRST planned is named",
    );
    let reversed = warn_headline(&[&second, &first]).expect("non-empty");
    assert_eq!(
        reversed.state_key, "seat-b",
        "and it is genuinely positional rather than a property of the records: \
         reversing the slice reverses the answer",
    );

    // Impact OUTRANKS position, in both orders -- so first-acted-wins is a
    // tie-breaker and never overrides the ranking.
    let envelope = acting("seat-a", GROUNDED_PATH, TransformClass::Envelope);
    let prefix = acting("seat-b", PREFIX_PATH, TransformClass::PrefixImpacting);
    let worst = Some(TransformClass::PrefixImpacting.as_str());
    assert_eq!(
        warn_headline(&[&envelope, &prefix])
            .expect("non-empty")
            .transform_class,
        worst,
        "a higher-impact decision planned SECOND still wins",
    );
    assert_eq!(
        warn_headline(&[&prefix, &envelope])
            .expect("non-empty")
            .transform_class,
        worst,
        "and planned FIRST it wins too, so the ranking is not an artifact of order",
    );

    // Three decisions, the worst in the middle: neither a first-element nor a
    // last-element shortcut satisfies this.
    let middle = warn_headline(&[&envelope, &prefix, &second]).expect("non-empty");
    assert_eq!(
        middle.transform_class, worst,
        "the worst decision is named wherever in the order it sits",
    );

    assert!(
        warn_headline(&[]).is_none(),
        "an empty slice names nothing -- the caller skips the WARN entirely rather \
         than emitting a line about no decision",
    );
}

#[test]
fn the_request_warn_names_the_first_seat_when_two_seats_act_on_one_class() {
    // THE END-TO-END TIE, through a real dispatch rather than hand-built records.
    // A same-class tie is unreachable within ONE target -- today's closed table
    // holds one row per class -- but entirely reachable ACROSS targets: both seats
    // here carry eligible verdicts for the SAME envelope row, so both act and both
    // decisions rank equally.
    //
    // The DEBUG tier proves the planning order (m0 then m1), and the WARN must name
    // m0. Reverting the call site to `max_by_key` names m1 instead -- deterministic,
    // but the wrong end: an operator scanning the DEBUG lines for the state key the
    // WARN reported would skip past the first acting decision.
    //
    // `Unavailable` on both seats is what makes both seats plan: the first fails
    // after its rewrite, so the chain advances and the second plans too.
    let (router, _seen) = chain_of(2, Answer::Unavailable);
    plant_eligible(&router, "m0");
    plant_eligible(&router, "m1");

    let events = routectl_testkit::capture_events(|| {
        block_on_dispatch(async {
            let _ = router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await;
        });
    });

    let (debugs, warns) = captured_diagnostics(&events);

    // The DEBUG tier establishes the planning order this test's claim rests on.
    let planned: Vec<Option<&str>> = debugs
        .iter()
        .map(|event| field_of(event, "state_key"))
        .collect();
    assert_eq!(
        planned,
        vec![Some("m0"), Some("m1")],
        "premise: the walk planned m0 before m1, so 'first planned' means m0",
    );
    let acted_classes: Vec<Option<&str>> = debugs
        .iter()
        .map(|event| field_of(event, "transform_class"))
        .collect();
    assert_eq!(
        acted_classes,
        vec![Some("envelope"), Some("envelope")],
        "premise: both decisions are of the SAME class, which is what makes this a \
         tie rather than a ranking",
    );
    for event in &debugs {
        assert_eq!(
            field_of(event, "acted"),
            Some("true"),
            "premise: both seats ACTED, so both are candidates for the headline",
        );
    }

    // The WARN names the FIRST of the two.
    assert_eq!(warns.len(), 1, "still exactly one request-level WARN");
    assert_eq!(
        field_of(warns[0], "state_key"),
        Some("m0"),
        "among equal-impact decisions the WARN names the FIRST planned, so it lines \
         up with the first matching DEBUG line. `max_by_key` would name m1",
    );
    assert_eq!(
        field_of(warns[0], "transform_class"),
        Some("envelope"),
        "and the class it reports is that decision's own",
    );
    // The aggregate counts still describe BOTH decisions -- naming one never
    // narrows them.
    assert_eq!(field_of(warns[0], "decisions_acted"), Some("2"));
    assert_eq!(field_of(warns[0], "decisions_planned"), Some("2"));
}
