//! Behavioral coverage of the reactive L0 field repair through all three
//! dispatch walks: the drop, the shared per-request ceiling, the two-phase
//! settlement, the persistence rows, and every path that must learn nothing.
//!
//! # Why the fixture injects a resolution
//!
//! The envelope-to-path parser is out of this stage, so nothing on real
//! traffic resolves a field path (`field_repair`'s module docs, and the
//! production-inertness control beside the resolver). A behavioral test
//! therefore injects a PROVISIONAL resolution the way the capability tests
//! plant learned negatives: the arm under test is everything DOWNSTREAM of
//! the resolution, and that is what a grounded parser will later switch on.
//!
//! # Why count_tokens is covered here and not by source guards
//!
//! The replay repair could not reach the token-count walk -- its capable
//! seats and the replay classifier's fixture-backed kinds are disjoint -- so
//! that walk's share of the ceiling was pinned by guards over its source.
//! The field repair closes that gap: it acts on the `anthropic-api` lane,
//! which is exactly the lane `seat_can_count_tokens` admits unconditionally,
//! so a repair genuinely fires in the token-count walk and every wiring
//! claim those guards made is now measurable. The N-seat ceiling test below
//! is what makes deleting the arm, or giving each seat its own budget, go
//! RED.

use super::super::PurgeOutcome;
use super::super::Router;
use super::super::repair_budget::REPAIRS_PER_REQUEST;
use super::{ANTHROPIC_API_KIND, provisional};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::failure_class::FailureClass;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error, Message,
    MessageContent, Provider, ReasoningConfig, Result, Role, TokenCount,
};
use serde_json::json;

use crate::config::{AliasValue, Config};
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

/// The qualified dotted path the captured rejection envelope named. The one
/// grounded row in the closed repair table, so it is what a repair can act
/// on; the fixture injects it as the provisional resolution.
const REJECTED_PATH: &str = "thinking.enabled.display";

/// The field capability key the grounded path mints.
///
/// MINTED through the namespace owner rather than spelled out: the prefix is
/// permanent and has exactly one compiled spelling, which a lexer-backed scan
/// enforces across every source file -- a literal here would be the second
/// spelling that scan exists to forbid. The PATH half is still spelled
/// literally in `REJECTED_PATH`, so what these tests pin is that the
/// qualified path survives into the key byte for byte; the prefix's own bytes
/// are pinned independently by the namespace owner's tests.
fn rejected_key() -> String {
    crate::field_capability::field_capability_key(REJECTED_PATH)
        .expect("the grounded path is a well-formed qualified path")
}

/// A rejection body shaped like the captured envelope. Carries no secret.
/// It is NOT what makes the repair fire -- production attributes no path to
/// it (asserted by `field_repair`'s own control) -- it is here so the
/// fixture's error is realistic rather than blank.
const FIELD_REJECT_BODY: &str = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;

/// Chain length, strictly greater than the ceiling so a per-target budget
/// and the shared per-request budget give DIFFERENT repair counts on one
/// fixture: per-target would permit one repair per leg (four), the shared
/// ceiling permits `REPAIRS_PER_REQUEST` (two).
const CHAIN_TARGETS: usize = 4;

/// The alias every fixture dispatches through. Deliberately NOT a model
/// name: an alias whose own name is also a chain member re-resolves through
/// itself and the walk dies on recursion depth before any repair can fire.
const ALIAS: &str = "field-repair-alias";

/// What one mock seat observed. `dispatched` keeps each attempt's request so
/// an assertion reads the BYTES that went upstream rather than inferring the
/// repair from a count.
#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    repairs: AtomicUsize,
    dispatched: Mutex<Vec<ChatRequest>>,
}

impl Observed {
    /// Record one dispatch, returning whether the attempt carried the field.
    fn record(&self, req: &ChatRequest) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let carried = carries_field(req);
        if !carried {
            self.repairs.fetch_add(1, Ordering::SeqCst);
        }
        self.dispatched.lock().push(req.clone());
        carried
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn repairs(&self) -> usize {
        self.repairs.load(Ordering::SeqCst)
    }

    /// Per attempt, in order: whether it still carried the rejected field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.dispatched.lock().iter().map(carries_field).collect()
    }

    /// The request of the `idx`-th attempt.
    fn attempt(&self, idx: usize) -> ChatRequest {
        self.dispatched.lock()[idx].clone()
    }
}

/// Whether a request still emits the rejected wire field through EITHER
/// canonical carrier. Both are checked because clearing only one leaves the
/// egress emitting the field from the other -- the drop's whole contract.
fn carries_field(req: &ChatRequest) -> bool {
    req.routectl_internal.anthropic_thinking_display.is_some()
        || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
}

/// How a mock answers an attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Reject every attempt with the field rejection, repaired or not: the
    /// repair count is then bounded by the budget and by nothing else.
    AlwaysRejectField,
    /// Reject the carried variant and SERVE the repaired one -- the shape
    /// that proves the repaired body is the one dispatched, and the only one
    /// that can reach a commit.
    ServeRepaired,
    /// Reject with a status no field rejection can carry, so the repair arm
    /// must decline.
    UnrelatedError,
    /// Reject with a 429. Paired with a `class_overrides` entry remapping 429
    /// onto the bad-request class, this is the control for "an operator's
    /// ROUTING preference must not become a repair trigger".
    RateLimited,
    /// Reject with a 503, the other remap control.
    Unavailable,
    /// Serve the FIRST attempt outright: the field is accepted, which is the
    /// clear path.
    ServeImmediately,
    /// Reject the carried variant with the field rejection, then answer the
    /// REPAIRED attempt with a capability error.
    ///
    /// This is the only shape that makes the token-count walk visit several
    /// seats while each one attempts a repair: that walk advances on a
    /// CAPABILITY error alone, so a seat whose repaired attempt still 400s
    /// settles the whole walk at seat one and an N-seat ceiling assertion
    /// would be vacuous. Each seat here spends one repair and then hands the
    /// walk on, which is exactly the shape the shared ceiling has to bound.
    RejectThenWalk,
}

struct MockSeat {
    answer: Answer,
    observed: Arc<Observed>,
}

impl MockSeat {
    const fn new(answer: Answer, observed: Arc<Observed>) -> Self {
        Self { answer, observed }
    }

