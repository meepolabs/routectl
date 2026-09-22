//! `Router::run_probe_pass`: the free due batch runs to completion first, then
//! at most one paid probe is attempted, and the scheduler's lifetime paid
//! counters land exactly once per outcome.
//!
//! # Why the dial fixture is reused rather than rebuilt
//!
//! The ordering and counter contracts this file pins are properties of the
//! SAME `RecordingLedger` / `RecordingProvider` pair `paid_probe_dial_tests.rs`
//! already builds a router around, so reusing `Dial` is what keeps a paid
//! call observable here the same way it is there: one shared recorder, not a
//! second independent double that could drift from the real dial path.

use super::*;

use std::sync::atomic::Ordering;

use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;
use crate::router::probe_lifecycle::PaidProbeCandidate;
use crate::router::probe_test_support::{FailingProvider, GROUNDED_PATH, remote_router};

/// A due free job on a router with no paid candidate and no paid cap: the
/// floor every other assertion in this file builds on.
fn idle_free_lane(router: &Router) {
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
}

// ---------------------------------------------------------------------------
// Idle tick: zero work, zero counters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pass_on_a_freshly_built_router_runs_no_work() {
    // Arrange: no queued job, no paid candidate.
    let provider = std::sync::Arc::new(FailingProvider {
        status: 500,
        calls: std::sync::atomic::AtomicUsize::new(0),
        count_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert_eq!(summary.free_validators_run, 0);
    assert!(!summary.paid_probe_attempted);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let snapshot = router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 0);
    assert_eq!(snapshot.paid_authorization_refusals_total, 0);
    assert_eq!(snapshot.paid_reservations_committed_total, 0);
}

// ---------------------------------------------------------------------------
// Free-before-paid ordering
// ---------------------------------------------------------------------------

/// A provider whose `count_tokens` answers a well-formed ZERO -- spending the
/// one-step free plan without settling it, which is the only shape that
/// exhausts a plan toward a paid candidate -- and whose `complete` counts its
/// calls, so the paid dial the SAME pass makes is directly observable.
struct ZeroCountCountingComplete {
    complete_calls: std::sync::atomic::AtomicUsize,
}

impl ZeroCountCountingComplete {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            complete_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn complete_calls(&self) -> usize {
        self.complete_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl routectl_core::Provider for ZeroCountCountingComplete {
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
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Ok(routectl_core::ChatResponse {
            model: "wire-model".to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
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
        Ok(routectl_core::TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

/// THE ordering contract: a due free batch whose step EXHAUSTS the plan
/// surfaces a paid candidate mid-pass, and the same pass must claim and dial
/// it.
///
/// The exhaustion is what makes this discriminating. A fixture with no paid
/// candidate reaches the same summary in either order, so it pins nothing;
/// here the candidate does not exist until the free half has run, and a pass
/// that asked the paid half first would find an empty list and dispatch no
/// call.
#[tokio::test]
async fn a_pass_runs_the_due_free_batch_before_attempting_a_paid_probe() {
    // Arrange: an admitted request grounds and activates the lane, leaving one
    // due free job whose zero count will spend the whole one-step plan. The
    // model carries a priced catalog row, without which the claimed candidate
    // would be refused for want of a profile and never dial.
    let provider = ZeroCountCountingComplete::new();
    let mut router = crate::router::probe_test_support::remote_router_with_paid_cap(
        provider.clone(),
        COUNTER_CAP,
    )
    .with_paid_probe_ledger(CountingLedger::new());
    install_priced_model(&mut router, provider.clone());
    let _ = router
        .complete(crate::router::probe_test_support::grounding_request())
        .await;
    // That admitted dispatch already reached `complete` once as the client call
    // the probe lane rode in on; only a later call can be the paid dial.
    let calls_before_pass = provider.complete_calls();

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert_eq!(
        summary.free_validators_run, 1,
        "fixture premise: the pass must run the one due free validator",
    );
    assert_eq!(
        router.probe_scheduler_snapshot().free_exhausted_total,
        1,
        "fixture premise: that free step must exhaust the plan, surfacing a \
         paid candidate mid-pass",
    );
    assert!(
        summary.paid_probe_attempted,
        "the candidate the free half surfaced this pass must be claimed by the \
         paid half of the SAME pass",
    );
    assert_eq!(
        provider.complete_calls(),
        calls_before_pass + 1,
        "the same pass must DIAL that candidate, not merely claim it",
    );
}

include!("probe_pass_counter_tests.rs");
