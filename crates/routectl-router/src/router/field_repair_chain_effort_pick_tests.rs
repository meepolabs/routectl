// Ranked effort repair through the dispatch walk on anthropic-api, as the
// operator reads it: each step's record and WARN name the refused level, the
// level written and which way the pick moved, all rank tokens; a pick above
// the refused level is recorded under its own action token and counted lossy
// once on the lane.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain` module, sharing its seats, rejections, readers and the
// effort helpers of `field_repair_chain_effort_tests.rs`.

/// Refuse a `low` effort listing only `high, medium`, else serve.
fn refuse_low(body: &serde_json::Value, _: usize) -> Option<Error> {
    (sent_effort(body) == Some("low")).then(low_refused)
}

/// `(action, refused, chosen, direction)` of each repair record.
type PickRecord = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

fn pick_records(meta: &crate::router::DispatchMeta) -> Vec<PickRecord> {
    meta.field_repairs
        .iter()
        .map(|r| (r.action, r.refused_level, r.chosen_level, r.direction))
        .collect()
}

/// Dispatch `req` on a fresh current-thread runtime inside an event capture,
/// returning the dispatch and the field-repair WARNs it emitted.
fn dispatch_capturing_warns(
    router: &Router,
    req: ChatRequest,
) -> (
    crate::router::Dispatched,
    Vec<routectl_testkit::CapturedEvent>,
) {
    let mut dispatched = None;
    let events = routectl_testkit::capture_events(|| {
        dispatched = Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime with timers")
                .block_on(router.complete_with_options(req, RouterOptions::new())),
        );
    });
    let warns = events
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
        .collect();
    (dispatched.expect("the dispatch ran"), warns)
}

/// A refused `low` whose list holds only levels above it is served at
/// `medium`: the step records the higher-pick action with `low`, `medium`
/// and `higher`, the WARN carries the same tokens, and the lane's lossy
/// tally counts it once.
///
/// Mutation checks: record a higher pick under the plain rewrite token in
/// `repair_action` -> red on the action; drop the higher-pick token from
/// `repair_action_is_lossy` -> red on the lane tally.
#[test]
fn a_higher_pick_records_its_own_action_and_counts_lossy_once_on_the_lane() {
    use crate::wire_check_state::FieldRepairOutcome;

    let seat = ChainSeat::new(Egress::Anthropic, refuse_low);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let (dispatched, warns) = dispatch_capturing_warns(
        &router,
        effort_req("low", Some(json!({"type": "adaptive"}))),
    );

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let sent: Vec<_> = seat
        .bodies()
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(sent, ["low", "medium"].map(|level| Some(level.to_string())));
    assert_eq!(
        pick_records(&dispatched.meta),
        vec![(
            "field_rewrite_higher_repair",
            Some("low"),
            Some("medium"),
            Some("higher"),
        )],
    );
    let lane = dispatched.meta.field_repairs[0].lane.clone();
    assert_ne!(lane, "other", "premise: the target has a learned lane");
    let lanes: Vec<_> = router
        .lossy_repairs_by_lane()
        .into_iter()
        .map(|row| (row.lane, row.lossy))
        .collect();
    assert_eq!(lanes, vec![(lane, 1)], "one lossy repair, on the lane");
    assert_eq!(
        effort_rejection_outcomes(&router),
        vec![FieldRepairOutcome::RepairedLossy],
    );
    assert_eq!(warns.len(), 1, "{warns:?}");
    let warn = &warns[0];
    assert_eq!(warn.field("action"), Some("field_rewrite_higher_repair"));
    assert_eq!(warn.field("refused_level"), Some("low"));
    assert_eq!(warn.field("chosen_level"), Some("medium"));
    assert_eq!(warn.field("direction"), Some("higher"));
}

/// A refused `xhigh` served at the lower `high` keeps the rewrite action and
/// carries `lower`; its WARN names the two levels by their rank tokens, and
/// no field of it carries the refusal's text.
///
/// Mutation check: answer `Higher` for every pick in `ranked_pick` -> red.
#[test]
fn a_lower_pick_keeps_the_rewrite_action_and_carries_direction_lower() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_xhigh);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let (dispatched, warns) = dispatch_capturing_warns(
        &router,
        effort_req("xhigh", Some(json!({"type": "adaptive"}))),
    );

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    assert_eq!(
        pick_records(&dispatched.meta),
        vec![(
            "field_rewrite_repair",
            Some("xhigh"),
            Some("high"),
            Some("lower"),
        )],
    );
    assert_eq!(router.field_repair_counters().repair_lossy, 1);
    assert_eq!(warns.len(), 1, "{warns:?}");
    let warn = &warns[0];
    assert_eq!(warn.field("refused_level"), Some("xhigh"));
    assert_eq!(warn.field("chosen_level"), Some("high"));
    assert_eq!(warn.field("direction"), Some("lower"));
    for (name, value) in &warn.fields {
        assert!(
            !value.contains("Supported levels") && !value.contains("does not support"),
            "{name} carries upstream text: {value}",
        );
    }
}

/// A step on a replacement level a sibling request holds commits nothing, and
/// still records its pick: `xhigh` -> `high` (lower), then `high`, claimed by
/// the sibling, -> `medium` (lower).
#[test]
fn an_unheld_replacement_step_records_its_pick_like_a_held_one() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_xhigh_then_high);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let sibling_req = effort_req("high", Some(json!({"type": "adaptive"})));
    let sibling_target = router
        .dispatch_chain_for_request(
            &sibling_req,
            crate::router::class_observe::DispatchSurface::Complete,
        )
        .expect("the fixture chain resolves")
        .0
        .into_iter()
        .next()
        .expect("one target");
    let sibling = router.plan_field_carry(
        &sibling_target,
        &sibling_req,
        crate::router::field_repair::FieldSettlementMode::Settling,
        std::time::Instant::now(),
    );
    assert!(
        sibling
            .as_ref()
            .is_some_and(|plan| plan.candidate_paths().contains(&HIGH_PATH)),
        "premise: the sibling claims high",
    );

    let (dispatched, _) = dispatch_capturing_warns(
        &router,
        effort_req("xhigh", Some(json!({"type": "adaptive"}))),
    );

    assert_eq!(
        succeeded(&dispatched.meta),
        vec![(XHIGH_PATH, true, true), (HIGH_PATH, true, false)],
        "premise: the second step is unheld and learns nothing",
    );
    assert_eq!(
        pick_records(&dispatched.meta),
        vec![
            (
                "field_rewrite_repair",
                Some("xhigh"),
                Some("high"),
                Some("lower"),
            ),
            (
                "field_rewrite_repair",
                Some("high"),
                Some("medium"),
                Some("lower"),
            ),
        ],
    );
    drop(sibling);
}
