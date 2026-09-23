//! Behavioral coverage of the re-verification canary: the cadence that makes
//! one request in a hundred carry the tested field unrepaired, the three
//! settlements that request can reach, and the wrong-repair accounting a
//! disproof charges.
//!
//! # Why eligibility and cadence are PLANTED rather than earned
//!
//! A verdict becomes pre-flight eligible only when it is resident, ACTING, and
//! its own incarnation carries an acknowledged confirmation -- which live
//! traffic cannot produce today (see `field_preflight_tests`). These tests
//! plant both halves through the registries' OWN seams, and where a test needs
//! a canary DUE without dispatching a hundred requests it uses the cold-boot
//! seed's `due_immediately` flag, which is the production writer for exactly
//! that state. The one test that pins the cadence NUMBER dispatches all
//! hundred, because a seeded shortcut cannot pin the number it skips.
//!
//! # Why the canary is observed on the dispatched bytes
//!
//! "Restores the field under test" is a claim about what reached the upstream,
//! so every assertion here reads the mock seat's recorded request bodies. A
//! record on the dispatch metadata says what the planner DECIDED; only the
//! bytes say what it did.

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

use super::super::{Router, RouterOptions};
use super::field_preflight::{FIELD_PREFLIGHT_CANARY_RESTORED, FIELD_PREFLIGHT_NOT_ELIGIBLE};

use crate::config::{AliasValue, CANARY_INTERVAL, Config};
use crate::field_canary::CanaryOutcome;
use crate::field_verdict::FieldVerdictKey;
use crate::resolved::ResolvedModel;

/// The one grounded row in the closed table.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// The lane this stage acts on.
const ANTHROPIC: &str = "anthropic-api";

/// The alias every fixture dispatches through. Deliberately not a model name:
/// an alias that is also a chain member re-resolves through itself.
const ALIAS: &str = "field-canary-alias";

/// How long a planted verdict stays unexpired.
const NOT_LAPSED: Duration = Duration::from_hours(1);

/// A rejection body shaped like the captured envelope. Carries no secret, and
/// is NOT what makes the repair fire -- the fixture injects the resolution.
const FIELD_REJECT_BODY: &str = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;

fn grounded_key() -> String {
    crate::field_capability::field_capability_key(GROUNDED_PATH)
        .expect("the grounded path is a well-formed qualified path")
}

fn verdict_key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(state_key, GROUNDED_PATH, ANTHROPIC).expect("a qualified path mints a key")
}

/// Whether a request still emits the grounded wire field through EITHER
/// canonical carrier.
fn carries_field(req: &ChatRequest) -> bool {
    req.routectl_internal.anthropic_thinking_display.is_some()
        || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
}

/// A request carrying the grounded field through BOTH canonical carriers.
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

// --- routers and targets -----------------------------------------------

fn chain_config(seats: usize) -> Config {
    let mut toml_text = String::new();
    let mut chain: Vec<String> = Vec::with_capacity(seats);
    for idx in 0..seats {
        toml_text.push_str(&format!(
            "\n[providers.p{idx}]\nkind = \"{ANTHROPIC}\"\napi_key_ref = \"literal:k\"\n"
        ));
        chain.push(format!("m{idx}"));
    }
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(chain));
    config
}

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
    // DURABLE PERSISTENCE IS ASSUMED, stated rather than defaulted: learned
    // pre-flight suspends unless a capability-persistence health read reports
    // durable writes, and `Router::new` installs none. Without this every canary
    // assertion in this file would run against a suspended planner -- no
    // restoration, no cadence, no settlement -- and pass for the wrong reason.
    (
        router.with_capability_writes_assumed_durable_for_tests(),
        observed,
    )
}

fn single_seat(answer: Answer) -> (Router, Arc<Observed>) {
    let (router, mut observed) = install(chain_config(1), 1, answer);
    (router, observed.remove(0))
}

// --- eligibility planting ----------------------------------------------

