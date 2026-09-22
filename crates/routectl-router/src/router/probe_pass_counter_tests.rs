// Lifetime paid-probe counters: one exact arm per dial outcome and per
// refusal, at most one candidate claimed per pass, and a pure snapshot read.
//
// This sidecar builds its own doubles rather than reusing `Dial` (in
// `paid_probe_dial_tests.rs`): that fixture is private to its own module and
// built around a SHARED ordered recorder whose contract is "reservation
// before call" -- a different property than this file pins, which is "the
// right counter moved, exactly once, per outcome".

use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::router::paid_probe_ledger::{PaidProbeLedger, PaidProbeReservation};
use crate::router::probe_test_support::remote_router_with_paid_cap;

/// A stamp date for a synthetic effective row. Staleness is not a fact under
/// test here.
const COUNTER_STAMP: &str = "2026-01-01";

/// A non-zero daily cap, so a claimed candidate can reach a committed
/// reservation.
const COUNTER_CAP: u32 = 5;

/// A fully-priced, viable effective row -- the only shape
/// `paid_probe_profile` accepts as a candidate for the paid body.
fn counter_priced_row() -> EffectiveRow {
    let mut row = CatalogRow::sentinel();
    row.input_cost_per_token = Some(3.0e-6);
    row.output_cost_per_token = Some(1.5e-5);
    row.max_output_tokens = Some(64_000);
    EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: COUNTER_STAMP.to_string(),
    }
}

/// How the provider double answers `complete`.
#[derive(Clone, Copy)]
enum CounterAnswer {
    Ok,
    Failing(u16),
    /// Never answers, for the timeout case.
    Pending,
}

/// A provider double that answers `complete` per [`CounterAnswer`] and counts
/// its own calls, for the paid-dial counter contracts.
struct CounterProvider {
    answer: CounterAnswer,
    calls: std::sync::atomic::AtomicUsize,
}

impl CounterProvider {
    fn new(answer: CounterAnswer) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            answer,
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl routectl_core::Provider for CounterProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response("p1", "unused"))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.answer {
            CounterAnswer::Ok => Ok(routectl_core::ChatResponse {
                model: "wire-model".to_string(),
                usage: Some(routectl_core::Usage::default()),
                ..Default::default()
            }),
            CounterAnswer::Failing(status) => {
                Err(routectl_core::Error::upstream("p1", status, "body"))
            }
            CounterAnswer::Pending => std::future::pending().await,
        }
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<super::probe_test_support::BoxStreamAlias> {
        Err(routectl_core::Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::TokenCount> {
        Err(routectl_core::Error::upstream("p1", 500, "body"))
    }
}

/// A ledger double that always commits and counts its own calls, so a
/// cap-zero pass can assert it was never asked.
struct CountingLedger {
    calls: std::sync::atomic::AtomicUsize,
}

impl CountingLedger {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PaidProbeLedger for CountingLedger {
    async fn reserve_paid_probe_unit(&self, _: &str, _: u32) -> PaidProbeReservation {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        PaidProbeReservation::Committed {
            used: 1,
            cap: COUNTER_CAP,
        }
    }
}

/// The acting lane's identity, matching `remote_router_with_paid_cap`'s
/// single installed lane.
fn counter_key() -> FieldVerdictKey {
    FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity")
}

/// A bounded, viable paid payload for the seeded candidate.
fn counter_payload() -> crate::probe_scheduler::ProbePayload {
    crate::probe_scheduler::ProbePayload::new(
        GROUNDED_PATH,
        "summarized".to_string(),
        &[],
        &[],
        true,
    )
    .expect("a modeled display token within every retention bound")
}

/// Give `router`'s single resolved model a fully-priced effective row, so
/// `paid_probe_profile` can accept a candidate against it.
fn install_priced_model(
    router: &mut Router,
    provider: std::sync::Arc<dyn routectl_core::Provider>,
) {
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "m1".to_string(),
        std::sync::Arc::new(
            crate::resolved::ResolvedModel::new("m1", "p1", provider, "claude-sonnet-4-5")
                .with_effective_row(counter_priced_row()),
        ),
    );
    router.install_resolved_models(models);
}

/// One candidate on the acting lane at the router's live incarnation, the
/// way an exhausted free plan would have queued it.
fn seed_one_candidate(router: &Router) {
    router
        .paid_probe_candidates
        .lock()
        .push_back(PaidProbeCandidate {
            key: counter_key(),
            incarnation: router.probe_incarnation(),
            validator: ProbeValidator::PaidCompletion,
            payload: counter_payload(),
        });
}

// ---------------------------------------------------------------------------
// Exact counters, one arm per dial outcome
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_paid_probe_counts_exactly_the_completed_arm() {
    // Arrange
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert!(summary.paid_probe_attempted);
    assert_eq!(provider.calls(), 1);
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 1);
    assert_eq!(snapshot.paid_provider_calls_started_total, 1);
    assert_eq!(snapshot.paid_completed_total, 1);
    assert_eq!(snapshot.paid_provider_failed_total, 0);
    assert_eq!(snapshot.paid_timeouts_total, 0);
    assert_eq!(snapshot.paid_gate_deferrals_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
}

#[tokio::test]
async fn a_failed_paid_probe_counts_exactly_the_provider_failed_arm() {
    // Arrange
    let provider = CounterProvider::new(CounterAnswer::Failing(503));
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);

    // Act
    router.run_probe_pass().await;

    // Assert
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 1);
    assert_eq!(snapshot.paid_provider_calls_started_total, 1);
    assert_eq!(snapshot.paid_completed_total, 0);
    assert_eq!(snapshot.paid_provider_failed_total, 1);
    assert_eq!(snapshot.paid_timeouts_total, 0);
    assert_eq!(snapshot.paid_gate_deferrals_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_paid_probe_counts_exactly_the_timed_out_arm() {
    // Arrange
    let provider = CounterProvider::new(CounterAnswer::Pending);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);

    // Act
    tokio::time::timeout(
        crate::probe_scheduler::PROBE_OPERATION_TIMEOUT * 4,
        router.run_probe_pass(),
    )
    .await
    .expect("the pass itself must bound the paid call and return");

    // Assert
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 1);
    assert_eq!(snapshot.paid_provider_calls_started_total, 1);
    assert_eq!(snapshot.paid_completed_total, 0);
    assert_eq!(snapshot.paid_provider_failed_total, 0);
    assert_eq!(snapshot.paid_timeouts_total, 1);
    assert_eq!(snapshot.paid_gate_deferrals_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
}

