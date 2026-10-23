// Ranked effort repair through the dispatch walk on anthropic-api where the
// level being refused is not this request's to learn, or was written by an
// edit routectl declared: a replacement level a sibling request holds is
// repaired and commits nothing; a level the pre-flight wrote from a learned
// verdict is repaired in-request, and once that level is learned the
// pre-flight follows it; and a replacement refused where the
// egress's injection and the repair both declared the effort is repaired
// again. A non-ranked row's pre-flight value refused stays terminal.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain` module, sharing its seats, rejections, readers and the
// effort helpers of `field_repair_chain_effort_tests.rs`.

/// Refuse `xhigh` with the captured list, then `high` listing `low, medium`,
/// else serve.
fn refuse_xhigh_then_high(body: &serde_json::Value, _: usize) -> Option<Error> {
    match sent_effort(body) {
        Some("xhigh") => Some(xhigh_refused()),
        Some("high") => Some(high_refused()),
        _ => None,
    }
}

/// A sibling request holding the replacement level's claim does not stop
/// the chain: `xhigh` -> `high` (claimed by the sibling) -> `medium`, served.
/// Only the `xhigh` step learns; the `high` step is recorded as repaired and
/// not learned, and the sibling still holds `high`'s claim afterwards.
///
/// Mutation check: drop the `Claim::Unheld` fallback in `claim` -> red (the
/// `high` refusal reaches the client). The step's learning bit is pinned by
/// the step tests: a step holding no claim has nothing to commit through.
#[tokio::test]
async fn a_replacement_level_a_sibling_holds_is_repaired_and_commits_nothing() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_xhigh_then_high);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || Some(json!({"type": "adaptive"}));
    let sibling_req = effort_req("high", adaptive());
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
    let holds_high = |plan: &Option<crate::router::field_repair::FieldRepairPlan<'_>>| {
        plan.as_ref()
            .is_some_and(|plan| plan.candidate_paths().contains(&HIGH_PATH))
    };
    assert!(holds_high(&sibling), "premise: the sibling claims high");

    let dispatched = router
        .complete_with_options(effort_req("xhigh", adaptive()), RouterOptions::new())
        .await;

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
    assert_eq!(
        sent,
        ["xhigh", "high", "medium"].map(|level| Some(level.to_string()))
    );
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("medium")))],
    );
    assert_eq!(
        succeeded(&dispatched.meta),
        vec![(XHIGH_PATH, true, true), (HIGH_PATH, true, false)],
    );
    assert!(holds_high(&sibling), "the sibling's claim is untouched");
    let after = router.plan_field_carry(
        &sibling_target,
        &sibling_req,
        crate::router::field_repair::FieldSettlementMode::Settling,
        std::time::Instant::now(),
    );
    assert!(
        !holds_high(&after),
        "a second sibling is still refused high's claim"
    );
}

/// Refuse `xhigh` with the captured list on every request, and `high`
/// listing `low, medium` from position 2 on, which is the second request.
fn refuse_xhigh_then_high_from_the_second_request(
    body: &serde_json::Value,
    position: usize,
) -> Option<Error> {
    match sent_effort(body) {
        Some("xhigh") => Some(xhigh_refused()),
        Some("high") if position >= 2 => Some(high_refused()),
        _ => None,
    }
}

/// A level the pre-flight wrote from a learned verdict and the upstream now
/// refuses is repaired in-request: with `xhigh -> high` resident, the next
/// `xhigh` request goes out at `high`, which is refused listing `low,
/// medium`, and is retried at `medium` and served. The step is keyed on the
/// refused `high` and learns `medium`; the resident `xhigh` verdict stands,
/// and the request after it is pre-flighted through both learned levels
/// straight to `medium`, served once with no repair.
///
/// Mutation checks: drop `EditSite::Preflight` from `RANKED_EFFORT_SITES` ->
/// red (the second request's refusal reaches the client); take only the
/// first hop in `follow_effort_chain` -> red (the third request goes out at
/// `high` and is refused).
#[tokio::test]
async fn a_preflight_written_level_refused_with_a_list_is_repaired_in_request() {
    let seat = ChainSeat::new(
        Egress::Anthropic,
        refuse_xhigh_then_high_from_the_second_request,
    );
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || effort_req("xhigh", Some(json!({"type": "adaptive"})));
    let taught = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;
    assert_eq!(
        verdicts(&taught.meta),
        vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("high")))],
        "premise: xhigh learns high",
    );
    acknowledge(&router, &taught);

    let dispatched = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let bodies = seat.bodies();
    let sent: Vec<_> = bodies[2..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["high", "medium"].map(|level| Some(level.to_string())),
        "the pre-flight wrote high, the repair medium",
    );
    assert!(
        dispatched
            .meta
            .field_preflight
            .iter()
            .any(|record| record.acted && record.field_path == Some(XHIGH_PATH)),
        "premise: the learned level was applied before dispatch",
    );
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(HIGH_PATH), Some(VerdictValue::Shape("medium")))],
    );
    assert_eq!(succeeded(&dispatched.meta), vec![(HIGH_PATH, true, true)]);
    acknowledge(&router, &dispatched);
    let before_third = seat.calls();
    let third = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;
    assert!(third.result.is_ok(), "{:?}", third.result.as_ref().err());
    let third_sent: Vec<_> = seat.bodies()[before_third..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        third_sent,
        [Some("medium".to_string())],
        "one attempt, pre-flighted through both learned levels",
    );
    let acted: Vec<_> = third
        .meta
        .field_preflight
        .iter()
        .filter(|record| record.acted)
        .map(|record| record.field_path)
        .collect();
    assert_eq!(acted, [Some(XHIGH_PATH), Some(HIGH_PATH)]);
    assert!(third.meta.field_repairs.is_empty());
    assert!(verdicts(&third.meta).is_empty(), "nothing new is learned");
}

