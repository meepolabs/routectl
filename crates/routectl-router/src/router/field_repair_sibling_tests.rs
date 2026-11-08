//! A settling request whose field identity a sibling request holds: it still
//! repairs its own attempt from the same rejection, but commits nothing, so
//! the identity's verdict is written by the holder alone. An acting verdict
//! is not contention: its row is dropped and the rejection stays terminal.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::stream::BoxStream;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};
use serde_json::json;
use tokio::sync::Barrier;

use crate::field_verdict::{FieldAdmission, FieldVerdictKey};
use crate::learned_capability::{EntryVerdict, ExportedEntry};
use crate::resolved::ResolvedModel;
use crate::router::probe_test_support::seat_chain_lane;
use crate::router::{DispatchMeta, Router, RouterOptions};

use super::ANTHROPIC_API_KIND;
use super::field_repair_tests::{
    ALIAS, REJECTED_PATH, carries_field, chain_config, field_reject_body, rejected_key, req_on,
    success_response, verdict_resident,
};
use super::provisional;

/// The remote base the fixture's seat egresses to: the default Anthropic
/// base, which may mint.
const REMOTE_BASE: &str = "https://api.anthropic.com";

/// How many sibling requests race the same identity.
const SIBLINGS: usize = 16;

/// Upper bound on the racing dispatch, so a rendezvous that never completes
/// fails the test rather than hanging the suite.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

/// Rejects every attempt carrying the field and serves every attempt without
/// it. With a rendezvous, every CARRIED attempt waits until all of them have
/// arrived, so every sibling is admitted before any rejection is answered.
struct FieldGateSeat {
    calls: AtomicUsize,
    rendezvous: Option<Barrier>,
}

impl FieldGateSeat {
    fn new(rendezvous: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            rendezvous: rendezvous.map(Barrier::new),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn answer(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !carries_field(req) {
            return Ok(());
        }
        if let Some(rendezvous) = &self.rendezvous {
            rendezvous.wait().await;
        }
        Err(Error::upstream("field-gate", 400, field_reject_body()))
    }
}

#[async_trait::async_trait]
impl Provider for FieldGateSeat {
    fn id(&self) -> &'static str {
        "field-gate"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("field-gate", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).await.map(|()| success_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("field-gate", "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Err(Error::normalize_response("field-gate", "unused"))
    }
}

/// A one-seat `anthropic-api` chain on the remote default base, answered by
/// `seat`.
fn router_on(seat: &Arc<FieldGateSeat>) -> Router {
    let mut router = Router::new(Arc::new(chain_config(ALIAS, 1)));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0".to_string(),
            "p0".to_string(),
            Arc::clone(seat) as Arc<dyn Provider>,
            "wire-0".to_string(),
        )),
    );
    router.install_resolved_models(models);
    router
}

/// The identity `plan_field_carry` keys the fixture's row on.
fn identity() -> FieldVerdictKey {
    FieldVerdictKey::new(&seat_chain_lane("m0"), REJECTED_PATH, ANTHROPIC_API_KIND)
        .expect("the grounded path mints an identity")
}

/// Claim the identity's single-flight slot the way a sibling request would.
fn hold_claim(router: &Router) -> crate::field_verdict::FieldRepairGuard<'_> {
    router
        .field_verdicts()
        .admit_provisional(
            &identity(),
            REMOTE_BASE,
            router.registry_generation(),
            Instant::now(),
        )
        .admitted()
        .expect("an unknown identity admits one claim")
}

fn is_contended(router: &Router) -> bool {
    matches!(
        router.field_verdicts().admit_provisional(
            &identity(),
            REMOTE_BASE,
            router.registry_generation(),
            Instant::now(),
        ),
        FieldAdmission::Contended
    )
}

