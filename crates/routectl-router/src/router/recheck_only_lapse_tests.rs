//! A field negative whose row a free token count validates is recovered ONLY
//! by that count re-check, never by decay into an ordinary request.
//!
//! The re-check removes the negative only after its `cleared` row is
//! acknowledged durable. Were the negative allowed to lapse like any other, the
//! chain filter would admit it as a generic re-probe (and the reactive arm would
//! admit a repair slot), and the first ordinary 2xx carrying the field would
//! remove it with no durable acknowledgement -- before the re-check ran, and
//! with the re-check itself never queued, since activation asks for an ACTING
//! negative.
//!
//! The controls pin the other side: a lapsed catalog capability and a lapsed
//! field negative on a row no count validates still clear on an ordinary
//! success, exactly as before.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, normalize_capability_key,
};
use routectl_core::{ChatRequest, ToolDef};

use super::Router;
use super::probe_test_support::{
    GROUNDED_PATH, OkProvider, RECHECK_PATH, grounding_request, lane,
    remote_router_without_negative,
};
use crate::field_verdict::FieldVerdictKey;
use crate::learned_capability::{EntryVerdict, ExportedEntry};

const KIND: &str = "anthropic-api";

/// A resident self-identifying negative on `feature_key` for the fixture lane
/// `m1`, whose decay window ended a minute ago.
fn plant_lapsed_negative(router: &Router, feature_key: &str) {
    let now = Instant::now();
    let lapsed_at = now.checked_sub(Duration::from_mins(1)).unwrap_or(now);
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: KIND.to_string(),
            state_key: lane("m1").as_lane_key().to_string(),
            feature_key: feature_key.to_string(),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: lapsed_at,
            last_seen: lapsed_at,
            expires_at: lapsed_at,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

fn field_key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("a qualified fixture path")
}

fn resident(router: &Router, feature_key: &str) -> bool {
    router
        .learned_capabilities
        .export_entries()
        .iter()
        .any(|entry| entry.feature_key == feature_key)
}

fn ok_router() -> Router {
    remote_router_without_negative(Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    }))
}

/// Mutation check: make the lapse predicate ignore the re-check-only rule
/// (`has_lapsed` returning `is_expired` alone) -> red on every assertion here.
#[tokio::test]
async fn a_lapsed_recheck_validated_negative_survives_an_ordinary_success_and_queues_its_recheck() {
    // Arrange
    let router = ok_router();
    let key = field_key(RECHECK_PATH);
    plant_lapsed_negative(&router, &key);
    let identity =
        FieldVerdictKey::new(&lane("m1"), RECHECK_PATH, KIND).expect("a qualified path mints");

    // Act -- an ordinary request carrying the field, answered 200.
    let dispatched = router
        .complete_with_options(grounding_request(), Default::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert!(
        dispatched.meta.cleared_capabilities.is_empty(),
        "an ordinary success rode no clear out: {:?}",
        dispatched.meta.cleared_capabilities,
    );
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&identity, Instant::now()),
        "the negative is still acting after the ordinary success",
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        (snap.activations_total, snap.queued),
        (1, 1),
        "the free re-check is queued, and it is what decides recovery",
    );
}

/// Positive control: the generic decay path is untouched for every key the
/// re-check does not own. One row per key class, each its own lapsed negative
/// on its own router, so a failure names the class.
#[tokio::test]
async fn a_lapsed_negative_the_recheck_does_not_own_still_clears_on_an_ordinary_success() {
    let rows: [(&str, String, ChatRequest); 2] = [
        (
            "catalog capability",
            normalize_capability_key("web_search", KIND),
            ChatRequest {
                tools: Some(vec![ToolDef::Other(
                    serde_json::json!({"type": "web_search", "name": "t"}),
                )]),
                ..Default::default()
            },
        ),
        ("field row no count validates", field_key(GROUNDED_PATH), {
            let mut req = ChatRequest::default();
            req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
            req
        }),
    ];
    for (class, key, req) in rows {
        // Arrange
        let router = ok_router();
        plant_lapsed_negative(&router, &key);
        assert!(
            resident(&router, &key),
            "{class}: premise, the negative is planted"
        );

        // Act
        let dispatched = router.complete_with_options(req, Default::default()).await;

        // Assert
        assert!(
            dispatched.result.is_ok(),
            "{class}: {:?}",
            dispatched.result.err()
        );
        assert!(
            !resident(&router, &key),
            "{class}: the lapsed negative is cleared by the ordinary success",
        );
        assert!(
            dispatched
                .meta
                .cleared_capabilities
                .iter()
                .any(|cleared| cleared.capability_key == key),
            "{class}: the clear rides out on the meta: {:?}",
            dispatched.meta.cleared_capabilities,
        );
    }
}