/// Refuse disabled thinking, then a `max_tokens` above the model limit, then
/// from position 3 on, which is the second request, one above the lower
/// limit; else serve.
fn ceiling_then_lower_from_the_second_request(
    body: &serde_json::Value,
    position: usize,
) -> Option<Error> {
    if thinking_type(body) == Some("disabled") {
        return Some(anthropic_disabled_rejection());
    }
    let sent = sent_max_tokens(body)?;
    if sent > u64::from(MODEL_LIMIT) {
        return Some(max_tokens_rejection());
    }
    (position >= 3 && sent > u64::from(LOWER_LIMIT)).then(lower_limit_refused)
}

/// A row that is not ranked keeps a refused pre-flight value terminal: a
/// learned ceiling clamps the next request, the upstream refuses that
/// ceiling for a lower one, and the refusal reaches the client with no
/// retry and nothing learned.
///
/// Characterization: the acting ceiling verdict also keeps its row out of
/// the request's admission, so the declared-edit gate is not the only thing
/// standing between this refusal and a repair.
#[tokio::test]
async fn a_refused_preflight_value_on_a_row_that_is_not_ranked_reaches_the_client() {
    let seat = ChainSeat::new(
        Egress::Anthropic,
        ceiling_then_lower_from_the_second_request,
    );
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let taught = router
        .complete_with_options(two_key_req(), RouterOptions::new())
        .await;
    assert!(taught.result.is_ok(), "premise: both fields are learned");
    acknowledge(&router, &taught);

    let dispatched = router
        .complete_with_options(two_key_req(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 4, "the second request is sent once");
    assert_eq!(
        sent_max_tokens(&bodies[3]),
        Some(u64::from(MODEL_LIMIT)),
        "premise: the pre-flight clamped to the learned ceiling",
    );
    assert_eq!(
        dispatched.meta.wire_reverts.len(),
        1,
        "premise: a declared edit explains the refusal",
    );
    assert!(dispatched.meta.field_repairs.is_empty());
    assert!(dispatched.meta.learned_capabilities.is_empty());
}

/// Refuse disabled thinking, then a `max_tokens` above the limit, then `low`
/// listing `high, medium`, then `medium` listing `high, low`; else serve.
fn disabled_ceiling_then_low_then_medium(body: &serde_json::Value, _: usize) -> Option<Error> {
    if thinking_type(body) == Some("disabled") {
        return Some(anthropic_disabled_rejection());
    }
    if sent_max_tokens(body).is_some_and(|sent| sent > u64::from(MODEL_LIMIT)) {
        return Some(max_tokens_rejection());
    }
    match sent_effort(body) {
        Some("low") => Some(low_refused()),
        Some("medium") => Some(medium_refused()),
        _ => None,
    }
}

/// A refusal of the replacement of an injected effort is repaired again in
/// both client shapes: `low` (injected) -> `medium` -> `high`, served. With a
/// client `output_config` the egress's injection and the first repair both
/// declare `/output_config/effort`, so the second refusal matches two
/// declared edits; with none the injection is declared at `/output_config`
/// and the refusal matches the repair alone.
///
/// Mutation check: answer only a single matched edit in `made_only_at`
/// (`Conflict` -> false) -> red on `client output_config` (the `medium`
/// refusal reaches the client); `no client output_config` stays green.
#[tokio::test]
async fn a_refused_replacement_of_an_injected_effort_is_repaired_again() {
    let rows = [
        ("no client output_config", None),
        ("client output_config", Some(format_config())),
    ];

    for (name, output_config) in rows {
        let seat = ChainSeat::new(Egress::Anthropic, disabled_ceiling_then_low_then_medium);
        let router = anthropic_router(&seat, &wire_checked(THREE_ATTEMPTS));
        let taught = router
            .complete_with_options(disabled_req(Some("high")), RouterOptions::new())
            .await;
        assert!(
            taught.result.is_ok(),
            "{name}: premise: the shape is learned"
        );
        acknowledge(&router, &taught);
        let taught_calls = seat.calls();

        let dispatched = router
            .complete_with_options(injected_effort_req(output_config), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.as_ref().err()
        );
        let sent: Vec<_> = seat.bodies()[taught_calls..]
            .iter()
            .map(|b| sent_effort(b).map(str::to_string))
            .collect();
        assert_eq!(
            sent,
            ["low", "low", "medium", "high"].map(|level| Some(level.to_string())),
            "{name}: the ceiling, then low and medium refused, then high served",
        );
        let high = Some(VerdictValue::Shape("high"));
        assert_eq!(
            verdicts(&dispatched.meta),
            vec![
                (
                    key(MAX_TOKENS_PATH),
                    Some(VerdictValue::Ceiling(MODEL_LIMIT))
                ),
                (key(LOW_PATH), high),
                (key(MEDIUM_PATH), high),
            ],
            "{name}",
        );
    }
}
