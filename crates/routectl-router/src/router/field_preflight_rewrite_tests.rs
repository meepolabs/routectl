// Pre-flight over a REWRITE row: an eligible verdict carries no stored value
// to write, so the planner neither applies nor canaries it.
// An `include!`d FRAGMENT of `field_preflight_tests.rs`.

// ---------------------------------------------------------------------------
// A rewrite verdict without a stored value is refused
// ---------------------------------------------------------------------------

/// A request carrying ONLY the test-only rewrite row's field.
fn req_rewrite_only(alias: &str) -> ChatRequest {
    let mut req = req_without_field(alias);
    req.chat_template_kwargs = Some(json!({
        super::super::field_repair::TEST_REWRITE_KWARG: "routectl-test-client-value",
    }));
    req
}

fn rewrite_verdict_key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(
        &crate::router::probe_test_support::seat_chain_lane(state_key),
        super::super::field_repair::TEST_REWRITE_PATH,
        ANTHROPIC,
    )
    .expect("a qualified path mints a key")
}

/// Plant a resident ACTING, acknowledged verdict on the rewrite row's identity.
fn plant_rewrite_eligible(router: &Router, state_key: &str) {
    let lane = crate::router::probe_test_support::seat_chain_lane(state_key);
    let feature_key = crate::field_capability::field_capability_key(
        super::super::field_repair::TEST_REWRITE_PATH,
    )
    .expect("the test path is a qualified path");
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            provider_kind: router
                .provider_kind_for_state_key(lane.as_lane_key())
                .to_string(),
            state_key: lane.as_lane_key().to_string(),
            feature_key: feature_key.clone(),
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
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        lane.as_lane_key(),
        &feature_key,
        ANTHROPIC,
    );
    router.field_verdicts().canaries().seed_from_rebuild(
        &rewrite_verdict_key(state_key),
        incarnation,
        1,
        false,
    );
    assert!(
        router.field_verdicts().preflight_eligible(
            &rewrite_verdict_key(state_key),
            router.registry_generation(),
            Instant::now(),
        ),
        "fixture premise: the rewrite verdict is pre-flight eligible",
    );
}

/// Mutation check: narrow the planner's non-drop refusal to the ceiling
/// transform -> red (the rewrite row is adopted as a drop).
#[test]
fn an_eligible_rewrite_verdict_without_a_stored_value_is_neither_applied_nor_canaried() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_rewrite_eligible(&router, "m0");
    let req = req_rewrite_only(ALIAS);

    // One full cadence interval and one more, so a canary that could be
    // claimed would have been.
    for _ in 0..=CANARY_INTERVAL {
        let (planned, decision) = plan(&router, &req, "m0");

        assert!(!decision.acted, "a rewrite verdict is never applied here");
        assert_eq!(decision.reason, super::FIELD_PREFLIGHT_NO_STORED_VALUE);
        assert_eq!(
            planned.chat_template_kwargs, req.chat_template_kwargs,
            "the field travels exactly as the client sent it",
        );
    }
    assert_eq!(router.field_repair_counters().preflight_actions, 0);
}