    /// Record the attempt and produce the mock's answer for it.
    fn answer_for(&self, req: &ChatRequest) -> Result<()> {
        let carried = self.observed.record(req);
        match self.answer {
            Answer::AlwaysRejectField => Err(Error::upstream("field-mock", 400, FIELD_REJECT_BODY)),
            Answer::RejectThenWalk if carried => {
                Err(Error::upstream("field-mock", 400, FIELD_REJECT_BODY))
            }
            Answer::RejectThenWalk => Err(Error::upstream("field-mock", 501, "cannot count")),
            Answer::ServeRepaired if carried => {
                Err(Error::upstream("field-mock", 400, FIELD_REJECT_BODY))
            }
            Answer::ServeRepaired | Answer::ServeImmediately => Ok(()),
            Answer::UnrelatedError => Err(Error::upstream(
                "field-mock",
                503,
                r#"{"error":{"type":"api_error","message":"upstream unavailable"}}"#,
            )),
            Answer::RateLimited => Err(Error::upstream(
                "field-mock",
                429,
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            )),
            Answer::Unavailable => Err(Error::upstream(
                "field-mock",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
        }
    }
}

#[async_trait::async_trait]
impl Provider for MockSeat {
    fn id(&self) -> &'static str {
        "field-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("field-mock", "unused"))
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
        // The answer passes through VERBATIM: the repair arm reads THIS
        // error, so a mock that rewrote a field rejection into something
        // else would hide the very rejection under test. Whether the walk
        // advances past this seat is the `Answer`'s business (see
        // `RejectThenWalk`), not this method's.
        self.answer_for(&req).map(|()| TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// A CONTENT-BEARING chunk. The streaming walk commits on first content, not
/// on stream-open, so a content-free chunk reads as "closed before any
/// content" and the fixture would report a repair failure that is really a
/// mock defect.
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

/// A request carrying the rejected field through BOTH canonical carriers,
/// plus an active thinking request so the drop can be shown to remove the
/// field WITHOUT disabling the feature.
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

/// Config text for an N-leg `anthropic-api` chain -- the lane this stage
/// acts on AND the lane the token-count walk admits -- each leg on its own
/// provider entry so per-seat gates, breakers and verdict keys are
/// independent. `base_url` is left at the default Anthropic base: the
/// suppression predicate keys on it, and a loopback base refuses every mint
/// (its own test below, not the default).
fn chain_config(alias: &str, seats: usize) -> Config {
    let mut toml_text = String::new();
    let mut chain: Vec<String> = Vec::with_capacity(seats);
    for idx in 0..seats {
        toml_text.push_str(&format!(
            "\n[providers.p{idx}]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n"
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
/// Returns the router plus each seat's observation record and its mock, in
/// chain order.
fn install(
    config: Config,
    seats: usize,
    answer: Answer,
) -> (Router, Vec<Arc<Observed>>, Vec<Arc<MockSeat>>) {
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut observed: Vec<Arc<Observed>> = Vec::with_capacity(seats);
    let mut mocks: Vec<Arc<MockSeat>> = Vec::with_capacity(seats);
    for idx in 0..seats {
        let seen = Arc::new(Observed::default());
        observed.push(seen.clone());
        let mock = Arc::new(MockSeat::new(answer, seen));
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
    (router, observed, mocks)
}

fn chain_of(alias: &str, seats: usize, answer: Answer) -> (Router, Vec<Arc<Observed>>) {
    let (router, observed, _mocks) = install(chain_config(alias, seats), seats, answer);
    (router, observed)
}

/// A one-seat chain, for the settlement and no-learn cases where the
/// interesting property is what ONE target's outcome persists.
fn single_seat(alias: &str, answer: Answer) -> (Router, Arc<Observed>) {
    let (router, mut observed) = chain_of(alias, 1, answer);
    (router, observed.remove(0))
}

/// Plant a resident but LAPSED field verdict for `state_key`, through the
/// registry's own carry-over import seam -- never by parsing a rejection
/// envelope, and never by reaching into the verdict lifecycle.
///
/// Lapsed rather than acting on purpose: an ACTING verdict refuses the repair
/// slot (that is the single-flight contract), so the state in which a request
/// re-verifies a field, and can therefore clear it, is exactly the lapsed one.
/// `expires_at` is stamped in the past so the entry is lapsed the instant the
/// dispatch reads it, with no dependence on wall-clock or on the configured
/// decay window.
fn plant_lapsed_verdict(router: &Router, state_key: &str) {
    // Stamped at NOW, which is already lapsed by the time the dispatch reads
    // it: the expiry test is `now >= expires_at`, so an entry whose expiry is
    // the planting instant is lapsed for every later instant. No clock
    // arithmetic, so nothing here depends on how long the process has been up
    // or on the configured decay window.
    let stamped = std::time::Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: rejected_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped,
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

/// Whether a verdict for `state_key` is resident in the shared registry, read
/// through the registry's own snapshot rather than through the lifecycle -- so
/// "mutated resident state" is a fact about the STORE, not about one key's
/// acting status.
fn verdict_resident(router: &Router, state_key: &str) -> bool {
    router
        .learned_capability_snapshot()
        .into_iter()
        .any(|entry| entry.state_key == state_key && entry.feature_key == rejected_key())
}

/// A one-seat `anthropic-api` chain whose provider entry authenticates with a
/// FORWARDED client credential, plus a request carrying a client bearer (a
/// forwarded target with no bearer fails terminally before dispatch).
fn forwarded_seat(answer: Answer) -> (Router, Arc<Observed>, ChatRequest) {
    let mut config = chain_config(ALIAS, 1);
    let entry = config
        .providers
        .remove("p0")
        .expect("the fixture entry exists")
        .with_credential_source(crate::config::CredentialSource::Forwarded);
    config.providers.insert("p0".to_string(), entry);
    let (router, mut observed, _mocks) = install(config, 1, answer);
    let mut req = req_on(ALIAS);
    req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        "sk-ant-oat01-FWD".into(),
    ));
    (router, observed.remove(0), req)
}

/// A one-seat chain whose provider REMAPS `status` onto the caller-shaped
/// bad-request class -- the operator override that must not become a repair
/// trigger. Built through the real config deserialize path so the
/// `[providers.p0.class_overrides]` adapter is genuinely exercised.
fn remapping_seat(status: u16, answer: Answer) -> (Router, Arc<Observed>) {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\n\
         [providers.p0.class_overrides]\n{status} = \"bad-request\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let (router, mut observed, _mocks) = install(config, 1, answer);
    (router, observed.remove(0))
}

/// A one-seat `anthropic-api` chain on a LOOPBACK base URL -- a local hop,
/// which this stage must not attribute a rejection to at all.
fn loopback_seat(answer: Answer) -> (Router, Arc<Observed>) {
    let mut config: Config = toml::from_str(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"http://127.0.0.1:8889\"\n",
    )
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let (router, mut observed, _mocks) = install(config, 1, answer);
    (router, observed.remove(0))
}

/// A one-seat `anthropic-api` chain on the BEDROCK MANTLE sub-lane.
///
/// `base_url` is deliberately left unset: mantle validation REQUIRES the
/// default, and the factory derives the real endpoint from the region. That is
/// the whole hazard -- the configured base reads as a remote Anthropic host
/// while the effective egress is Bedrock.
#[cfg(feature = "bedrock")]
fn mantle_seat(answer: Answer) -> (Router, Arc<Observed>) {
    let toml_text = r#"
[providers.p0]
kind = "anthropic-api"
api_key_ref = ""

[providers.p0.bedrock_mantle]
region = "us-west-2"

[providers.p0.bedrock_mantle.creds]
kind = "bearer-key"
key_ref = "env://AWS_BEARER_TOKEN_BEDROCK"
"#;
    let mut config: Config = toml::from_str(toml_text).expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let (router, mut observed, _mocks) = install(config, 1, answer);
    (router, observed.remove(0))
}

fn repairs_observed(observed: &[Arc<Observed>]) -> usize {
    observed.iter().map(|o| o.repairs()).sum()
}

fn calls_observed(observed: &[Arc<Observed>]) -> usize {
    observed.iter().map(|o| o.calls()).sum()
}

/// The fixture's own premise: every leg was actually dispatched, so a
/// ceiling assertion cannot pass vacuously on a walk that stopped early.
fn assert_every_seat_dispatched(observed: &[Arc<Observed>], surface: &str) {
    assert!(
        CHAIN_TARGETS > usize::from(REPAIRS_PER_REQUEST),
        "the fixture must offer more repair-eligible seats than the ceiling permits, or a \
         per-seat budget would produce the same count as the shared one",
    );
    for (idx, seen) in observed.iter().enumerate() {
        assert!(
            seen.calls() > 0,
            "{surface}: seat {idx} was never dispatched, so the repair ceiling is not what \
             bounded this walk",
        );
    }
}

// ---------------------------------------------------------------------------
// The repair fires, in all three walks, and the repaired body is dispatched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_drops_the_field_and_serves_the_repaired_request() {
    // Arrange
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    // Act
    let served = router.complete(req_on(ALIAS)).await;

    // Assert -- the repaired attempt is what served, in exactly two touches.
    assert!(
        served.is_ok(),
        "the repaired request must serve: {served:?}"
    );
    assert_eq!(seen.calls(), 2, "one carried attempt plus one repair");
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "the first attempt carried the field and the retry did not: the REPAIRED body is the \
         one dispatched upstream",
    );
}

#[tokio::test]
async fn stream_pre_content_drops_the_field_and_serves_the_repaired_request() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let served = router.stream(req_on(ALIAS)).await;

    assert!(
        served.is_ok(),
        "the repaired stream must open: {:?}\ncarried={:?}",
        served.as_ref().err(),
        seen.carried_per_attempt()
    );
    assert_eq!(seen.calls(), 2, "one carried attempt plus one repair");
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "the repaired body is the one dispatched on the streaming walk too",
    );
}

#[tokio::test]
async fn count_tokens_drops_the_field_and_counts_the_repaired_request() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let counted = router.count_tokens(req_on(ALIAS)).await;

    assert!(
        counted.is_ok(),
        "the repaired count must serve: {counted:?}"
    );
    assert_eq!(seen.calls(), 2, "one carried attempt plus one repair");
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "the repaired body is the one whose COUNT is returned -- a count of the unrepaired \
         body would be a number for a request that was never sent",
    );
}

#[tokio::test]
async fn the_repair_drops_the_rejected_field_only() {
    // The drop's SCOPE, read off the dispatched bytes rather than asserted
    // about the surface: the retried request lost both carriers of the
    // rejected field and KEPT the thinking request itself plus the prompt. A
    // drop that took the whole reasoning block, or rebuilt the messages,
    // would look identical at every call count.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let served = router.complete(req_on(ALIAS)).await;
    assert!(served.is_ok(), "premise: the repair served");

    let carried = seen.attempt(0);
    let repaired = seen.attempt(1);
    assert!(
        carries_field(&carried),
        "premise: the first attempt carried the field, so the comparison below is not vacuous",
    );
    assert!(
        repaired
            .routectl_internal
            .anthropic_thinking_display
            .is_none(),
        "the verbatim carrier is gone from the dispatched body",
    );
    assert_eq!(
        repaired.reasoning.as_ref().and_then(|r| r.exclude),
        None,
        "and so is the canonical boolean the egress would otherwise derive it from",
    );
    assert_eq!(
        repaired.reasoning.as_ref().and_then(|r| r.enabled),
        Some(true),
        "whether thinking was REQUESTED is a different field and must survive, or the repair \
         silently disables the feature it was repairing",
    );
    assert_eq!(
        repaired.reasoning.as_ref().and_then(|r| r.max_tokens),
        Some(2048),
        "the thinking budget survives too",
    );
    assert_eq!(
        serde_json::to_string(&repaired.messages).expect("serializable"),
        serde_json::to_string(&carried.messages).expect("serializable"),
        "the messages are untouched byte for byte: a rebuilt prefix would cost upstream \
         prompt-cache affinity on every repair",
    );
}

// ---------------------------------------------------------------------------
// The shared per-request ceiling, in all three walks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_spends_at_most_the_request_ceiling_across_an_n_target_chain() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, observed) = chain_of(ALIAS, CHAIN_TARGETS, Answer::AlwaysRejectField);

    let result = router.complete(req_on(ALIAS)).await;

    assert!(result.is_err(), "no target serves the repaired variant");
    assert_every_seat_dispatched(&observed, "complete");
    assert_eq!(
        repairs_observed(&observed),
        usize::from(REPAIRS_PER_REQUEST),
        "complete must spend the shared per-request ceiling, not one repair per target",
    );
    assert_eq!(
        calls_observed(&observed),
        CHAIN_TARGETS + usize::from(REPAIRS_PER_REQUEST),
        "one carried attempt per target plus exactly the shared repair allowance",
    );
}

#[tokio::test]
async fn stream_pre_content_spends_at_most_the_request_ceiling_across_an_n_target_chain() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, observed) = chain_of(ALIAS, CHAIN_TARGETS, Answer::AlwaysRejectField);

    let result = router.stream(req_on(ALIAS)).await;

    assert!(result.is_err(), "no target serves the repaired variant");
    assert_every_seat_dispatched(&observed, "stream");
    assert_eq!(
        repairs_observed(&observed),
        usize::from(REPAIRS_PER_REQUEST),
        "stream pre-content must spend the shared per-request ceiling",
    );
}

