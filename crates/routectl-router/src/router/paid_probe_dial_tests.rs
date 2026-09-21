//! Dialing ONE paid completion on a committed authorization: that the
//! reservation is observed before the call, that the gate still decides, that
//! exactly one call is made, and what the body carries on the wire.
//!
//! # The ordered recorder is the instrument
//!
//! One shared recorder receives `reserve` from the ledger double and `complete`
//! from the provider double, so the ORDER of the two is a direct observation
//! rather than an inference from two independent counters. A pair of counters
//! can only say both happened; this says which came first, which is the whole
//! contract -- a call dispatched before its unit is committed is spend the
//! accounting layer never authorized, and no refund exists to undo it.
//!
//! # Every absence assertion is paired
//!
//! "Zero provider calls" is free on a fixture that could never have dialed, so
//! each such case names a POSITIVE control differing in exactly the fact under
//! test. Likewise "the breaker is untouched" is asserted on a fixture whose
//! failure threshold is ONE, so a single debit would be visible.

use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::stream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, Usage,
};

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{PROBE_OPERATION_TIMEOUT, ProbePayload, ProbeValidator};
use crate::resolved::ResolvedModel;
use crate::router::paid_probe_authorize::PaidProbeRefusal;
use crate::router::paid_probe_ledger::{PaidProbeLedger, PaidProbeReservation};
use crate::router::paid_probe_profile::{
    ADAPTIVE_MIN_VIABLE_MAX_TOKENS, LEGACY_MIN_VIABLE_MAX_TOKENS,
};
use crate::router::probe_failure_class::PaidProbeFailure;
use crate::router::probe_lifecycle::PaidProbeCandidate;
use crate::router::probe_test_support::GROUNDED_PATH;

/// A stamp date for a synthetic effective row. Staleness is not a fact under
/// test here.
const STAMP: &str = "2026-01-01";

/// The acting lane's `[providers]` key and model nickname.
const PROVIDER: &str = "p1";
const LANE: &str = "m1";

/// The SECOND lane, present only so "no fallback" has something to fall back
/// TO. Without it a zero-call assertion on a second seat is free.
const FALLBACK_PROVIDER: &str = "p2";
const FALLBACK_LANE: &str = "m2";

/// The wire model id both lanes resolve to.
const UPSTREAM: &str = "claude-sonnet-4-5";

/// A non-zero daily cap, so a cap-zero refusal is a fixture choice rather than
/// the default.
const CAP: u32 = 5;

/// The client and operator beta tokens the captured payload carries.
///
/// DISTINCT strings, and that is load-bearing: the egress filters the client
/// carrier through `allowed_betas` and exempts the operator carrier, so a body
/// that crossed them over would travel under a different effective header than
/// the request under test. Two identical tokens would make a swap invisible.
const CLIENT_BETA: &str = "client-only-flag-1";
const OPERATOR_BETA: &str = "operator-only-flag-1";

/// The body text every failing provider answer carries.
///
/// A DISTINCTIVE sentinel rather than a plausible message: the redaction case
/// asserts it never reaches a log line, and a generic word like "body" could
/// match incidental output and pass for the wrong reason.
const UPSTREAM_ERROR_BODY: &str = "upstream-detail-must-not-be-logged";

/// The recorder tokens. Closed set, so a typo cannot silently widen an
/// order assertion.
const RESERVE: &str = "reserve";
const COMPLETE: &str = "complete";

/// One shared, ORDERED record of what the accounting layer and the upstream
/// were asked to do.
///
/// Shared by both doubles deliberately. Two separate counters can establish
/// that a reservation and a call both happened, but not which came first --
/// and the contract this sidecar exists for is exactly the order.
#[derive(Clone)]
struct Recorder(Arc<parking_lot::Mutex<Vec<&'static str>>>);

impl Recorder {
    fn new() -> Self {
        Self(Arc::new(parking_lot::Mutex::new(Vec::new())))
    }

    fn note(&self, what: &'static str) {
        self.0.lock().push(what);
    }

    fn events(&self) -> Vec<&'static str> {
        self.0.lock().clone()
    }

    fn count(&self, what: &str) -> usize {
        self.0.lock().iter().filter(|seen| **seen == what).count()
    }
}