/// Plant a resident field negative through the registry's own carry-over
/// import seam.
fn plant_verdict(router: &Router, state_key: &str) {
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
            expires_at: stamped + NOT_LAPSED,
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

fn resident_incarnation(router: &Router, state_key: &str) -> u64 {
    router.learned_capabilities.resident_incarnation_for_tests(
        state_key,
        &grounded_key(),
        ANTHROPIC,
    )
}

/// Plant a pre-flight eligible verdict whose canary cadence starts at the full
/// interval -- the steady state a hundred requests walk through.
fn plant_eligible(router: &Router, state_key: &str) {
    plant_verdict(router, state_key);
    let incarnation = resident_incarnation(router, state_key);
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key(state_key),
        incarnation,
        1,
        false,
    );
    assert!(
        router.field_verdicts().preflight_eligible(
            &verdict_key(state_key),
            router.registry_generation(),
            Instant::now(),
        ),
        "fixture premise: {state_key} must be pre-flight eligible",
    );
}

/// The same, with the cadence forced DUE on the very next eligible request --
/// through the cold-boot seed's own `due_immediately` flag, which is
/// production's writer for that state.
fn plant_eligible_with_canary_due(router: &Router, state_key: &str) {
    plant_verdict(router, state_key);
    let incarnation = resident_incarnation(router, state_key);
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key(state_key),
        incarnation,
        1,
        true,
    );
    let snap = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key(state_key))
        .expect("seeded");
    assert_eq!(
        snap.cadence, 1,
        "fixture premise: the next eligible request trips the canary",
    );
}

fn canary_snapshot(
    router: &Router,
    state_key: &str,
) -> Option<crate::field_canary::CanaryStateSnapshot> {
    router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key(state_key))
}

// --- mock seat ----------------------------------------------------------

#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    dispatched: Mutex<Vec<ChatRequest>>,
}

impl Observed {
    fn record(&self, req: &ChatRequest) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let carried = carries_field(req);
        self.dispatched.lock().push(req.clone());
        carried
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Per attempt, in order: whether it still carried the grounded field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.dispatched.lock().iter().map(carries_field).collect()
    }

    /// How many attempts carried the field -- the canary count, since every
    /// other attempt on an eligible identity is pre-flight repaired.
    fn attempts_carrying_the_field(&self) -> usize {
        self.dispatched
            .lock()
            .iter()
            .filter(|req| carries_field(req))
            .count()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Serve every attempt. An unrepaired canary therefore SUCCEEDS, which is
    /// the disproof shape.
    ServeEverything,
    /// Reject an attempt carrying the field with the field rejection and serve
    /// the repaired one -- the confirmation shape.
    ServeRepaired,
    /// Reject the carried attempt with the field rejection and reject the
    /// repaired retry too, with an unrelated fault: the repaired-retry-failed
    /// shape, which proves nothing.
    RejectRepairedRetry,
    /// Reject with a 400 carrying NO field attribution: a terminal caller
    /// error unrelated to the tested field.
    UnrelatedBadRequest,
    /// Reject with a 503: an availability failure, which proves nothing.
    Unavailable,
    /// Never answer, so a dropped future leaves the claim to RAII.
    Hang,
}

struct MockSeat {
    answer: Answer,
    observed: Arc<Observed>,
}

impl MockSeat {
    const fn new(answer: Answer, observed: Arc<Observed>) -> Self {
        Self { answer, observed }
    }

