// Pre-flight over the CEILING row: a stored ceiling lowers a token limit above
// it and leaves every other limit as the client sent it, and is never canaried.
// An `include!`d FRAGMENT of `field_preflight_tests.rs`.

/// The ceiling the planted verdicts store.
const STORED_CEILING: u32 = 8192;

/// The qualified dotted path of the grounded ceiling row.
const CEILING_ROW_PATH: &str = "max_tokens";

/// A request carrying ONLY the ceiling row's field, at `max_tokens`.
fn req_max_tokens_only(alias: &str, max_tokens: u32) -> ChatRequest {
    let mut req = req_without_field(alias);
    req.max_tokens = Some(max_tokens);
    req
}

fn ceiling_verdict_key(state_key: &str) -> FieldVerdictKey {
    FieldVerdictKey::new(
        &crate::router::probe_test_support::seat_chain_lane(state_key),
        CEILING_ROW_PATH,
        ANTHROPIC,
    )
    .expect("a qualified path mints a key")
}

/// Plant a resident ACTING, acknowledged verdict on the ceiling row's identity,
/// storing `value` for its incarnation.
fn plant_ceiling_eligible(router: &Router, state_key: &str, value: Option<crate::VerdictValue>) {
    let lane = crate::router::probe_test_support::seat_chain_lane(state_key);
    let feature_key = crate::field_capability::field_capability_key(CEILING_ROW_PATH)
        .expect("the row path is a qualified path");
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
    let key = ceiling_verdict_key(state_key);
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, 1, false);
    router
        .field_verdicts()
        .seed_value_from_rebuild(&key, incarnation, value);
    assert!(
        router.field_verdicts().preflight_eligible(
            &key,
            router.registry_generation(),
            Instant::now()
        ),
        "fixture premise: the ceiling verdict is pre-flight eligible",
    );
}

/// One row per client limit and verdict state: whether the planner acted, its
/// reason, and the limit the planned request carries.
///
/// Mutation check: make `FieldSurface::clamp_lowers` answer `true` for any
/// nonzero ceiling (dropping `ceiling < emitted`) -> red on the "below" and
/// "equal" rows, which would ship the stored ceiling, raising the client's
/// limit.
#[test]
fn a_stored_ceiling_clamps_only_a_client_limit_above_it() {
    let stored = Some(crate::VerdictValue::Ceiling(STORED_CEILING));
    let rows = [
        (
            "below the ceiling",
            4096,
            Some(stored),
            false,
            super::FIELD_PREFLIGHT_WITHIN_CEILING,
            4096,
        ),
        (
            "at the ceiling",
            STORED_CEILING,
            Some(stored),
            false,
            super::FIELD_PREFLIGHT_WITHIN_CEILING,
            STORED_CEILING,
        ),
        (
            "above, no verdict",
            64_000,
            None,
            false,
            FIELD_PREFLIGHT_NOT_ELIGIBLE,
            64_000,
        ),
        (
            "above, verdict without a value",
            64_000,
            Some(None),
            false,
            super::FIELD_PREFLIGHT_NO_STORED_VALUE,
            64_000,
        ),
        (
            "above, verdict",
            64_000,
            Some(stored),
            true,
            super::FIELD_PREFLIGHT_ACTION_CEILING,
            STORED_CEILING,
        ),
    ];

    for (name, sent, verdict, acted, reason, shipped) in rows {
        let (router, _seen) = single_seat(Answer::ServeImmediately);
        if let Some(value) = verdict {
            plant_ceiling_eligible(&router, "m0", value);
        }
        let req = req_max_tokens_only(ALIAS, sent);

        let (planned, decision) = plan(&router, &req, "m0");

        assert_eq!(decision.acted, acted, "{name}");
        assert_eq!(decision.reason, reason, "{name}");
        assert_eq!(decision.field_path, Some(CEILING_ROW_PATH), "{name}");
        assert_eq!(planned.max_tokens, Some(shipped), "{name}");
        let mut unclamped = planned.clone();
        unclamped.max_tokens = req.max_tokens;
        assert_eq!(
            serde_json::to_value(&unclamped).expect("serialize"),
            serde_json::to_value(&req).expect("serialize"),
            "{name}: nothing but the limit changes",
        );
        assert_eq!(
            router.field_repair_counters().preflight_actions,
            u64::from(acted),
            "{name}",
        );
    }
}

/// A ceiling verdict is applied on every request, including the one a drop
/// verdict would have spent as its canary: a ceiling recovers by decay.
///
/// Mutation check: let the canary gate admit a ceiling edit
/// (`!matches!(edit, PreflightEdit::Rewrite(_))` in place of
/// `edit == PreflightEdit::Drop`) -> red (the request the cadence trips on is
/// restored at the client's limit).
#[test]
fn a_ceiling_verdict_never_claims_the_canary() {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_ceiling_eligible(
        &router,
        "m0",
        Some(crate::VerdictValue::Ceiling(STORED_CEILING)),
    );
    let req = req_max_tokens_only(ALIAS, 64_000);

    for request in 0..=CANARY_INTERVAL {
        let (planned, decision) = plan(&router, &req, "m0");

        assert!(decision.acted, "request {request}");
        assert_eq!(
            decision.reason,
            super::FIELD_PREFLIGHT_ACTION_CEILING,
            "request {request}",
        );
        assert_eq!(
            planned.max_tokens,
            Some(STORED_CEILING),
            "request {request}"
        );
    }
}

/// The two ceiling tokens are spelled as the operator docs say, and the docs
/// list both.
#[test]
fn the_ceiling_tokens_are_documented_with_their_spelling() {
    const LOGGING_DOC: &str = include_str!("../../../../docs/LOGGING.md");
    let rows = [
        (
            super::FIELD_PREFLIGHT_ACTION_CEILING,
            "field_preflight_ceiling",
        ),
        (super::FIELD_PREFLIGHT_WITHIN_CEILING, "within_ceiling"),
    ];

    for (token, spelled) in rows {
        assert_eq!(token, spelled);
        assert!(
            LOGGING_DOC.contains(&format!("| `{spelled}` |")),
            "LOGGING.md must carry a reason row for `{spelled}`",
        );
    }
}