/// A ledger double that records its reservation on the shared recorder.
struct RecordingLedger {
    recorder: Recorder,
    answer: PaidProbeReservation,
}

impl RecordingLedger {
    fn committing(recorder: &Recorder) -> Arc<Self> {
        Arc::new(Self {
            recorder: recorder.clone(),
            answer: PaidProbeReservation::Committed { used: 2, cap: CAP },
        })
    }
}

#[async_trait::async_trait]
impl PaidProbeLedger for RecordingLedger {
    async fn reserve_paid_probe_unit(&self, _: &str, _: u32) -> PaidProbeReservation {
        self.recorder.note(RESERVE);
        self.answer.clone()
    }
}

/// How the provider double answers a completion.
#[derive(Clone, Copy)]
enum CompleteAnswer {
    /// A well-formed response.
    Ok,
    /// An upstream failure at this status.
    Failing(u16),
    /// Never answers, for the timeout and cancellation cases.
    Pending,
    /// PARKS inside the call until released, so a test can hold the call in
    /// flight at exactly the point a publication has to matter.
    ///
    /// The park is the instrument: without it a "the reload landed mid-call"
    /// test is a timing accident that would pass against an implementation
    /// which abandoned the call on supersession just as readily.
    Slow,
}

/// A provider double that records every `complete` on the shared recorder and
/// RETAINS the body it was handed.
///
/// Retaining the body is what makes the wire-shape cases assertions about what
/// was SENT rather than about what the builder returned.
struct RecordingProvider {
    recorder: Recorder,
    answer: CompleteAnswer,
    seen: parking_lot::Mutex<Vec<ChatRequest>>,
    /// `complete` calls only. Separate from the recorder's own tally so a test
    /// can read this lane's dials without filtering a shared log.
    complete_calls: AtomicUsize,
    /// Signalled once a call has been ENTERED, so a racing test can wait on
    /// that fact rather than sleeping and hoping.
    entered: tokio::sync::Semaphore,
    /// Awaited inside a `Slow` call; the racing side adds a permit to let it
    /// finish.
    release: tokio::sync::Semaphore,
}