    async fn answer_for(&self, req: &ChatRequest) -> Result<()> {
        let carried = self.observed.record(req);
        match self.answer {
            Answer::ServeEverything => Ok(()),
            Answer::ServeRepaired if carried => {
                Err(Error::upstream("canary-mock", 400, FIELD_REJECT_BODY))
            }
            Answer::ServeRepaired => Ok(()),
            Answer::RejectRepairedRetry if carried => {
                Err(Error::upstream("canary-mock", 400, FIELD_REJECT_BODY))
            }
            Answer::RejectRepairedRetry => Err(Error::upstream(
                "canary-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
            Answer::UnrelatedBadRequest => Err(Error::upstream(
                "canary-mock",
                400,
                r#"{"error":{"type":"invalid_request_error","message":"max_tokens is required"}}"#,
            )),
            Answer::Unavailable => Err(Error::upstream(
                "canary-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
            Answer::Hang => {
                // Longer than any test's own timeout: the point is that the
                // caller's future is dropped while this attempt is in flight.
                tokio::time::sleep(Duration::from_hours(1)).await;
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockSeat {
    fn id(&self) -> &'static str {
        "canary-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("canary-mock", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer_for(&req).await.map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer_for(&req).await.map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.answer_for(&req).await.map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

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

/// Make every field rejection on this thread resolve to the grounded path, the
/// way `field_repair_tests` does: the envelope-to-path parser is out of this
/// stage, so a behavioral test of everything downstream of it injects the
/// resolution.
fn inject_resolution() -> super::field_repair::provisional::Injection {
    super::field_repair::provisional::inject(GROUNDED_PATH)
}

// ---------------------------------------------------------------------------
// Cadence: 99 repair, the hundredth restores the field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ninety_nine_eligible_completes_repair_and_the_hundredth_restores_the_field() {
    // The cadence NUMBER, pinned by dispatching all hundred rather than by
    // seeding the due state: a seeded shortcut cannot pin the count it skips.
    // Mutation check for the constant -- change 100 to 99 and the carried
    // attempt lands at index 98 instead of 99, failing both halves.
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");

    for _ in 0..CANARY_INTERVAL {
        let dispatched = router
            .complete_with_options(req_on(ALIAS), RouterOptions::new())
            .await;
        assert!(dispatched.result.is_ok(), "every request in the run serves");
    }

    let carried = seen.carried_per_attempt();
    assert_eq!(
        carried.len(),
        CANARY_INTERVAL as usize,
        "one attempt per request: no request retried",
    );
    let canary_positions: Vec<usize> = carried
        .iter()
        .enumerate()
        .filter_map(|(idx, c)| c.then_some(idx))
        .collect();
    assert_eq!(
        canary_positions,
        vec![CANARY_INTERVAL as usize - 1],
        "exactly the hundredth request carried the tested field unrepaired; every \
         earlier one was repaired before dispatch",
    );
}

#[tokio::test]
async fn the_canary_request_is_recorded_as_a_restoration_not_as_a_rewrite() {
    let (router, _seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    let record = dispatched
        .meta
        .field_preflight
        .first()
        .expect("the walk records its decision");
    assert!(
        !record.acted,
        "a canary rewrote nothing, so claiming it acted would be a false claim about \
         the dispatched bytes",
    );
    assert_eq!(record.reason, FIELD_PREFLIGHT_CANARY_RESTORED);
    assert_eq!(record.field_path, Some(GROUNDED_PATH));
}

#[tokio::test]
async fn a_canary_restores_only_the_field_under_test() {
    // The rest of the request must survive the restoration untouched -- a
    // canary that also reverted an unrelated part of the envelope would be
    // testing something other than the field.
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");
    let original = req_on(ALIAS);

    let dispatched = router
        .complete_with_options(original.clone(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok());
    let sent = seen.dispatched.lock()[0].clone();
    assert!(carries_field(&sent), "the tested field is restored");
    assert_eq!(
        serde_json::to_value(&sent.messages).unwrap(),
        serde_json::to_value(&original.messages).unwrap(),
        "and nothing else in the envelope moved",
    );
    assert_eq!(
        sent.reasoning.as_ref().and_then(|r| r.max_tokens),
        Some(2048),
        "an unrelated reasoning field is carried through",
    );
}

// ---------------------------------------------------------------------------
// Only the completion surface advances the cadence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streaming_request_never_advances_the_cadence() {
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");
    let before = canary_snapshot(&router, "m0").expect("seeded").cadence;

    for _ in 0..5 {
        let dispatched = router
            .stream_with_options(req_on(ALIAS), RouterOptions::new())
            .await;
        assert!(dispatched.result.is_ok());
    }

    assert_eq!(
        canary_snapshot(&router, "m0").expect("resident").cadence,
        before,
        "streaming requests are repaired but never counted toward the canary interval",
    );
    assert!(
        seen.carried_per_attempt().iter().all(|c| !*c),
        "and every one of them dispatched the repaired body",
    );
}

#[tokio::test]
async fn a_token_count_never_advances_the_cadence() {
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");
    let before = canary_snapshot(&router, "m0").expect("seeded").cadence;

    for _ in 0..5 {
        assert!(
            router
                .count_tokens_with_meta(req_on(ALIAS))
                .await
                .result
                .is_ok()
        );
    }

    assert_eq!(
        canary_snapshot(&router, "m0").expect("resident").cadence,
        before,
        "token counts are repaired but never counted toward the canary interval",
    );
    assert!(seen.carried_per_attempt().iter().all(|c| !*c));
}

#[tokio::test]
async fn a_stream_with_the_canary_due_still_repairs_rather_than_claiming_the_slot() {
    // The sharper case: the countdown is AT one, so a surface that advanced it
    // would claim the canary here. The stream must still repair, and the claim
    // must be left for the next completion.
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");

    let streamed = router
        .stream_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(streamed.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![false],
        "a due canary is not a stream's to claim",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert!(!snap.canary_claimed, "no claim was taken");
    assert_eq!(snap.cadence, 1, "and the canary is still due");
}

// ---------------------------------------------------------------------------
// Unrepaired success: the verdict is durably cleared
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unrepaired_canary_success_clears_the_verdict_and_the_next_request_forwards_unchanged() {
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");

    let canary = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(canary.result.is_ok(), "the unrepaired field was accepted");
    assert_eq!(
        canary.meta.cleared_capabilities.len(),
        1,
        "the clear rides out on the meta so the ledger records it and a warm rebuild \
         cannot resurrect the verdict",
    );
    assert_eq!(
        canary.meta.cleared_capabilities[0].capability_key,
        grounded_key(),
    );
    assert!(
        !router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "the resident verdict is gone, not merely suspended",
    );

    // The next request is the observable consequence: nothing is rewritten.
    let next = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(next.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, true],
        "with the verdict cleared the client's field forwards unchanged",
    );
    assert_eq!(
        next.meta
            .field_preflight
            .first()
            .expect("a decision is still recorded")
            .reason,
        FIELD_PREFLIGHT_NOT_ELIGIBLE,
        "and the planner reports the verdict as gone rather than silently doing nothing",
    );
}

#[tokio::test]
async fn a_disproof_charges_the_alarm_every_request_the_verdict_repaired() {
    // The accounting claim, end to end and at full cadence: ninety-nine
    // requests were modified by a verdict the hundredth disproved, so the
    // lifetime alarm counts ninety-nine -- not one canary attempt, and not
    // zero.
    let (router, _seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");

    for _ in 0..CANARY_INTERVAL {
        assert!(
            router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await
                .result
                .is_ok()
        );
    }

    let canaries = router.field_verdicts().canaries();
    assert_eq!(
        canaries.disproved_requests_total(),
        u64::from(CANARY_INTERVAL) - 1,
        "every request the disproved verdict modified is charged, and the canary \
         request itself -- which was NOT modified -- is not",
    );
    assert_eq!(
        canaries.outstanding_unconfirmed_total(),
        0,
        "and nothing is left outstanding: the tally moved rather than being copied",
    );
}

#[tokio::test]
async fn the_outstanding_count_tracks_requests_modified_since_the_last_confirmation() {
    let (router, _seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");

    for _ in 0..3 {
        assert!(
            router
                .complete_with_options(req_on(ALIAS), RouterOptions::new())
                .await
                .result
                .is_ok()
        );
    }

    assert_eq!(
        router
            .field_verdicts()
            .canaries()
            .outstanding_unconfirmed_total(),
        3,
        "three requests are riding on a verdict no canary has re-confirmed",
    );
    assert_eq!(
        canary_snapshot(&router, "m0")
            .expect("resident")
            .outstanding,
        0,
        "and none of them is still in flight -- the two counts answer different questions",
    );
}

// ---------------------------------------------------------------------------
// Same-field rejection plus a successful repaired retry is a confirmation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_field_rejection_repairs_and_re_confirms_the_verdict() {
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeRepaired);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "the repaired retry served, so the request succeeded",
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "the canary carried the field, drew the same rejection, and the repaired \
         retry went out on the SAME target",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Confirmed));
    assert_eq!(
        snap.cadence, CANARY_INTERVAL,
        "a confirmation restarts the full interval",
    );
    assert!(!snap.canary_claimed, "and releases the claim");
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "the verdict stands: the upstream still refuses the field",
    );
    assert!(
        dispatched.meta.cleared_capabilities.is_empty(),
        "a confirmation clears nothing",
    );
    assert_eq!(
        dispatched.meta.learned_capabilities.len(),
        1,
        "and it rides a learn row out so the ledger records the confirmation",
    );
}

/// The consequence of a confirmation, and the reason the confirmation must carry
/// its identity onto the incarnation its own re-observation minted: the NEXT
/// request still pre-flights.
///
/// Canary state left on the superseded incarnation does not merely cost a round
/// trip. Pre-flight refuses the identity on the incarnation match, and the
/// reactive repair path refuses it too because the verdict is ACTING -- so the
/// field reaches the upstream unrepaired and the request TERMINATES on the very
/// rejection the verdict exists to avoid.
///
/// Mutation check: replace `settle_confirmed_and_carry` with a plain
/// `settle(Confirmed)` in `record_canary_confirmation` and this goes red on the
/// follow-up's own result.
#[tokio::test]
async fn a_confirmed_verdict_still_pre_flights_the_following_request() {
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeRepaired);
    plant_eligible_with_canary_due(&router, "m0");

    // The canary itself: carries the field, draws the rejection, repaired retry
    // serves. This is the request that mints the new incarnation.
    let canary = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(canary.result.is_ok(), "premise: the canary confirmed");

    let next = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        next.result.is_ok(),
        "the follow-up must succeed: a verdict stranded on the superseded \
         incarnation is refused by pre-flight AND by reactive repair, so the \
         field goes out unrepaired and the request dies on the rejection",
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false, false],
        "and it is rewritten PRE-FLIGHT: one attempt, field already stripped, no \
         rejection round trip",
    );
}

#[tokio::test]
async fn the_canary_repaired_retry_runs_before_the_terminal_four_xx_path() {
    // The ORDERING claim, and the client's own result is what discriminates it:
    // a 400 is a terminal, non-fallbackable class, so a canary retry placed
    // AFTER the terminal handling would never run at all and the canary's own
    // provoked rejection would be returned to the client verbatim. `Ok` versus
    // `Err(400)` is therefore the whole test -- and it is a mutation check on
    // the branch's POSITION, not merely on its existence.
    //
    // One seat deliberately: an acting verdict DEMOTES its own target into the
    // chain filter's learned tail, so a two-seat fixture reorders the chain and
    // the sibling seat -- not the canary's target -- is reached first. The
    // no-fallback half is asserted on the walk's own hop count instead.
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeRepaired);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "the canary's own rejection must never reach the client: {:?}",
        dispatched.result.err(),
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "the repair and re-dispatch happened on the canary's own target",
    );
    assert_eq!(
        dispatched.meta.fallback_count, 0,
        "and the walk never hopped: the retry resolved the request in place",
    );
}

// ---------------------------------------------------------------------------
// Everything else is inconclusive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unrelated_four_xx_on_a_canary_earns_no_exemption() {
    // An unrelated caller error is terminal, exactly as it is for any other
    // request: the canary buys the field under test a repaired retry and
    // nothing else.
    let (router, seen) = single_seat(Answer::UnrelatedBadRequest);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "an unrelated 400 stays terminal under a canary",
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true],
        "one attempt only: no repaired retry was owed",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Inconclusive));
    assert!(!snap.canary_claimed, "the claim is released");
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "and the verdict is untouched: nothing was proved either way",
    );
    assert_eq!(
        router
            .field_verdicts()
            .canaries()
            .disproved_requests_total(),
        0,
        "an inconclusive outcome charges the alarm nothing",
    );
}