#[tokio::test]
async fn count_tokens_spends_at_most_the_request_ceiling_across_an_n_seat_walk() {
    // THE test the temporary source guards existed in place of. A repair now
    // genuinely fires in this walk, so deleting its arm reds the repair count
    // and giving each seat its own budget reds the ceiling.
    //
    // `RejectThenWalk`: each seat rejects the carried variant and answers the
    // repaired one with a capability error, which is the ONLY error class this
    // walk advances on. That is what lets several seats each attempt a repair.
    //
    // The two budget shapes are distinguishable here, and the assertions below
    // name both numbers rather than only the repair count:
    //
    // - SHARED (correct): seats 1 and 2 repair and hand the walk on; seat 3's
    //   carried rejection finds the budget spent, so its 400 is a terminal
    //   caller-shaped error and the walk stops there. Three seats visited, two
    //   repairs.
    // - PER-SEAT (the defect): every seat repairs and hands the walk on, so
    //   all four are visited and four repairs are paid for one logical count.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, observed) = chain_of(ALIAS, CHAIN_TARGETS, Answer::RejectThenWalk);

    let result = router.count_tokens(req_on(ALIAS)).await;

    assert!(result.is_err(), "no seat serves the count");
    // Premise: the walk really did reach past the ceiling's worth of seats, so
    // the repair count below is bounded by the BUDGET and not by the walk
    // stopping early for some unrelated reason.
    let visited = observed.iter().filter(|seen| seen.calls() > 0).count();
    assert!(
        visited > usize::from(REPAIRS_PER_REQUEST),
        "the walk must visit more seats than the ceiling permits repairs, or the ceiling is \
         not what bounded it; visited {visited}",
    );
    assert_eq!(
        repairs_observed(&observed),
        usize::from(REPAIRS_PER_REQUEST),
        "the token-count walk must draw the SHARED per-request ceiling: a fresh budget per \
         seat would let this {CHAIN_TARGETS}-seat walk pay one repair per seat for one \
         logical count, and deleting the arm would pay none",
    );
    assert_eq!(
        observed[CHAIN_TARGETS - 1].calls(),
        0,
        "the last seat is never reached: once the shared allowance is spent, an unrepaired \
         field rejection is a terminal caller-shaped error. A per-seat budget would repair on \
         every seat and walk all the way here",
    );
}

