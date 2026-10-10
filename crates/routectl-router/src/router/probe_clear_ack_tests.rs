//! Settling a re-check probe: an accepted field clears its negative only once
//! the clear is acknowledged durable, and a rejection naming the same field
//! refreshes it.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};

use super::Router;
use super::field_repair::provisional;
use super::probe_test_support::{
    AckAnswer, BoxStreamAlias, FailingProvider, GROUNDED_PATH, OkProvider, RECHECK_PATH,
    RecordingClearAck, grounding_request, lane, remote_router,
};
use crate::field_verdict::FieldVerdictKey;
use crate::learned_capability::{GenerationOutcome, PurgePreparation};
use crate::probe_scheduler::{PROBE_BACKOFF_CEILING, PROBE_MAX_ATTEMPTS};

fn recheck_key() -> FieldVerdictKey {
    FieldVerdictKey::new(&lane("m1"), RECHECK_PATH, "anthropic-api").expect("identity")
}

fn ok_provider() -> Arc<OkProvider> {
    Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    })
}

fn is_acting(router: &Router) -> bool {
    router
        .field_verdicts()
        .is_negative_acting(&recheck_key(), Instant::now())
}

fn resident_incarnation(router: &Router) -> u64 {
    let key = recheck_key();
    router.learned_capabilities.resident_incarnation_for_tests(
        key.registry_key(),
        key.capability_key(),
        key.provider_kind(),
    )
}

/// The resident re-check row's `(observations, last_seen)`.
fn resident_row(router: &Router) -> (u32, Instant) {
    let key = recheck_key();
    router
        .learned_capabilities
        .snapshot()
        .into_iter()
        .find(|row| row.state_key == key.registry_key() && row.feature_key == key.capability_key())
        .map(|row| (row.observations, row.last_seen))
        .expect("the re-check negative is resident")
}

/// A router whose admitted request queued one re-check on `m1`.
async fn router_with_queued_recheck(provider: Arc<dyn Provider>) -> Router {
    let router = remote_router(provider);
    let _ = router.complete(grounding_request()).await;
    assert_eq!(
        router.probe_scheduler_snapshot().queued,
        1,
        "premise: queued"
    );
    router
}

#[tokio::test]
async fn an_acknowledged_clear_removes_the_negative_and_resolves_the_job() {
    let ack = RecordingClearAck::new(AckAnswer::Durable);
    let router = router_with_queued_recheck(ok_provider())
        .await
        .with_probe_clear_acknowledgement(ack.clone());
    let incarnation = resident_incarnation(&router);

    router.run_due_probes().await;

    assert!(!is_acting(&router), "the disproved negative is cleared");
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.resolved_total, 1);
    assert_eq!(snap.tombstoned, 1, "a resolved re-check is terminal");
    assert_eq!(snap.clear_persistence_failures_total, 0);
    let seen = ack.seen.lock();
    assert_eq!(seen.len(), 1, "exactly one clear submitted");
    assert_eq!(seen[0].capability_key, recheck_key().capability_key());
    assert_eq!(seen[0].state_key, recheck_key().registry_key());
    assert_eq!(
        seen[0].incarnation, incarnation,
        "the clear names the version the probe asked about"
    );
}

#[tokio::test]
async fn a_refused_clear_keeps_the_negative_acting_and_the_job_retryable() {
    let ack = RecordingClearAck::new(AckAnswer::Refused);
    let router = router_with_queued_recheck(ok_provider())
        .await
        .with_probe_clear_acknowledgement(ack.clone());

    router.run_due_probes().await;

    assert_eq!(ack.seen.lock().len(), 1, "the clear was submitted");
    assert_not_durable_outcome(&router);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_clear_times_out_and_keeps_the_job_retryable() {
    let ack = RecordingClearAck::new(AckAnswer::Silent);
    let router = router_with_queued_recheck(ok_provider())
        .await
        .with_probe_clear_acknowledgement(ack.clone());

    router.run_due_probes().await;

    assert_eq!(ack.seen.lock().len(), 1, "the clear was submitted");
    assert_not_durable_outcome(&router);
}

fn assert_not_durable_outcome(router: &Router) {
    assert!(is_acting(router), "an unacknowledged clear removes nothing");
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.resolved_total, 0,
        "not resolved without a durable clear"
    );
    assert_eq!(snap.tombstoned, 0, "no tombstone, so later traffic may ask");
    assert_eq!(snap.backing_off, 1, "the job stays retryable");
    assert_eq!(snap.clear_persistence_failures_total, 1);
}

#[tokio::test]
async fn with_no_acknowledgement_installed_a_recheck_is_never_resolved() {
    let router = router_with_queued_recheck(ok_provider()).await;

    router.run_due_probes().await;
    let mut later = Instant::now();
    for _ in 1..PROBE_MAX_ATTEMPTS {
        later += PROBE_BACKOFF_CEILING;
        let lease = router
            .lease_due_probe(later)
            .expect("a retryable job leases again after its backoff");
        router.execute_leased_probe_for_tests(lease).await;
    }

    assert!(
        is_acting(&router),
        "nothing may clear without a durable write"
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.resolved_total, 0);
    assert_eq!(
        snap.clear_persistence_failures_total,
        u64::from(PROBE_MAX_ATTEMPTS)
    );
    assert_eq!(snap.abandoned_total, 1, "the attempt cap still bounds it");
}