#[tokio::test]
async fn an_availability_failure_settles_the_canary_inconclusive() {
    let (router, _seen) = single_seat(Answer::Unavailable);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(snap.last_outcome, Some(CanaryOutcome::Inconclusive));
    assert!(!snap.canary_claimed);
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "an upstream that could not answer says nothing about the envelope",
    );
}

#[tokio::test]
async fn a_failed_repaired_retry_settles_the_canary_inconclusive() {
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::RejectRepairedRetry);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "premise: the retry did run, and then failed for its own reason",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(
        snap.last_outcome,
        Some(CanaryOutcome::Inconclusive),
        "a repaired retry that failed confirms nothing",
    );
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "and it disproves nothing either",
    );
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert!(dispatched.meta.cleared_capabilities.is_empty());
}

// ---------------------------------------------------------------------------
// The claim is released on every exit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_request_releases_the_canary_claim() {
    // RAII, observed through a genuinely dropped future: the walk is abandoned
    // mid-flight, so no settlement arm runs at all. A claim stranded here
    // would block every later canary for the identity, permanently.
    let (router, seen) = single_seat(Answer::Hang);
    plant_eligible_with_canary_due(&router, "m0");

    let outcome = tokio::time::timeout(
        Duration::from_millis(100),
        router.complete_with_options(req_on(ALIAS), RouterOptions::new()),
    )
    .await;

    assert!(
        outcome.is_err(),
        "premise: the future was dropped in flight"
    );
    assert_eq!(
        seen.attempts_carrying_the_field(),
        1,
        "premise: the abandoned attempt was the canary",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert!(
        !snap.canary_claimed,
        "a dropped future must release the claim, or the identity is never re-verified again",
    );
    assert_eq!(
        router
            .field_verdicts()
            .canaries()
            .disproved_requests_total(),
        0,
        "and an abandoned canary proves nothing",
    );
}