#[tokio::test]
async fn a_gate_deferred_paid_probe_counts_exactly_the_gate_deferred_arm() {
    // Arrange: an open breaker on the acting lane defers the call AFTER the
    // reservation commits, per `run_paid_probe`'s own ordering.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);
    router.force_open_breaker("m1", std::time::Duration::from_mins(5));

    // Act
    router.run_probe_pass().await;

    // Assert
    assert_eq!(
        provider.calls(),
        0,
        "a deferred call must never reach the upstream"
    );
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 1);
    assert_eq!(snapshot.paid_provider_calls_started_total, 0);
    assert_eq!(snapshot.paid_gate_deferrals_total, 1);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
}

#[tokio::test]
async fn a_refused_paid_probe_counts_exactly_the_refused_arm_and_commits_no_reservation() {
    // Arrange: no ledger installed, so authorization refuses `NoLedger`
    // before any reservation call -- a `Refused`, non-`NoCandidate` outcome
    // with zero accounting cost.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP);
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);

    // Act
    router.run_probe_pass().await;

    // Assert
    assert_eq!(provider.calls(), 0);
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_authorization_refusals_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 0);
    assert_eq!(snapshot.paid_provider_calls_started_total, 0);
    assert_eq!(snapshot.paid_completed_total, 0);
    assert_eq!(snapshot.paid_provider_failed_total, 0);
    assert_eq!(snapshot.paid_timeouts_total, 0);
    assert_eq!(snapshot.paid_gate_deferrals_total, 0);
}

#[tokio::test]
async fn an_idle_pass_counts_nothing_the_no_candidate_case_stays_uncounted() {
    // Arrange: no queued candidate at all.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert!(!summary.paid_probe_attempted);
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
    assert_eq!(snapshot.paid_reservations_committed_total, 0);
}

// ---------------------------------------------------------------------------
// A cap of zero still runs free work but never touches the ledger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_zero_cap_lane_runs_the_free_batch_but_never_calls_the_ledger() {
    // Arrange: cap zero refuses `CapZero` before the ledger-installed check,
    // so a ledger double planted here must observe zero calls.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let ledger = CountingLedger::new();
    let mut router =
        remote_router_with_paid_cap(provider.clone(), 0).with_paid_probe_ledger(ledger.clone());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);
    idle_free_lane(&router);

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert_eq!(summary.free_validators_run, 1);
    assert_eq!(
        ledger.calls(),
        0,
        "a zero cap must refuse before any reservation call"
    );
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_authorization_refusals_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 0);
}