/// The same validated row on a lane with no free token count keeps its decay:
/// with nothing to ask, the re-check could never retract it.
#[test]
fn only_a_lane_with_a_free_count_makes_a_validated_row_recheck_only() {
    let key = field_key(RECHECK_PATH);
    let lane_key = lane("m1").as_lane_key().to_string();

    assert!(super::negative_recovers_only_by_recheck(
        &lane_key, &key, KIND
    ));
    assert!(!super::negative_recovers_only_by_recheck(
        &lane_key,
        &key,
        "openai-compat"
    ));
    assert!(!super::negative_recovers_only_by_recheck(
        &lane_key,
        &field_key(GROUNDED_PATH),
        KIND
    ));
}

/// Plant an ACTING, confirmed field negative on `path` for lane `m1` whose
/// canary is due on the very next eligible request -- the state a cold boot's
/// replay seeds for a verdict it found acting.
fn plant_due_canary_verdict(router: &Router, path: &str) {
    let lane = lane("m1");
    super::probe_test_support::plant_acting_negative(router, &lane, path, KIND);
    let identity = FieldVerdictKey::new(&lane, path, KIND).expect("a qualified path mints");
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        lane.as_lane_key(),
        &field_key(path),
        KIND,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&identity, incarnation, 1, true);
}

/// A verdict the re-check owns is never canaried: a due canary would restore
/// the field, and its unrepaired success would clear the negative with no
/// durable acknowledgement. The pre-flight repair keeps applying instead.
///
/// Mutation check: drop the re-check-only exclusion from the canary claim in
/// `plan_one_row` -> red: the request is the canary and the negative clears.
#[tokio::test]
async fn a_due_canary_never_restores_a_field_the_recheck_owns() {
    // Arrange
    let router = ok_router().with_capability_writes_assumed_durable_for_tests();
    plant_due_canary_verdict(&router, RECHECK_PATH);
    let identity =
        FieldVerdictKey::new(&lane("m1"), RECHECK_PATH, KIND).expect("a qualified path mints");

    // Act
    let dispatched = router
        .complete_with_options(grounding_request(), Default::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert!(
        dispatched
            .meta
            .field_preflight
            .iter()
            .any(|record| record.acted),
        "the pre-flight repair applied: {:?}",
        dispatched.meta.field_preflight,
    );
    assert!(dispatched.meta.cleared_capabilities.is_empty());
    assert!(
        router
            .field_verdicts()
            .is_negative_acting(&identity, Instant::now()),
        "the negative is still acting",
    );
}

/// Control: a due canary on a field row no count validates still restores the
/// field, and its unrepaired success clears the negative as before.
#[tokio::test]
async fn a_due_canary_still_restores_a_field_the_recheck_does_not_own() {
    // Arrange
    let router = ok_router().with_capability_writes_assumed_durable_for_tests();
    plant_due_canary_verdict(&router, GROUNDED_PATH);
    let mut req = ChatRequest::default();
    req.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());

    // Act
    let dispatched = router.complete_with_options(req, Default::default()).await;

    // Assert
    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert!(
        !resident(&router, &field_key(GROUNDED_PATH)),
        "the canary's unrepaired success disproved and cleared the negative",
    );
}