impl RecordingProvider {
    fn new(recorder: &Recorder, answer: CompleteAnswer) -> Arc<Self> {
        Arc::new(Self {
            recorder: recorder.clone(),
            answer,
            seen: parking_lot::Mutex::new(Vec::new()),
            complete_calls: AtomicUsize::new(0),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.complete_calls.load(Ordering::Acquire)
    }

    /// Wait until a call is provably inside the provider.
    async fn wait_until_called(&self) {
        self.entered
            .acquire()
            .await
            .expect("the entry signal must not be closed")
            .forget();
    }

    /// Let a parked call finish.
    fn release(&self) {
        self.release.add_permits(1);
    }

    /// The ONE body this provider was handed, or a panic naming the real count.
    fn only_body(&self) -> ChatRequest {
        let seen = self.seen.lock();
        assert_eq!(
            seen.len(),
            1,
            "exactly one body must have reached the upstream",
        );
        seen[0].clone()
    }
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("recording", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.recorder.note(COMPLETE);
        self.complete_calls.fetch_add(1, Ordering::AcqRel);
        self.seen.lock().push(req);
        self.entered.add_permits(1);
        match self.answer {
            CompleteAnswer::Failing(status) => {
                Err(Error::upstream("recording", status, UPSTREAM_ERROR_BODY))
            }
            CompleteAnswer::Pending => std::future::pending().await,
            CompleteAnswer::Slow => {
                self.release
                    .acquire()
                    .await
                    .expect("the release signal must not be closed")
                    .forget();
                Ok(self.response())
            }
            CompleteAnswer::Ok => Ok(self.response()),
        }
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Ok(Box::pin(stream::iter(vec![Ok(ChatChunk::default())])))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

impl RecordingProvider {
    /// The one well-formed answer both success shapes return.
    fn response(&self) -> ChatResponse {
        ChatResponse {
            model: UPSTREAM.to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        }
    }
}

/// THE fully-priced control cell: both rates finite and positive, ceiling well
/// above either wire shape's floor.
fn fully_priced() -> EffectiveRow {
    let mut row = CatalogRow::sentinel();
    row.input_cost_per_token = Some(3.0e-6);
    row.output_cost_per_token = Some(1.5e-5);
    row.max_output_tokens = Some(64_000);
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: STAMP.to_string(),
    }
}

/// A fixture router plus the doubles wired into it, so a test reads the
/// ordering, the call counts, and the body from one value.
struct Dial {
    router: Router,
    recorder: Recorder,
    provider: Arc<RecordingProvider>,
    /// The SECOND lane's provider. Zero calls on it is what makes "no
    /// fallback" mean something.
    fallback_provider: Arc<RecordingProvider>,
}

impl Dial {
    /// Build the acting lane with `answer` and the LEGACY thinking shape.
    fn legacy(answer: CompleteAnswer) -> Self {
        Self::build(answer, false)
    }

    /// The same with the ADAPTIVE thinking shape, whose viable allowance is the
    /// smallest positive one.
    fn adaptive(answer: CompleteAnswer) -> Self {
        Self::build(answer, true)
    }

    /// Two anthropic-api lanes on two providers, `default` aliased to BOTH in
    /// order, a non-zero cap on each, the ledger installed, and one candidate
    /// seeded for the acting lane.
    ///
    /// The breaker threshold is ONE on both, which is load-bearing: at the
    /// default the breaker is disabled, so "a paid dial settled nothing on the
    /// breaker" would pass against code that settled it.
    fn build(answer: CompleteAnswer, adaptive: bool) -> Self {
        let recorder = Recorder::new();
        let provider = RecordingProvider::new(&recorder, answer);
        let fallback_provider = RecordingProvider::new(&recorder, CompleteAnswer::Ok);
        let mut config = Config::default();
        for name in [PROVIDER, FALLBACK_PROVIDER] {
            let mut entry = ProviderEntry::anthropic_api("literal:k");
            if let ProviderEntry::AnthropicApi {
                base_url, runtime, ..
            } = &mut entry
            {
                *base_url = "https://api.anthropic.com".to_string();
                runtime.circuit_failures = Some(1);
            }
            config.providers.insert(name.to_string(), entry);
            config
                .fidelity
                .paid_probe_daily_caps
                .insert(name.to_string(), CAP);
        }
        config
            .models
            .insert(LANE.to_string(), ModelEntry::new(PROVIDER, UPSTREAM));
        config.models.insert(
            FALLBACK_LANE.to_string(),
            ModelEntry::new(FALLBACK_PROVIDER, UPSTREAM),
        );
        config.aliases.insert(
            "default".to_string(),
            AliasValue::Chain(vec![LANE.to_string(), FALLBACK_LANE.to_string()]),
        );
        let mut router = Router::new(Arc::new(config));
        let mut models = std::collections::BTreeMap::new();
        models.insert(
            LANE.to_string(),
            Arc::new(
                ResolvedModel::new(
                    LANE,
                    PROVIDER,
                    Arc::clone(&provider) as Arc<dyn Provider>,
                    UPSTREAM,
                )
                .with_supports_adaptive_thinking(adaptive)
                .with_effective_row(fully_priced()),
            ),
        );
        models.insert(
            FALLBACK_LANE.to_string(),
            Arc::new(
                ResolvedModel::new(
                    FALLBACK_LANE,
                    FALLBACK_PROVIDER,
                    Arc::clone(&fallback_provider) as Arc<dyn Provider>,
                    UPSTREAM,
                )
                .with_effective_row(fully_priced()),
            ),
        );
        router.install_resolved_models(models);
        let router = router.with_paid_probe_ledger(RecordingLedger::committing(&recorder));
        let dial = Self {
            router,
            recorder,
            provider,
            fallback_provider,
        };
        dial.seed_candidate();
        dial
    }

    /// Seed one candidate for the acting lane at the router's LIVE
    /// incarnation, the way an exhausted free plan would have.
    ///
    /// Asserts its own premise: a fixture that silently seeded nothing would
    /// make every case below pass as `NoCandidate` for the wrong reason.
    fn seed_candidate(&self) {
        let before = self.router.all_recorded_paid_candidates_for_tests().len();
        self.router
            .paid_probe_candidates
            .lock()
            .push_back(PaidProbeCandidate {
                key: key(),
                incarnation: self.router.probe_incarnation(),
                validator: ProbeValidator::PaidCompletion,
                payload: payload(),
            });
        assert_eq!(
            self.router.all_recorded_paid_candidates_for_tests().len(),
            before + 1,
            "fixture premise: the candidate must actually be on the list",
        );
    }
}

/// The acting lane's identity.
fn key() -> FieldVerdictKey {
    FieldVerdictKey::new(LANE, GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// A bounded payload carrying BOTH beta sources and the Claude Code bit, built
/// through the production constructor so no fixture can hold a value the
/// capture path would have refused.
fn payload() -> ProbePayload {
    ProbePayload::new(
        GROUNDED_PATH,
        "summarized".to_string(),
        &[CLIENT_BETA.to_string()],
        &[OPERATOR_BETA.to_string()],
        true,
    )
    .expect("a modeled display token within every retention bound")
}

/// Drive one whole pass and unwrap the dial outcome, or panic naming the
/// refusal. The refusal cases below assert on `run_paid_probe` directly.
async fn dialed(dial: &Dial) -> PaidProbeDialOutcome {
    match dial.router.run_paid_probe().await {
        PaidProbePass::Dialed(outcome) => outcome,
        PaidProbePass::Refused(refusal) => panic!(
            "the fixture must authorize so there is a call to observe: {}",
            refusal.as_str(),
        ),
    }
}

// ---------------------------------------------------------------------------
// THE ordering contract: the unit is committed before the call is made
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paid_probe_dispatch_observes_committed_reservation() {
    // THE ordering assertion, on ONE shared ordered recorder rather than on two
    // independent counters: a call dispatched before its unit is committed is
    // spend the accounting layer never authorized, and the ledger has no refund
    // method to undo it.
    let dial = Dial::legacy(CompleteAnswer::Ok);

    let outcome = dialed(&dial).await;

    assert_eq!(outcome, PaidProbeDialOutcome::Completed);
    assert_eq!(
        dial.recorder.events(),
        vec![RESERVE, COMPLETE],
        "the reservation must be committed BEFORE the outbound call, and each \
         must happen exactly once",
    );
}

#[tokio::test]
async fn a_refused_pass_makes_no_call_and_leaves_the_recorder_empty() {
    // The NEGATIVE control for the ordering case above: with no candidate there
    // is nothing to authorize, so neither event occurs. Without this, the
    // ordering assertion could be satisfied by an implementation that recorded
    // the pair from somewhere other than the real calls.
    let dial = Dial::legacy(CompleteAnswer::Ok);
    dial.router.paid_probe_candidates.lock().clear();

    let pass = dial.router.run_paid_probe().await;

    assert!(
        matches!(pass, PaidProbePass::Refused(PaidProbeRefusal::NoCandidate)),
        "an empty candidate list must refuse the whole pass: {pass:?}",
    );
    assert!(
        dial.recorder.events().is_empty(),
        "a refused pass asks neither the accounting layer nor the upstream: {:?}",
        dial.recorder.events(),
    );
}

// The remaining groups live in sibling files to keep every file under the size
// ceiling. They compile into THIS module via `include!`, so the fixture helpers
// above stay in scope and no test's module path changes.
include!("paid_probe_dial_gate_tests.rs");
include!("paid_probe_dial_failure_tests.rs");
include!("paid_probe_dial_wire_tests.rs");
include!("paid_probe_dial_lifecycle_tests.rs");
