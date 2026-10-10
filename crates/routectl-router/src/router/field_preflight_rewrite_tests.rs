// Pre-flight over a REWRITE row: a verdict applies only with the value stored
// for its own incarnation, and one without a value is neither applied nor
// canaried.
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

/// Mutation check: make `preflight_edit` drop a rewrite row that has no stored
/// value -> red (the rewrite row is adopted as a drop).
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

// ---------------------------------------------------------------------------
// A rewrite verdict restored by a warm rebuild
// ---------------------------------------------------------------------------

/// A ledger holding one persisted rewrite verdict on seat `m0`'s lane, its
/// value column carrying `token`.
struct RewriteVerdictLedger {
    token: Option<String>,
}

impl crate::capability_rebuild::CapabilityLedgerReader for RewriteVerdictLedger {
    fn tombstone(&self) -> Option<crate::capability_rebuild::ReplayTombstone> {
        Some(crate::capability_rebuild::ReplayTombstone::new(0, 0, 0))
    }

    fn read_events(&self) -> Vec<crate::capability_rebuild::CapabilityEventRow> {
        let capability = crate::field_capability::field_capability_key(
            super::super::field_repair::TEST_REWRITE_PATH,
        )
        .expect("the test path is a qualified path");
        vec![
            crate::capability_rebuild::CapabilityEventRow::new(
                1,
                Instant::now(),
                "broken".to_string(),
                Some("f1".to_string()),
                "live".to_string(),
                Some("self-identifying".to_string()),
                None,
                capability,
                crate::router::probe_test_support::seat_chain_lane("m0")
                    .as_lane_key()
                    .to_string(),
                ANTHROPIC.to_string(),
                0,
                0,
            )
            .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
            .with_upstream_token(self.token.clone()),
        ]
    }
}

/// A rewrite verdict replayed from a `shape:<member>` row is pre-flight
/// eligible on its own (the rebuild seeds its confirmation) and is applied
/// with that member, on every request -- including the one a drop verdict
/// would have spent as its canary.
///
/// Mutation checks: make the rebuild seed no value
/// (`seed_value_from_rebuild(.., None)`) -> red (`no_stored_value`); make the
/// planner drop instead of rewriting -> red (the field is gone).
#[test]
fn a_rewrite_verdict_restored_from_the_ledger_is_applied_with_its_value() {
    let member = super::super::field_repair::TEST_REWRITE_ACCEPTED[1];
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let summary = router.rebuild_learned_from_ledger(&RewriteVerdictLedger {
        token: Some(format!("shape:{member}")),
    });
    assert_eq!(
        summary.replayed_negative, 1,
        "premise: the verdict replayed"
    );
    let req = req_rewrite_only(ALIAS);

    for _ in 0..=CANARY_INTERVAL {
        let (planned, decision) = plan(&router, &req, "m0");

        assert!(decision.acted, "the restored rewrite verdict is applied");
        assert_eq!(decision.reason, super::FIELD_PREFLIGHT_ACTION_REWRITE);
        assert_eq!(
            planned.chat_template_kwargs,
            Some(json!({ super::super::field_repair::TEST_REWRITE_KWARG: member })),
            "the field is rewritten to the stored member, not dropped",
        );
    }
}

/// A row whose value token the strict decode refuses restores a plain negative
/// with no value: authorized, but never applied as a rewrite.
///
/// Mutation check: make replay's `decode_row_value` skip the strict decode for
/// a shape slot (any token restores the row's first member) -> red.
#[test]
fn a_rewrite_verdict_with_a_refused_value_token_is_never_applied() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let summary = router.rebuild_learned_from_ledger(&RewriteVerdictLedger {
        token: Some("shape:routectl-test-not-a-member".to_string()),
    });
    assert_eq!(
        summary.replayed_negative, 1,
        "premise: the verdict replayed"
    );
    assert_eq!(summary.degraded_value, 1, "the refused token is counted");
    let req = req_rewrite_only(ALIAS);

    let (planned, decision) = plan(&router, &req, "m0");

    assert!(
        !decision.acted,
        "a verdict without a value is never applied"
    );
    assert_eq!(decision.reason, super::FIELD_PREFLIGHT_NO_STORED_VALUE);
    assert_eq!(planned.chat_template_kwargs, req.chat_template_kwargs);
}

/// A request already sending the stored value has nothing to rewrite.
#[test]
fn a_request_already_sending_the_stored_value_is_left_alone() {
    let member = super::super::field_repair::TEST_REWRITE_ACCEPTED[0];
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let _ = router.rebuild_learned_from_ledger(&RewriteVerdictLedger {
        token: Some(format!("shape:{member}")),
    });
    let mut req = req_rewrite_only(ALIAS);
    req.chat_template_kwargs =
        Some(json!({ super::super::field_repair::TEST_REWRITE_KWARG: member }));

    let (planned, decision) = plan(&router, &req, "m0");

    assert!(!decision.acted);
    assert_eq!(decision.reason, super::FIELD_PREFLIGHT_VALUE_ALREADY_SENT);
    assert_eq!(planned.chat_template_kwargs, req.chat_template_kwargs);
}