#[tokio::test]
async fn all_three_walks_agree_on_one_ceiling() {
    // A fresh chain per walk: the ceiling is per REQUEST, so each walk gets
    // the full allowance and the three counts must come out identical. A
    // walk that kept a per-seat budget, or lost its repair position, reads as
    // a different number here -- and one site asserting them together is
    // what keeps a later edit to one walk from silently desyncing the others.
    let _injection = provisional::inject(REJECTED_PATH);
    let (complete_router, complete_seen) =
        chain_of(ALIAS, CHAIN_TARGETS, Answer::AlwaysRejectField);
    let (stream_router, stream_seen) = chain_of(ALIAS, CHAIN_TARGETS, Answer::AlwaysRejectField);
    // count_tokens needs the walking answer to reach past its first seat (see
    // the N-seat test); the ceiling it draws is the same either way.
    let (count_router, count_seen) = chain_of(ALIAS, CHAIN_TARGETS, Answer::RejectThenWalk);

    let _ = complete_router.complete(req_on(ALIAS)).await;
    let _ = stream_router.stream(req_on(ALIAS)).await;
    let _ = count_router.count_tokens(req_on(ALIAS)).await;

    assert_eq!(
        [
            repairs_observed(&complete_seen),
            repairs_observed(&stream_seen),
            repairs_observed(&count_seen),
        ],
        [usize::from(REPAIRS_PER_REQUEST); 3],
        "complete, stream pre-content and count_tokens must share ONE per-request ceiling",
    );
}

// ---------------------------------------------------------------------------
// Settlement: what learns, what clears, and what must learn nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_repaired_retry_commits_one_learned_verdict() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, _seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "premise: the repair served");
    assert_eq!(
        dispatched.meta.learned_capabilities.len(),
        1,
        "a confirmed repair persists exactly one verdict",
    );
    let event = &dispatched.meta.learned_capabilities[0];
    assert_eq!(
        event.capability_key,
        rejected_key(),
        "the learned row is keyed on the qualified dotted path, byte for byte",
    );
    assert_eq!(event.state_key, "m0", "keyed on the repaired target");
    assert!(
        dispatched.meta.cleared_capabilities.is_empty(),
        "a repaired retry commits; it does not also clear",
    );
}

#[tokio::test]
async fn count_tokens_settlements_reach_the_capability_event_sink() {
    // The gap the token-count walk carried: its settlement had no ledger
    // sink, so it deliberately learned nothing. The walk now returns its
    // meta, so a committed verdict rides out as an event row the same way the
    // messages walks' rows do -- a registry mutation whose row went nowhere
    // is exactly the state a warm rebuild resurrects a verdict from.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;

    assert!(counted.result.is_ok(), "premise: the repaired count served");
    assert_eq!(seen.calls(), 2, "premise: the walk repaired");
    assert_eq!(
        counted.meta.learned_capabilities.len(),
        1,
        "the token-count walk's committed verdict must ride out as an event row",
    );
    assert_eq!(
        counted.meta.learned_capabilities[0].capability_key,
        rejected_key(),
        "under the same key the messages walks would emit",
    );
}

#[tokio::test]
async fn an_accepted_field_clears_a_resident_verdict_through_the_event_sink() {
    // The other half of the two-phase lifecycle. A resident verdict is
    // PLANTED through the registry's own carry-over import seam -- the way
    // every other capability test plants one -- rather than by minting it
    // through a first dispatch, because a verdict that is still ACTING
    // correctly refuses the repair slot: what a later request re-verifies is a
    // LAPSED verdict, so that is what the fixture has to present.
    //
    // The ROW is the property, not just the in-memory state: a clear that
    // mutated the registry without emitting an event row would leave the
    // ledger holding only the `broken` row, and a warm rebuild would
    // resurrect the verdict this request disproved.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeImmediately);
    plant_lapsed_verdict(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "the accepted request serves");
    assert_eq!(seen.calls(), 1, "the field is accepted, so nothing repairs");
    assert_eq!(
        dispatched.meta.cleared_capabilities.len(),
        1,
        "an accepted field clears the resident verdict and rides the clear out as an event \
         row, so a warm rebuild cannot resurrect it",
    );
    assert_eq!(
        dispatched.meta.cleared_capabilities[0].capability_key,
        rejected_key(),
        "the cleared row names the same key a commit would have written, or the rebuild would \
         remove a different entry than the one that was cleared",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "an accepted field learns nothing",
    );
}

#[tokio::test]
async fn count_tokens_clears_a_resident_verdict_through_the_event_sink() {
    // The same clear, on the walk whose settlement previously had no sink at
    // all. Without the returned meta this clear would mutate the shared
    // registry and drop its row, which is precisely the warm-rebuild
    // resurrection this pins against.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeImmediately);
    plant_lapsed_verdict(&router, "m0");

    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;

    assert!(counted.result.is_ok(), "the accepted count serves");
    assert_eq!(seen.calls(), 1, "nothing repairs");
    assert_eq!(
        counted.meta.cleared_capabilities.len(),
        1,
        "the token-count walk's clear must reach the capability-event sink",
    );
    assert_eq!(
        counted.meta.cleared_capabilities[0].capability_key,
        rejected_key(),
        "under the same key the messages walks emit",
    );
}

#[tokio::test]
async fn a_repeat_rejection_learns_nothing_and_strands_no_slot() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);

    let first = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        first.result.is_err(),
        "the repaired variant is rejected too"
    );
    assert_eq!(seen.calls(), 2, "one carried attempt plus one repair");
    assert!(
        first.meta.learned_capabilities.is_empty(),
        "an unconfirmed rejection must persist nothing: a repair that did not fix it is not \
         evidence the field is the problem",
    );

    // The slot must be free: a second request repairs again rather than being
    // refused by a slot the first never released.
    let second = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert_eq!(
        seen.calls(),
        4,
        "the second request repaired too, so the first settled its single-flight slot",
    );
    assert!(
        second.meta.learned_capabilities.is_empty(),
        "still nothing learned",
    );
}

#[tokio::test]
async fn an_unrelated_error_neither_repairs_nor_learns() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::UnrelatedError);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "the error propagates");
    assert_eq!(
        seen.repairs(),
        0,
        "a class that cannot name a field must not trigger a field drop",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "an unrelated fault learns nothing",
    );
    assert!(
        dispatched.meta.field_repair.is_none(),
        "and reports no repair",
    );
}

#[tokio::test]
async fn a_request_that_does_not_carry_the_field_performs_no_repair() {
    // The drop's precondition. A rejection naming a field the attempt never
    // sent is not repairable by dropping it, and a request that mutates
    // nothing must not spend budget, claim a slot, or learn.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);
    let mut bare = req_on(ALIAS);
    bare.routectl_internal.anthropic_thinking_display = None;
    bare.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(2048),
        exclude: None,
        enabled: Some(true),
    });
    assert!(
        !carries_field(&bare),
        "premise: the fixture request really does not carry the field",
    );

    let dispatched = router
        .complete_with_options(bare, RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    assert_eq!(seen.calls(), 1, "one attempt, no repair re-dispatch");
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "a no-op drop learns nothing",
    );
    assert!(
        dispatched.meta.field_repair.is_none(),
        "and claims no repair in the summary",
    );
}

