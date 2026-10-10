// The tier a canary confirmation re-observes at: the verdict's own, never the
// tier of the rejection the canary provoked.
// An `include!`d FRAGMENT of `field_canary_settlement_tests.rs`.

/// Plant an ACTING verdict learned at the INFERRED tier (two observations, the
/// corroborated state), pre-flight eligible and with its canary due.
fn plant_inferred_eligible_with_canary_due(router: &Router, state_key: &str) {
    let stamped = Instant::now();
    let lane = crate::router::probe_test_support::seat_chain_lane(state_key);
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            provider_kind: router
                .provider_kind_for_state_key(lane.as_lane_key())
                .to_string(),
            state_key: lane.as_lane_key().to_string(),
            feature_key: grounded_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::Inferred,
            observations: 2,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: routectl_core::capability::FailurePhase::F1,
            source: routectl_core::capability::EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    let incarnation = resident_incarnation(router, state_key);
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key(state_key),
        incarnation,
        1,
        true,
    );
}

/// Mutation check: pass `SignalTier::SelfIdentifying` instead of `tier` to the
/// observation in `record_canary_confirmation` -> red on the resident tier;
/// hardcode it on the event row instead -> red on the event tier.
#[tokio::test]
async fn a_canary_confirmation_keeps_the_tier_of_the_verdict_it_confirms() {
    // The injected resolution is SELF-IDENTIFYING, so a confirmation that took
    // the provoked rejection's tier would promote the inferred verdict.
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeRepaired);
    plant_inferred_eligible_with_canary_due(&router, "m0");

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false],
        "premise: the canary restored the field and the repaired retry served",
    );
    let snap = canary_snapshot(&router, "m0").expect("resident");
    assert_eq!(
        snap.last_outcome,
        Some(CanaryOutcome::Confirmed),
        "premise: the canary confirmed",
    );
    let event = dispatched
        .meta
        .learned_capabilities
        .first()
        .expect("the confirmation rides out a row");
    assert_eq!(
        event.signal_tier,
        routectl_core::capability::SignalTier::Inferred,
        "the confirmation row carries the confirmed verdict's tier",
    );
    let resident = router
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| entry.feature_key == grounded_key())
        .expect("the verdict stays resident");
    assert_eq!(
        resident.signal_tier,
        routectl_core::capability::SignalTier::Inferred,
        "a canary re-tests the field; it does not re-attribute the rejection",
    );
}