/// A count provider that runs `hook` while the probe is in flight.
struct MidFlightProvider {
    hook: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[async_trait::async_trait]
impl Provider for MidFlightProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "wire-model".to_string(),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        if let Some(hook) = self.hook.get() {
            hook();
        }
        Ok(TokenCount {
            input_tokens: 5,
            extras: serde_json::Map::new(),
        })
    }
}

/// Purge the re-check negative through the registry's own reservation, then,
/// when `relearn`, learn it again as live traffic would.
fn purge_hook(
    registry: Arc<crate::learned_capability::LearnedCapabilityRegistry>,
    relearn: bool,
) -> Box<dyn Fn() + Send + Sync> {
    Box::new(move || {
        let key = recheck_key();
        let generation = registry.generation();
        let PurgePreparation::Reserved(lease) = registry.prepare_purge(
            generation,
            key.registry_key(),
            key.capability_key(),
            key.provider_kind(),
        ) else {
            panic!("premise: the negative is resident and purgeable");
        };
        assert!(registry.finalize_purge(lease));
        if relearn {
            let observed = registry.observe_field_verdict_in_generation(
                generation,
                &key,
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                Instant::now(),
            );
            assert!(matches!(observed, GenerationOutcome::Applied { .. }));
        }
    })
}

async fn run_with_mid_flight(relearn: bool) -> (Router, Arc<RecordingClearAck>, u64) {
    // `complete` succeeds: a failing admitted request would open the
    // threshold-one breaker and the probe would defer instead of dialing.
    let provider = Arc::new(MidFlightProvider {
        hook: OnceLock::new(),
    });
    let ack = RecordingClearAck::new(AckAnswer::Durable);
    let router = router_with_queued_recheck(provider.clone())
        .await
        .with_probe_clear_acknowledgement(ack.clone());
    let asked = resident_incarnation(&router);
    assert!(
        provider
            .hook
            .set(purge_hook(
                Arc::clone(&router.learned_capabilities),
                relearn
            ))
            .is_ok()
    );
    router.run_due_probes().await;
    (router, ack, asked)
}

#[tokio::test]
async fn a_negative_relearned_mid_flight_is_not_cleared() {
    let (router, ack, asked) = run_with_mid_flight(true).await;

    assert!(
        ack.seen.lock().is_empty(),
        "no clear for a superseded version"
    );
    assert!(is_acting(&router), "the relearned negative survives");
    assert_ne!(
        resident_incarnation(&router),
        asked,
        "premise: the relearn minted a new version"
    );
    assert_eq!(router.probe_scheduler_snapshot().resolved_total, 0);
}

#[tokio::test]
async fn a_negative_purged_mid_flight_emits_no_clear() {
    let (router, ack, _) = run_with_mid_flight(false).await;

    assert!(ack.seen.lock().is_empty(), "the purge already cleared it");
    assert!(!is_acting(&router));
    assert_eq!(router.probe_scheduler_snapshot().resolved_total, 1);
}

#[tokio::test]
async fn a_rejection_naming_the_rechecked_field_refreshes_the_negative() {
    let ack = RecordingClearAck::new(AckAnswer::Durable);
    let router = router_with_queued_recheck(Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    }))
    .await
    .with_probe_clear_acknowledgement(ack.clone());
    let (observations, last_seen) = resident_row(&router);
    std::thread::sleep(std::time::Duration::from_millis(5));

    let _named = provisional::inject(RECHECK_PATH);
    router.run_due_probes().await;

    let (refreshed_observations, refreshed_last_seen) = resident_row(&router);
    assert!(refreshed_last_seen > last_seen, "last_seen advanced");
    assert_eq!(refreshed_observations, observations + 1);
    assert!(is_acting(&router));
    assert!(ack.seen.lock().is_empty(), "a rejection never clears");
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.resolved_total, 0);
    assert_eq!(snap.free_exhausted_total, 1, "the step was spent");
}

#[tokio::test]
async fn a_rejection_naming_another_field_does_not_refresh_the_negative() {
    let router = router_with_queued_recheck(Arc::new(FailingProvider {
        status: 400,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    }))
    .await;
    let before = resident_row(&router);

    let _named = provisional::inject(GROUNDED_PATH);
    router.run_due_probes().await;

    assert_eq!(resident_row(&router), before, "the negative is untouched");
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.abandoned_probe_requests_total, 1);
    assert_eq!(snap.free_exhausted_total, 0);
}

/// An acknowledgement that relearns the negative while its write is pending,
/// then reports the write durable.
struct RelearningAck {
    relearn: Box<dyn Fn() + Send + Sync>,
}

#[async_trait::async_trait]
impl super::ProbeClearAcknowledgement for RelearningAck {
    async fn acknowledge_clear(&self, _: super::CapabilityClearedEvent) -> bool {
        (self.relearn)();
        true
    }
}

#[tokio::test]
async fn a_negative_relearned_while_its_clear_is_pending_survives_the_ack() {
    let router = router_with_queued_recheck(ok_provider()).await;
    let ack = Arc::new(RelearningAck {
        relearn: purge_hook(Arc::clone(&router.learned_capabilities), true),
    });
    let router = router.with_probe_clear_acknowledgement(ack);
    let asked = resident_incarnation(&router);

    router.run_due_probes().await;

    assert!(is_acting(&router), "the clear named the superseded version");
    assert_ne!(resident_incarnation(&router), asked, "premise: relearned");
    assert_eq!(router.probe_scheduler_snapshot().resolved_total, 0);
}