#[tokio::test]
async fn an_unknown_field_path_repairs_nothing_and_learns_nothing() {
    // The closed table's refusal, end to end: a resolution outside the table
    // reaches the arm and the arm declines. The injected path is the LEAF
    // spelling of the grounded row on purpose -- a table matched on a suffix
    // or a leaf name rather than the whole qualified path would accept it.
    let _injection = provisional::inject("thinking.display");
    let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    assert_eq!(
        seen.calls(),
        1,
        "a path with no row in the closed table performs no repair",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "and mints nothing -- an ungrounded path must not create a permanent token",
    );
}

#[tokio::test]
async fn the_kill_switch_off_disables_the_repair_entirely() {
    let _injection = provisional::inject(REJECTED_PATH);
    let mut config = chain_config(ALIAS, 1);
    config.capability.enabled = false;
    let (router, observed, _mocks) = install(config, 1, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "with the repair disabled the rejection stands",
    );
    assert_eq!(
        observed[0].calls(),
        1,
        "the kill switch stops the repair before it re-dispatches",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "and before it can learn",
    );
}

#[tokio::test]
async fn a_loopback_target_repairs_nothing_and_mints_nothing() {
    // The suppression predicate this stage reuses, unchanged. A local hop's
    // rejection is not attributable to the wire format that hop was
    // configured with, so it must never mint -- and because minting is what
    // the guard admits, such a target performs no repair at all rather than
    // repairing unguarded.
    let _injection = provisional::inject(REJECTED_PATH);
    let mut config: Config = toml::from_str(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"http://127.0.0.1:8889\"\n",
    )
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let (router, observed, _mocks) = install(config, 1, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "a loopback target can never mint an envelope-field verdict",
    );
    assert_eq!(
        observed[0].calls(),
        1,
        "and performs no repair, since the repair is what the guard admits",
    );
    assert!(dispatched.result.is_err(), "so the rejection stands");
}

#[tokio::test]
async fn a_non_anthropic_lane_performs_no_field_repair() {
    // Stage 1 acts on one lane. A second lane's rejection envelope has not
    // been captured, and the Bedrock lane additionally rewrites a dotted
    // capability key -- so both are excluded at this arm rather than by
    // changing the shared normalizer other capability classes depend on.
    let _injection = provisional::inject(REJECTED_PATH);
    let mut config: Config = toml::from_str(
        "\n[providers.p0]\nkind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n",
    )
    .expect("valid test toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let (router, observed, _mocks) = install(config, 1, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert_eq!(
        observed[0].calls(),
        1,
        "a lane outside this stage performs no field repair",
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "and mints no field verdict",
    );
    assert!(dispatched.result.is_err());
}

// ---------------------------------------------------------------------------
// Result-only count_tokens is NON-SETTLING
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_result_only_count_tokens_cannot_mutate_resident_state() {
    // The result-only signature keeps no `DispatchMeta`, so a settlement's
    // event row would be produced and dropped -- a persisted verdict with no
    // ledger record, which is what a warm rebuild resurrects from. It must
    // therefore not settle AT ALL, and this asserts on the STORE rather than on
    // a returned row: with no meta there is no row to inspect, so the registry
    // snapshot is the only place the violation would show.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let counted = router.count_tokens(req_on(ALIAS)).await;

    assert!(counted.is_ok(), "the count still serves");
    assert!(
        !verdict_resident(&router, "m0"),
        "a result-only call must leave the shared learned registry untouched: a verdict \
         persisted here would have no ledger row, and a warm rebuild would never learn it was \
         cleared",
    );
    // It DOES still repair -- the count it returns must describe a body the
    // upstream would accept -- so the inertness above is about SETTLEMENT
    // rather than about the walk doing nothing. That distinction is the whole
    // point: a walk that simply skipped the repair would satisfy the registry
    // assertion while returning a count for a request that was rejected.
    assert_eq!(
        seen.repairs(),
        1,
        "the result-only walk still repairs for the answer it returns",
    );
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "and the repaired body is the one whose count came back",
    );
}

#[tokio::test]
async fn the_result_only_count_tokens_cannot_clear_a_resident_verdict() {
    // The other settlement direction, which is the dangerous one: a clear that
    // removed a resident verdict WITHOUT emitting a cleared row would leave the
    // ledger holding only the `broken` row, so the next warm rebuild would
    // resurrect a verdict this process had already dropped.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, _seen) = single_seat(ALIAS, Answer::ServeImmediately);
    plant_lapsed_verdict(&router, "m0");
    assert!(
        verdict_resident(&router, "m0"),
        "premise: a resident verdict exists to be wrongly cleared",
    );

    let counted = router.count_tokens(req_on(ALIAS)).await;

    assert!(counted.is_ok(), "the accepted count serves");
    assert!(
        verdict_resident(&router, "m0"),
        "a result-only call must NOT clear a resident verdict: the removal would be invisible \
         to the ledger and the next rebuild would resurrect it",
    );
}

#[tokio::test]
async fn the_with_meta_count_tokens_still_settles_both_directions() {
    // The positive control for the two tests above: the SAME fixtures through
    // the settling entry point do mutate resident state and do produce a
    // rebuild-visible row. Without this, making the walk inert everywhere would
    // satisfy them.
    let _injection = provisional::inject(REJECTED_PATH);

    // Commit direction.
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let counted = router.count_tokens_with_meta(req_on(ALIAS)).await;
    assert!(counted.result.is_ok(), "the repaired count serves");
    assert_eq!(seen.repairs(), 1, "the settling walk does repair");
    assert!(
        verdict_resident(&router, "m0"),
        "and persists the verdict in the shared registry",
    );
    assert_eq!(
        counted.meta.learned_capabilities.len(),
        1,
        "and hands the caller a rebuild-visible learned row",
    );

    // Clear direction.
    let (clearing, _clear_seen) = single_seat(ALIAS, Answer::ServeImmediately);
    plant_lapsed_verdict(&clearing, "m0");
    let cleared = clearing.count_tokens_with_meta(req_on(ALIAS)).await;
    assert!(cleared.result.is_ok(), "the accepted count serves");
    assert!(
        !verdict_resident(&clearing, "m0"),
        "the settling walk drops the resident verdict",
    );
    assert_eq!(
        cleared.meta.cleared_capabilities.len(),
        1,
        "and hands the caller a rebuild-visible cleared row, so the two always agree",
    );
}

// ---------------------------------------------------------------------------
// Forwarded-credential targets never repair and never mint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_forwarded_credential_target_performs_no_field_repair() {
    // A forwarded target authenticates with the CLIENT's own bearer: routectl
    // owns no credential there, and one client's rejection would mint a
    // permanent verdict that steers every other client through that entry.
    // Refused before the body is read, the budget is drawn, or a guard is
    // claimed -- so the request dispatches exactly once, unmodified.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen, req) = forwarded_seat(Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert_eq!(
        seen.calls(),
        1,
        "a forwarded target is dispatched once and never repaired",
    );
    assert!(
        carries_field(&seen.attempt(0)),
        "and its request is untouched: the field is still on the attempt that went upstream",
    );
    assert!(
        !verdict_resident(&router, "m0"),
        "and no verdict is minted for a credential routectl does not own",
    );
    assert!(
        dispatched.meta.field_repair.is_none(),
        "and the summary claims no repair",
    );
    assert!(dispatched.result.is_err(), "so the rejection stands");
}

