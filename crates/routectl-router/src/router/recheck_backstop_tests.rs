//! A negative the free token-count re-check owns stays recoverable when that
//! re-check ends without an answer.
//!
//! The re-check is the only recovery such a negative has inside its backstop:
//! it never lapses at its decay deadline. Once its job is tombstoned for the
//! incarnation nothing would ever ask again, so past `expires_at +
//! RECHECK_BACKSTOP` the negative falls back to the generic re-probe.
//!
//! Every read takes an explicit `Instant` derived forward from the planted
//! deadline, never one subtracted from the present: a low-uptime host cannot
//! reach that far back.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use super::Router;
use super::probe_test_support::{RECHECK_PATH, ZeroCountProvider, lane, remote_router};
use crate::field_verdict::FieldVerdictKey;
use crate::learned_capability::{RECHECK_BACKSTOP, RoutingDecision};
use crate::probe_scheduler::{ProbeActivation, ProbeValidator};

const KIND: &str = "anthropic-api";

fn recheck_key() -> String {
    crate::field_capability::field_capability_key(RECHECK_PATH).expect("a qualified fixture path")
}

/// A router holding an acting re-check-owned negative on lane `m1` whose
/// re-check job has run out of its free plan and been tombstoned.
async fn router_with_tombstoned_recheck() -> Router {
    let router = remote_router(Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    }));
    let identity =
        FieldVerdictKey::new(&lane("m1"), RECHECK_PATH, KIND).expect("a qualified path mints");
    router.activate_probe_lane(&identity, ProbeValidator::CountTokens);
    router.run_due_probes().await;
    assert_eq!(
        router.activate_probe_lane(&identity, ProbeValidator::CountTokens),
        ProbeActivation::Tombstoned,
        "premise: the re-check is tombstoned, so it will never answer",
    );
    router
}

fn planted_expiry(router: &Router) -> Instant {
    router
        .learned_capabilities
        .export_entries()
        .into_iter()
        .find(|entry| entry.feature_key == recheck_key())
        .expect("premise: the re-check-owned negative is resident")
        .expires_at
}

fn decision_at(router: &Router, at: Instant) -> RoutingDecision {
    router.learned_capabilities.acting_negative_for(
        lane("m1").as_lane_key(),
        &recheck_key(),
        KIND,
        at,
    )
}

/// Mutation check: drop the backstop arm from `negative_lapsed` (lapse at
/// `expires_at` for every negative) -> red at the decay-deadline assertion.
/// Make the backstop unbounded (never lapse a re-check-owned negative) -> red
/// at the past-the-backstop assertion.
#[tokio::test]
async fn a_tombstoned_recheck_negative_is_admitted_for_a_reprobe_only_past_the_backstop() {
    // Arrange
    let router = router_with_tombstoned_recheck().await;
    let expires_at = planted_expiry(&router);
    let backstop = expires_at + RECHECK_BACKSTOP;

    // Act
    let at_deadline = decision_at(&router, expires_at);
    let just_before_backstop = decision_at(
        &router,
        expires_at + RECHECK_BACKSTOP.saturating_sub(Duration::from_secs(1)),
    );
    let past_backstop = decision_at(&router, backstop);

    // Assert
    assert!(
        matches!(at_deadline, RoutingDecision::RouteAway { .. }),
        "the re-check owns recovery at the decay deadline: {at_deadline:?}",
    );
    assert!(
        matches!(just_before_backstop, RoutingDecision::RouteAway { .. }),
        "still acting inside the backstop: {just_before_backstop:?}",
    );
    assert_eq!(
        past_backstop,
        RoutingDecision::ProbeAdmitted,
        "past the backstop the negative falls back to the generic re-probe",
    );
}

/// The snapshot carries the re-check ownership the display reads, so the
/// doctor applies the same backstop without re-deriving the table.
#[tokio::test]
async fn the_snapshot_marks_a_recheck_owned_negative() {
    // Arrange
    let router = router_with_tombstoned_recheck().await;

    // Act
    let row = router
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| entry.feature_key == recheck_key())
        .expect("premise: the re-check-owned negative is resident");

    // Assert
    assert!(row.recheck_owned);
    assert_eq!(
        row.expires_at,
        planted_expiry(&router),
        "the snapshot reports the raw decay deadline, not the backstop",
    );
}