#[tokio::test]
async fn a_released_claim_is_reclaimable_by_a_later_request() {
    // Mutation check for the release: the second canary can only be claimed if
    // the first one's slot actually came free. Delete the release and this
    // request dispatches the repaired body instead of the field.
    let (router, seen) = single_seat(Answer::Unavailable);
    plant_eligible_with_canary_due(&router, "m0");

    assert!(
        router
            .complete_with_options(req_on(ALIAS), RouterOptions::new())
            .await
            .result
            .is_err(),
        "premise: the first canary settled inconclusive",
    );
    // Force the cadence due again, the way a bounded reschedule eventually
    // would, without re-seeding the state a settlement just wrote.
    for _ in 0..(CANARY_INTERVAL - 1) {
        let _ = router
            .complete_with_options(req_on(ALIAS), RouterOptions::new())
            .await;
    }

    assert_eq!(
        seen.attempts_carrying_the_field(),
        2,
        "the identity was re-verified a second time, so the first claim was released",
    );
}

#[tokio::test]
async fn a_canary_claimed_across_a_reload_stays_visible_to_the_replacement() {
    // A reload must not admit a second concurrent canary for an identity whose
    // first one is still in flight: the state moves by shared handle, never by
    // copy.
    let (router, _seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");
    let key = verdict_key("m0");
    let incarnation = canary_snapshot(&router, "m0").expect("seeded").incarnation;
    let claim = router
        .field_verdicts()
        .canaries()
        .claim_canary(&key, incarnation)
        .expect("the slot starts free");

    let replacement = router
        .field_verdicts()
        .rebuilt_on(Arc::clone(&router.learned_capabilities));

    assert!(
        replacement
            .canaries()
            .claim_canary(&key, incarnation)
            .is_none(),
        "the replacement facade must see the in-flight claim",
    );
    drop(claim);
    assert!(
        replacement
            .canaries()
            .claim_canary(&key, incarnation)
            .is_some(),
        "and see its release",
    );
}

#[tokio::test]
async fn a_canary_abandoned_while_the_verdict_moves_disproves_nothing() {
    // A canary in flight while the learned row's incarnation moves on: the
    // walk is then abandoned, so no settlement names an outcome. The claim must
    // come free and the alarm must be charged nothing -- a disproof recorded
    // here would attribute requests to a verdict lifecycle the canary was never
    // authorized against.
    //
    // Scope, stated because it is easy to overread: this exercises the DISPATCH
    // path's release. The canary registry's own incarnation-staleness guard is a
    // different axis (its own resident incarnation, reseeded rather than bumped)
    // and is pinned by `a_stale_regressed_settlement_charges_the_alarm_nothing`.
    let (router, seen) = single_seat(Answer::Hang);
    plant_eligible_with_canary_due(&router, "m0");

    let flight = tokio::time::timeout(
        Duration::from_millis(100),
        router.complete_with_options(req_on(ALIAS), RouterOptions::new()),
    );
    let bumped = {
        let learned = Arc::clone(&router.learned_capabilities);
        let grounded = grounded_key();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            learned.bump_incarnation_for_tests("m0", &grounded, ANTHROPIC);
        })
    };
    let outcome = flight.await;
    bumped.await.expect("the bump task completes");

    assert!(outcome.is_err(), "premise: the walk was abandoned");
    assert_eq!(
        seen.attempts_carrying_the_field(),
        1,
        "premise: a canary was in flight when the incarnation moved",
    );
    assert!(
        !canary_snapshot(&router, "m0")
            .expect("resident")
            .canary_claimed,
        "the claim is released regardless of what moved under it",
    );
    assert_eq!(
        router
            .field_verdicts()
            .canaries()
            .disproved_requests_total(),
        0,
        "and an abandoned canary disproves nothing",
    );
}