// ---------------------------------------------------------------------------
// A free failure does not skip the paid half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_free_validator_failure_still_attempts_the_paid_half() {
    // Arrange: a due free job that will fail, plus a paid candidate already
    // queued from an earlier tick, so the paid half has something to claim
    // regardless of how the free job settles this pass.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);
    idle_free_lane(&router);

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert_eq!(summary.free_validators_run, 1);
    assert!(summary.paid_probe_attempted);
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
}

// ---------------------------------------------------------------------------
// At most one candidate is claimed per pass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_queued_candidates_need_two_passes_to_both_be_claimed() {
    // Arrange: two candidates queued for the same lane, so a pass that
    // drained more than one would be directly observable as an extra call.
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);
    seed_one_candidate(&router);

    // Act
    let first = router.run_probe_pass().await;

    // Assert: exactly one call, one candidate still queued.
    assert!(first.paid_probe_attempted);
    assert_eq!(provider.calls(), 1);
    assert_eq!(router.all_recorded_paid_candidates_for_tests().len(), 1);
    let after_first = router.probe_scheduler_snapshot();
    assert_eq!(after_first.paid_candidate_attempts_total, 1);

    // Act: a second pass claims the remaining candidate.
    let second = router.run_probe_pass().await;

    // Assert
    assert!(second.paid_probe_attempted);
    assert_eq!(provider.calls(), 2);
    assert!(router.all_recorded_paid_candidates_for_tests().is_empty());
    let after_second = router.probe_scheduler_snapshot();
    assert_eq!(after_second.paid_candidate_attempts_total, 2);
    assert_eq!(after_second.paid_completed_total, 2);
}

// ---------------------------------------------------------------------------
// Cancellation: the irreversible milestones stay counted, the terminal
// outcome does not appear
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancelling_a_pass_mid_call_keeps_the_irreversible_milestones_counted() {
    // Arrange: a provider whose `complete` never answers, so the pass is
    // provably sitting in the dial when it is cancelled -- past the claim, past
    // the committed reservation, and past the dispatch.
    let provider = CounterProvider::new(CounterAnswer::Pending);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);

    // Act: drop the whole pass future WELL BEFORE its own operation timeout
    // could settle the call, which is what shutdown does to a driver tick.
    let cancelled = tokio::time::timeout(
        crate::probe_scheduler::PROBE_OPERATION_TIMEOUT / 2,
        router.run_probe_pass(),
    )
    .await;

    // Assert
    assert!(
        cancelled.is_err(),
        "fixture premise: the pass must still be in the dial when it is dropped",
    );
    assert_eq!(
        provider.calls(),
        1,
        "fixture premise: the paid call must have been dispatched before the \
         cancellation",
    );
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(
        snapshot.paid_candidate_attempts_total, 1,
        "the claim is irreversible: the candidate is off the list",
    );
    assert_eq!(
        snapshot.paid_reservations_committed_total, 1,
        "the committed unit is spent, and there is no refund",
    );
    assert_eq!(
        snapshot.paid_provider_calls_started_total, 1,
        "the upstream may have received the request",
    );
    assert_eq!(
        snapshot.paid_completed_total, 0,
        "no terminal outcome exists: the pass never settled",
    );
    assert_eq!(snapshot.paid_provider_failed_total, 0);
    assert_eq!(snapshot.paid_timeouts_total, 0);
    assert_eq!(snapshot.paid_gate_deferrals_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
    assert_eq!(
        snapshot.in_flight, 0,
        "and the cancelled dial releases its concurrency slot",
    );
}

// ---------------------------------------------------------------------------
// A snapshot read is pure: it changes nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reading_the_snapshot_repeatedly_reports_no_new_activity() {
    // Arrange
    let provider = CounterProvider::new(CounterAnswer::Ok);
    let mut router = remote_router_with_paid_cap(provider.clone(), COUNTER_CAP)
        .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    seed_one_candidate(&router);
    router.run_probe_pass().await;
    let first = router.probe_scheduler_snapshot();

    // Act: read the snapshot several more times, with no intervening pass.
    let second = router.probe_scheduler_snapshot();
    let third = router.probe_scheduler_snapshot();

    // Assert
    assert_eq!(
        first.paid_candidate_attempts_total,
        second.paid_candidate_attempts_total
    );
    assert_eq!(
        second.paid_candidate_attempts_total,
        third.paid_candidate_attempts_total
    );
    assert_eq!(first.paid_completed_total, third.paid_completed_total);
    assert_eq!(provider.calls(), 1, "a snapshot read must dispatch no call");
}