#[tokio::test]
async fn an_own_credential_target_on_the_same_lane_still_repairs() {
    // The positive control that makes the refusal above about the FORWARDED
    // flag rather than about the fixture: the same lane, the same body, the
    // same injected resolution, with routectl's own credential -- and it
    // repairs.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let served = router.complete(req_on(ALIAS)).await;

    assert!(
        served.is_ok(),
        "an own-credential target repairs and serves"
    );
    assert_eq!(seen.repairs(), 1, "exactly one repair");
}

#[tokio::test]
async fn a_loopback_target_refuses_the_repair_on_every_walk_including_result_only() {
    // The suppression predicate applies to the PLAN, not just to the mint, and
    // this is what makes that true for the result-only walk as well. Checking
    // it only inside the guard's admission left a hole: a non-settling walk
    // never reaches that admission, so it would have mutated the body, spent
    // the shared allowance, and re-dispatched a target whose rejection this
    // stage has decided it cannot attribute at all.
    //
    // A suppressed target is one routectl must not ACT on, not merely one it
    // must not learn from -- so every walk dispatches it exactly once, with the
    // field still on the request.
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in [
        "complete",
        "stream",
        "count_tokens",
        "count_tokens_result_only",
    ] {
        let (router, seen) = loopback_seat(Answer::ServeRepaired);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            "count_tokens" => {
                let _ = router.count_tokens_with_meta(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.calls(),
            1,
            "{surface}: a loopback target must be dispatched once and never repaired",
        );
        assert!(
            carries_field(&seen.attempt(0)),
            "{surface}: and its body must be untouched -- the field is still on the attempt \
             that went upstream, so no mutation preceded the refusal",
        );
        assert!(
            !verdict_resident(&router, "m0"),
            "{surface}: and nothing may be minted for a local hop",
        );
    }
}

#[tokio::test]
async fn a_remote_target_on_the_same_lane_repairs_on_every_walk() {
    // The positive control for the refusal above, per walk: the SAME fixture on
    // a REMOTE base URL repairs everywhere. Without it, an arm that had simply
    // stopped repairing would satisfy every loopback assertion.
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in [
        "complete",
        "stream",
        "count_tokens",
        "count_tokens_result_only",
    ] {
        let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            "count_tokens" => {
                let _ = router.count_tokens_with_meta(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.repairs(),
            1,
            "{surface}: a remote target on the default Anthropic base DOES repair, so the \
             loopback refusals are about the base URL and not about a dead arm",
        );
    }
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn a_bedrock_mantle_target_performs_no_field_repair_on_any_walk() {
    // The sub-lane a bare variant match gets wrong. A mantle entry is
    // `kind = "anthropic-api"` and leaves `base_url` at the Anthropic default
    // (validation requires that), while the factory derives the real endpoint
    // from `bedrock_mantle.region` -- so the configured base reads as an
    // attributable remote Anthropic host when the effective egress is Bedrock.
    // Stage 1 excludes Bedrock, so this target must not repair or mint, and the
    // exclusion lives in the base-URL accessor rather than in Bedrock
    // capability-key normalization, which is untouched.
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in [
        "complete",
        "stream",
        "count_tokens",
        "count_tokens_result_only",
    ] {
        let (router, seen) = mantle_seat(Answer::ServeRepaired);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            "count_tokens" => {
                let _ = router.count_tokens_with_meta(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.calls(),
            1,
            "{surface}: a mantle target must be dispatched once and never repaired",
        );
        assert!(
            carries_field(&seen.attempt(0)),
            "{surface}: and its body must be untouched -- the field is still on the attempt \
             that went upstream",
        );
        assert!(
            !verdict_resident(&router, "m0"),
            "{surface}: and nothing may be minted for a Bedrock egress",
        );
    }
}

// ---------------------------------------------------------------------------
// A remapped class is not a repair trigger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_operator_remap_of_a_rate_limit_does_not_trigger_a_field_repair() {
    // `[class_overrides]` expresses how the operator wants a status ROUTED; it
    // is not a claim about what the upstream said. Reading the remapped class
    // would let this override drop a field over a rate limit and then mint a
    // permanent verdict from it.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = remapping_seat(429, Answer::RateLimited);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert_eq!(
        seen.calls(),
        1,
        "a remapped 429 must not repair: the upstream said rate limit, not field",
    );
    assert!(!verdict_resident(&router, "m0"), "and must mint nothing");
    assert!(dispatched.meta.field_repair.is_none(), "and claim nothing");
}

#[tokio::test]
async fn an_operator_remap_of_an_unavailable_upstream_does_not_trigger_a_field_repair() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = remapping_seat(503, Answer::Unavailable);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert_eq!(seen.calls(), 1, "a remapped 503 must not repair either");
    assert!(!verdict_resident(&router, "m0"), "and must mint nothing");
    assert!(dispatched.meta.field_repair.is_none());
}

#[tokio::test]
async fn an_operator_remap_does_not_trigger_a_field_repair_on_any_walk() {
    // Per-walk, because each arm reads its own class binding: a fix applied to
    // one walk leaves the others live, and a single-walk test would report the
    // class hazard as closed while two arms still had it.
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in ["complete", "stream", "count_tokens"] {
        let (router, seen) = remapping_seat(429, Answer::RateLimited);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens_with_meta(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.calls(),
            1,
            "{surface}: a remapped 429 must not repair -- the upstream said rate limit, and an \
             operator's ROUTING preference is not a claim about the envelope",
        );
        assert!(
            !verdict_resident(&router, "m0"),
            "{surface}: and must mint nothing",
        );
    }
}

#[tokio::test]
async fn a_native_bad_request_still_repairs_on_any_walk_under_the_same_overrides() {
    // The per-walk positive control for the test above: same override table,
    // but a NATIVE 400, and every walk repairs. Without this, an arm that read
    // no class at all would satisfy the negatives.
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in ["complete", "stream", "count_tokens"] {
        let (router, seen) = remapping_seat(429, Answer::ServeRepaired);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens_with_meta(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.repairs(),
            1,
            "{surface}: a native bad request repairs even with an unrelated remap configured",
        );
    }
}

#[tokio::test]
async fn a_genuine_native_bad_request_still_repairs_under_the_same_overrides() {
    // The control that makes the two remap tests non-vacuous: with the SAME
    // override table installed, a NATIVE 400 -- what the upstream actually
    // returned -- still repairs. Without this, reading no class at all would
    // satisfy them.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = remapping_seat(429, Answer::ServeRepaired);

    let served = router.complete(req_on(ALIAS)).await;

    assert!(
        served.is_ok(),
        "a native bad request repairs even with an unrelated remap configured: {served:?}",
    );
    assert_eq!(seen.repairs(), 1, "exactly one repair");
}

// ---------------------------------------------------------------------------
// A no-op apply consumes nothing
// ---------------------------------------------------------------------------

#[test]
fn an_apply_that_removes_nothing_charges_no_budget_and_reports_no_repair() {
    // The ordering property: the budget draw lives INSIDE the successful apply
    // branch, so a drop that removes nothing leaves the allowance intact. A
    // draw in the caller's condition chain would charge a request that then
    // fell through to the ordinary error path -- a charge nothing observable
    // can see, which is why it is pinned here on the budget directly.
    //
    // Driven through the real plan: a request that does NOT carry the field is
    // exactly the state where the drop is a no-op. The injection is in scope so
    // the rejection genuinely NAMES the admitted row -- without it `apply` would
    // refuse on candidate selection instead, and this test would pass for the
    // wrong reason.
    let _injection = provisional::inject(REJECTED_PATH);
    let router = {
        let (r, _seen) = single_seat(ALIAS, Answer::ServeRepaired);
        r
    };
    let mut bare = req_on(ALIAS);
    bare.routectl_internal.anthropic_thinking_display = None;
    bare.reasoning = None;
    assert!(
        !carries_field(&bare),
        "premise: the request carries no droppable field",
    );

    let carrying = req_on(ALIAS);
    let target = router
        .dispatch_chain_for_request(&carrying)
        .expect("the fixture chain resolves")
        .0
        .into_iter()
        .next()
        .expect("one target");
    let mut plan = router
        .plan_field_carry(
            &target,
            &carrying,
            super::FieldSettlementMode::Settling,
            std::time::Instant::now(),
        )
        .expect("a carried field admits a plan");

    let mut budget = crate::router::repair_budget::RepairBudget::per_request();
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let before = serde_json::to_string(&bare).expect("serializable");

    // Act -- apply against the request that carries nothing.
    let applied = plan.apply(
        &mut bare,
        &mut meta,
        &mut budget,
        &FailureClass::BadRequest,
        &Error::upstream("p", 400, FIELD_REJECT_BODY),
        ANTHROPIC_API_KIND,
    );

    // Assert -- no repair, no mutation, no charge.
    assert!(applied.is_none(), "a no-op drop must report no repair");
    assert!(
        plan.repaired_path().is_none(),
        "and the plan records no repaired row, so no settlement can commit one",
    );
    assert_eq!(
        serde_json::to_string(&bare).expect("serializable"),
        before,
        "and must leave the request byte-identical",
    );
    assert!(
        meta.calib_estimated_tokens.is_none(),
        "and must not re-stamp an estimate for a payload it did not change",
    );
    // The allowance is intact: it still funds the full ceiling.
    let spent = (0..=usize::from(REPAIRS_PER_REQUEST))
        .filter(|_| budget.draw())
        .count();
    assert_eq!(
        spent,
        usize::from(REPAIRS_PER_REQUEST),
        "the no-op apply must have charged nothing, so the whole ceiling remains",
    );
}

/// The fail-closed branch of `apply`, exercised in RELEASE only.
///
/// In debug the same input trips the `debug_assert!`, which is the correct
/// development behavior and is why this is release-gated rather than
/// unconditional. The property under test is the OTHER half: that release
/// behavior does not depend on that assertion being compiled. A build with the
/// assertion stripped must still refuse -- no reported repair, no mutation, no
/// charge -- because a silent absorption here would report a repair the wire
/// never saw, spend the shared allowance on it, and let the caller select the
/// commit settlement.
#[cfg(not(debug_assertions))]
#[test]
fn a_surface_that_removes_nothing_fails_closed_in_release() {
    // Arrange -- a plan whose surface CLAIMS presence and removes nothing. The
    // real surface cannot diverge (its presence check and drop read the same
    // carriers), so the divergence is manufactured to reach the branch at all.
    // That surface has no closed-table row (it reports itself present
    // unconditionally, so a row would make every test request carry it), which
    // is why the EFFECT half is driven directly rather than through the
    // rejection-to-candidate selection.
    let mut plan = super::FieldRepairPlan::divergent_for_tests();
    let mut req = req_on(ALIAS);
    let before = serde_json::to_string(&req).expect("serializable");
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let mut budget = crate::router::repair_budget::RepairBudget::per_request();

    // Act
    let applied = plan.apply_sole_candidate_for_tests(
        &mut req,
        &mut meta,
        &mut budget,
        &Error::upstream("p", 400, FIELD_REJECT_BODY),
    );

    // Assert -- refused, and nothing spent or changed on the way out.
    assert!(
        applied.is_none(),
        "a surface that removed nothing must report NO repair in release: reporting one would \
         mark the attempt repaired and select the commit settlement",
    );
    assert_eq!(
        serde_json::to_string(&req).expect("serializable"),
        before,
        "and must leave the request byte-identical",
    );
    assert!(
        meta.calib_estimated_tokens.is_none(),
        "and must not re-stamp an estimate for a payload it did not change",
    );
    let spent = (0..=usize::from(REPAIRS_PER_REQUEST))
        .filter(|_| budget.draw())
        .count();
    assert_eq!(
        spent,
        usize::from(REPAIRS_PER_REQUEST),
        "and must have charged nothing, so the whole shared ceiling remains for a repair that \
         can actually happen",
    );
}

// ---------------------------------------------------------------------------
// Breaker neutrality and the calibration re-stamp
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repairable_rejection_does_not_debit_the_breaker_in_any_walk() {
    let _injection = provisional::inject(REJECTED_PATH);
    for surface in ["complete", "stream", "count_tokens"] {
        let (router, seen) = single_seat(ALIAS, Answer::AlwaysRejectField);
        match surface {
            "complete" => {
                let _ = router.complete(req_on(ALIAS)).await;
            }
            "stream" => {
                let _ = router.stream(req_on(ALIAS)).await;
            }
            _ => {
                let _ = router.count_tokens(req_on(ALIAS)).await;
            }
        }
        assert_eq!(
            seen.repairs(),
            1,
            "{surface} premise: the repair fired, so the breaker assertion is about a \
             repairable rejection and not about an untouched seat",
        );
        assert_eq!(
            router
                .capacity_snapshot_for("m0", std::time::Instant::now())
                .expect("seat state slot exists")
                .circuit,
            crate::runtime_state::CircuitPhase::Closed,
            "{surface}: a repairable 4xx is a caller-shaped envelope fault, not this seat's \
             health signal, so it must never debit the breaker",
        );
    }
}

#[tokio::test]
async fn the_calibration_estimate_is_restamped_from_the_repaired_body() {
    // The estimate is stamped once, before the retry loop, from the request
    // as it stood then. A repair makes the dispatched payload SMALLER, and an
    // uncorrected stamp then describes bytes that were never sent -- which
    // biases the correction factor the window gate depends on.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "premise: the repair served");
    let stamped = dispatched
        .meta
        .calib_estimated_tokens
        .expect("every dispatched attempt stamps an estimate");
    let repaired = crate::context_trim::estimate_total_tokens(&seen.attempt(1));
    assert_eq!(
        stamped, repaired,
        "the stamped estimate must describe the REPAIRED payload -- the one the upstream will \
         price -- not the pre-repair request",
    );
}

// ---------------------------------------------------------------------------
// The aggregated WARN summary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_summary_reports_the_repair_and_its_confirmed_outcome() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, _seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    let summary = dispatched
        .meta
        .field_repair
        .as_ref()
        .expect("a fired repair records a summary");
    assert_eq!(summary.field_path, REJECTED_PATH, "the closed-table path");
    assert!(summary.repair_attempted, "the arm fired");
    assert!(summary.repair_succeeded, "and the repaired retry served");
    assert!(summary.learned, "and the verdict was persisted");
    assert_eq!(
        summary.state_key, "m0",
        "the sanitized state key of the repaired target",
    );
}