/// Plant a resident verdict for the identity that ACTS for the next hour,
/// through the registry's carry-over import seam.
fn plant_acting_verdict(router: &Router) {
    let now = Instant::now();
    let lane = seat_chain_lane("m0").as_lane_key().to_string();
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: router.provider_kind_for_state_key(&lane).to_string(),
            state_key: lane,
            feature_key: rejected_key(),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: now,
            last_seen: now,
            expires_at: now + Duration::from_hours(1),
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

/// The repair records of `meta`: (repair attempted, succeeded, learned).
fn repairs(meta: &DispatchMeta) -> Vec<(bool, bool, bool)> {
    meta.field_repairs
        .iter()
        .map(|record| {
            (
                record.repair_attempted,
                record.repair_succeeded,
                record.learned,
            )
        })
        .collect()
}

/// With a sibling holding the identity's claim, the request is repaired
/// once and served, and commits nothing: no learn event, no resident verdict,
/// and the sibling's claim is untouched.
///
/// Mutation check: answer `None` for `FieldAdmission::Contended` in
/// `plan_field_carry` -> red (the rejection reaches the client).
#[tokio::test]
async fn a_request_whose_field_a_sibling_holds_is_repaired_and_commits_nothing() {
    // Arrange
    let _injection = provisional::inject(REJECTED_PATH);
    let seat = FieldGateSeat::new(None);
    let router = router_on(&seat);
    let held = hold_claim(&router);

    // Act
    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    // Assert
    assert!(
        dispatched.result.is_ok(),
        "the repaired retry serves: {:?}",
        dispatched.result.as_ref().err()
    );
    assert_eq!(seat.calls(), 2, "one carried attempt plus one repair");
    assert_eq!(
        repairs(&dispatched.meta),
        vec![(true, true, false)],
        "one repair, served, learned nothing"
    );
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "no learn row"
    );
    assert!(
        dispatched.meta.cleared_capabilities.is_empty(),
        "no clear row"
    );
    assert!(!verdict_resident(&router, "m0"), "no verdict is resident");
    assert!(is_contended(&router), "the sibling still holds the claim");
    drop(held);
}

/// The paired control: with no claim held, the same request commits exactly
/// one verdict.
#[tokio::test]
async fn a_request_with_no_sibling_claim_commits_one_verdict() {
    // Arrange
    let _injection = provisional::inject(REJECTED_PATH);
    let seat = FieldGateSeat::new(None);
    let router = router_on(&seat);

    // Act
    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    // Assert
    assert!(dispatched.result.is_ok(), "the repaired retry serves");
    assert_eq!(seat.calls(), 2, "one carried attempt plus one repair");
    assert_eq!(repairs(&dispatched.meta), vec![(true, true, true)]);
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    assert_eq!(
        dispatched.meta.learned_capabilities[0].capability_key,
        rejected_key()
    );
    assert!(verdict_resident(&router, "m0"), "the verdict is resident");
}

/// Sixteen requests race one unknown identity, each admitted before any
/// rejection is answered. Every request is repaired and served, and exactly
/// one of them commits the verdict.
///
/// Mutation check: admit a held slot in `admit_provisional` (every sibling
/// gets a guard) -> red (more than one commit).
#[tokio::test]
async fn racing_siblings_all_succeed_and_exactly_one_commits() {
    // Arrange
    let _injection = provisional::inject(REJECTED_PATH);
    let seat = FieldGateSeat::new(Some(SIBLINGS));
    let router = router_on(&seat);

    // Act
    let dispatches =
        (0..SIBLINGS).map(|_| router.complete_with_options(req_on(ALIAS), RouterOptions::new()));
    let dispatched = tokio::time::timeout(RACE_TIMEOUT, join_all(dispatches))
        .await
        .expect("every sibling reaches the rendezvous");

    // Assert
    for (idx, sibling) in dispatched.iter().enumerate() {
        assert!(
            sibling.result.is_ok(),
            "sibling {idx} serves: {:?}",
            sibling.result.as_ref().err()
        );
        assert_eq!(
            sibling.meta.field_repairs.len(),
            1,
            "sibling {idx} repaired once"
        );
    }
    assert_eq!(
        seat.calls(),
        2 * SIBLINGS,
        "one rejection and one repair each"
    );
    let commits: usize = dispatched
        .iter()
        .map(|sibling| sibling.meta.learned_capabilities.len())
        .sum();
    assert_eq!(commits, 1, "the claim holder alone commits");
    let learned_records = dispatched
        .iter()
        .flat_map(|sibling| repairs(&sibling.meta))
        .filter(|&(_, _, learned)| learned)
        .count();
    assert_eq!(learned_records, 1, "one repair record reports the commit");
}

/// An acting verdict is not contention: the row is dropped from the plan,
/// so the rejection reaches the client unrepaired and nothing is learned.
///
/// Mutation check: answer `FieldAdmission::Contended` for an acting state in
/// `admit_provisional` -> red (the request is repaired).
#[tokio::test]
async fn an_acting_verdict_drops_the_row_and_the_rejection_stays_terminal() {
    // Arrange
    let _injection = provisional::inject(REJECTED_PATH);
    let seat = FieldGateSeat::new(None);
    let router = router_on(&seat);
    plant_acting_verdict(&router);
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&identity(), Instant::now()),
        "premise: the verdict acts"
    );

    // Act
    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    // Assert
    assert!(
        dispatched.result.is_err(),
        "the rejection reaches the client"
    );
    assert_eq!(seat.calls(), 1, "no repaired retry");
    assert!(dispatched.meta.field_repairs.is_empty(), "no repair");
    assert!(dispatched.meta.learned_capabilities.is_empty());
}