// ---------------------------------------------------------------------------
// Exactly one canary under hostile concurrency
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_of_many_concurrent_eligible_requests_claims_the_canary() {
    // The cadence trip and the claim are two separate atomic operations, so
    // concurrent requests could otherwise both observe the trip, or both
    // observe a free slot. The property is a COUNT over the dispatched bytes:
    // exactly one attempt carried the tested field, whatever the interleaving.
    //
    // The seat answers with an unrelated, TERMINAL 400, and both properties are
    // load-bearing. Not serving: an unrepaired canary that SUCCEEDS disproves
    // the verdict and clears it, after which every later request forwards the
    // field for a completely different reason -- so a carried-attempt count on a
    // serving fixture measures the clear, not the claim. Terminal: a retryable
    // class (a 503) has the canary request re-dispatch its own unchanged body,
    // which is correct behavior but makes the count two attempts for one canary.
    // A non-retryable class gives exactly one attempt per request, so the
    // attempt count and the request count coincide and the assertion below says
    // what it appears to say. Both are checked rather than assumed, by the
    // `calls()` assertion.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .expect("a multi-thread runtime with timers");

    let (router, seen) = single_seat(Answer::UnrelatedBadRequest);
    plant_eligible_with_canary_due(&router, "m0");
    let router = Arc::new(router);

    runtime.block_on(async {
        let mut tasks = Vec::with_capacity(64);
        for _ in 0..64 {
            let router = Arc::clone(&router);
            tasks.push(tokio::spawn(async move {
                let _ = router
                    .complete_with_options(req_on(ALIAS), RouterOptions::new())
                    .await;
            }));
        }
        for task in tasks {
            task.await.expect("no task panics");
        }
    });

    assert_eq!(
        seen.calls(),
        64,
        "premise: one attempt per request, so the count below is over requests",
    );
    assert_eq!(
        seen.attempts_carrying_the_field(),
        1,
        "exactly one of 64 concurrent eligible requests carried the field unrepaired: {:?}",
        seen.carried_per_attempt(),
    );
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&verdict_key("m0"), Instant::now()),
        "premise: the verdict survived, so every other request was repaired because it \
         was eligible rather than because the verdict had been cleared",
    );
    assert_eq!(
        router
            .field_verdicts()
            .canaries()
            .disproved_requests_total(),
        0,
        "and no request disproved anything",
    );
}