#[tokio::test]
async fn the_summary_claims_nothing_learned_when_the_repair_failed() {
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, _seen) = single_seat(ALIAS, Answer::AlwaysRejectField);

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    let summary = dispatched
        .meta
        .field_repair
        .as_ref()
        .expect("the arm fired, so a summary exists");
    assert!(summary.repair_attempted, "the arm fired");
    assert!(
        !summary.repair_succeeded,
        "the repaired variant was rejected too",
    );
    assert!(
        !summary.learned,
        "a summary must not claim a verdict was persisted on a path that persists nothing",
    );
}

// ---------------------------------------------------------------------------
// The injection seam's own properties
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_injected_resolution_is_visible_inside_a_dispatch() {
    // The property the whole fixture rests on: a thread-local injection set
    // in the test body IS visible inside the dispatch the test drives. If a
    // future runtime change moved the dispatch onto another thread, every
    // behavioral test here would silently stop exercising the repair, and
    // this is what would say so.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let _ = router.complete(req_on(ALIAS)).await;

    assert_eq!(
        seen.repairs(),
        1,
        "the dispatch observed the injected resolution; a zero here means the injection is \
         not visible on the dispatch's thread and every behavioral test above is vacuous",
    );
}

#[tokio::test]
async fn without_an_injection_no_walk_repairs_anything() {
    // The control for every test above, and the production posture: with no
    // provisional resolution in scope, the same fixture and the same
    // rejection body produce NO repair on any walk -- because production
    // attributes no field path to any envelope in this stage.
    let (complete_router, complete_seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let (stream_router, stream_seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let (count_router, count_seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let _ = complete_router.complete(req_on(ALIAS)).await;
    let _ = stream_router.stream(req_on(ALIAS)).await;
    let _ = count_router.count_tokens(req_on(ALIAS)).await;

    for (surface, seen) in [
        ("complete", &complete_seen),
        ("stream", &stream_seen),
        ("count_tokens", &count_seen),
    ] {
        assert_eq!(
            seen.repairs(),
            0,
            "{surface}: with no grounded parser, production must repair nothing",
        );
        assert_eq!(
            seen.calls(),
            1,
            "{surface}: and the rejection stands after one attempt",
        );
    }
}

// ---------------------------------------------------------------------------
// Lifecycle: mint, then a durable purge, then the chain preference stops
// acting -- all in one process, all through the landed protocol.
// ---------------------------------------------------------------------------

/// A two-target chain whose legs answer independently, so a chain-preference
/// assertion can tell "the demoted leg was skipped" from "the demoted leg
/// happened to succeed anyway".
fn two_seat_chain(
    alias: &str,
    first: Answer,
    second: Answer,
) -> (Router, Arc<Observed>, Arc<Observed>) {
    let config = chain_config(alias, 2);
    let mut router = Router::new(Arc::new(config));
    let seen0 = Arc::new(Observed::default());
    let seen1 = Arc::new(Observed::default());
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0",
            "p0",
            Arc::new(MockSeat::new(first, seen0.clone())) as Arc<dyn Provider>,
            "wire-0",
        )),
    );
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            Arc::new(MockSeat::new(second, seen1.clone())) as Arc<dyn Provider>,
            "wire-1",
        )),
    );
    router.install_resolved_models(models);
    (router, seen0, seen1)
}

#[tokio::test]
async fn a_durably_purged_field_verdict_stops_tailing_its_target() {
    // Mint through a genuine dispatch on leg `m0`: the field is carried, the
    // carried attempt is rejected, and the repaired retry is served -- the
    // confirmed-repair shape `a_successful_repaired_retry_commits_one_
    // learned_verdict` already pins as the one that commits a verdict. `m1`
    // never rejects, so it is the clean leg every chain-preference assertion
    // below reads against.
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen0, seen1) =
        two_seat_chain(ALIAS, Answer::ServeRepaired, Answer::ServeImmediately);

    let minted = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(minted.result.is_ok(), "premise: the mint's repair served");
    assert_eq!(
        minted.meta.learned_capabilities.len(),
        1,
        "premise: the mint committed exactly one verdict"
    );
    assert_eq!(seen1.calls(), 0, "premise: m0 alone served the mint");
    assert!(
        verdict_resident(&router, "m0"),
        "premise: the minted verdict is resident before the purge"
    );

    // Before the purge: the chain preference is already acting on the
    // resident negative, so a request carrying the same field tries the
    // clean leg first and never reaches the tailed one.
    let before = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(before.result.is_ok());
    assert_eq!(
        seen1.calls(),
        1,
        "the resident negative tails m0, so m1 is tried first"
    );
    assert_eq!(
        seen0.calls(),
        2,
        "m0 is not attempted again while its negative is acting"
    );

    // Mint -> durable purge: the landed two-phase protocol, not a shortcut
    // removal. The reservation's settlement is exactly what a caller commits
    // durably before finalizing; finalizing without that commit is what the
    // protocol forbids in production, but the commit's own durability is
    // covered where the capability key can be spelled on the wire.
    let capability_key = rejected_key();
    let reserved = match router.reserve_learned_capability_purge("m0", &capability_key) {
        PurgeOutcome::Reserved(reserved) => reserved,
        other => panic!(
            "premise: a resident entry under a live generation must reserve; got {}",
            match other {
                PurgeOutcome::Absent => "absent",
                PurgeOutcome::Busy => "busy",
                PurgeOutcome::Stale => "stale",
                PurgeOutcome::Reserved(_) => unreachable!(),
            }
        ),
    };
    let _settlement = reserved.settlement();
    let removed = router.finalize_learned_capability_purge(reserved);
    assert!(removed, "the purge must report the entry removed");
    assert!(
        !verdict_resident(&router, "m0"),
        "the purged verdict must no longer be resident"
    );

    // After the purge: the SAME chain preference no longer demotes m0 -- no
    // new reorderer, the existing partition simply has nothing acting to
    // read.
    let after = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(after.result.is_ok());
    assert_eq!(
        seen0.calls(),
        4,
        "m0 is tried first again: one carried attempt plus one repair"
    );
    assert_eq!(
        seen1.calls(),
        1,
        "m1 is not reached once m0 is no longer tailed"
    );
}

mod grounded_field_feature_keys_tests {
    use super::super::grounded_field_feature_keys;
    use super::{ALIAS, rejected_key, req_on};

    #[test]
    fn a_request_carrying_the_grounded_surface_yields_its_minted_key() {
        let req = req_on(ALIAS);

        let keys = grounded_field_feature_keys(&req);

        assert_eq!(
            keys,
            vec![rejected_key()],
            "the one grounded surface present on the request must mint its closed-table key",
        );
    }

    #[test]
    fn a_request_without_the_surface_yields_no_field_keys() {
        let mut req = req_on(ALIAS);
        req.routectl_internal.anthropic_thinking_display = None;
        req.reasoning = None;

        let keys = grounded_field_feature_keys(&req);

        assert!(
            keys.is_empty(),
            "a request carrying neither canonical carrier must derive no field key, \
             preserving current call sites' output when the surface is absent",
        );
    }

    #[test]
    fn only_the_exclude_carrier_still_yields_the_minted_key() {
        let mut req = req_on(ALIAS);
        req.routectl_internal.anthropic_thinking_display = None;

        let keys = grounded_field_feature_keys(&req);

        assert_eq!(
            keys,
            vec![rejected_key()],
            "the derived `reasoning.exclude` carrier alone is still a grounded presence",
        );
    }
}

include!("field_repair_parser_tests.rs");