/// The dispatch-level consequence of the monotonic rule: a request whose
/// identity is carried forward by a concurrent confirmation AFTER its
/// eligibility read must not strand into a terminal rejection.
///
/// The straggler's accounting is refused, so its planner fails open and forwards
/// the field unchanged. Without the monotonic refusal it would instead reseed the
/// identity -- erasing the live lifecycle's tally and claim -- and this fixture's
/// seat rejects the carried field, so the request would die on a 400 the verdict
/// exists to avoid.
///
/// Deterministic: the carry is performed inside the eligibility interposition,
/// the same seam the existing moved-under-the-read tests use, rather than by
/// racing threads.
#[tokio::test]
async fn a_straggler_forwards_unchanged_instead_of_stranding_on_a_rejection() {
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible(&router, "m0");

    // Carry the identity's CANARY state forward in the eligibility window, so
    // the request leaves the read authorized against a now-superseded
    // incarnation -- exactly a straggler behind a concurrent confirmation.
    let canaries = Arc::clone(router.field_verdicts().canaries());
    let vkey = verdict_key("m0");
    let fired = Arc::new(AtomicUsize::new(0));
    let fired_in_hook = Arc::clone(&fired);
    let _interposed = crate::field_verdict::eligibility_interpose::install(move || {
        // Once only: the planner reads eligibility per attempt, and carrying on
        // every pass would move the target the assertions name.
        if fired_in_hook.fetch_add(1, Ordering::SeqCst) == 0 {
            let current = canaries.snapshot(&vkey).expect("resident").incarnation;
            canaries.acknowledge_confirmation(&vkey, current + 1, 1);
        }
    });

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        fired.load(Ordering::SeqCst) >= 1,
        "premise: the interposition ran, so the straggler window was reached",
    );
    assert!(
        dispatched.result.is_ok(),
        "the straggler forwards unchanged and succeeds rather than stranding on a \
         terminal rejection",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert!(
        !snap.canary_claimed,
        "and it claimed no canary against the lifecycle it no longer names",
    );
    assert_eq!(
        snap.modified_since_confirmation, 0,
        "nor charged exposure to the carried lifecycle it never repaired for",
    );
    assert!(
        !seen.carried_per_attempt().is_empty(),
        "premise: the request actually reached the seat",
    );
}
