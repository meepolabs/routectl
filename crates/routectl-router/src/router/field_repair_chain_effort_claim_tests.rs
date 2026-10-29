// Ranked effort repair through the dispatch walk on anthropic-api where the
// level being refused is not this request's to learn, or was written by an
// edit routectl declared: a replacement level a sibling request holds is
// repaired and commits nothing; a level the pre-flight wrote from a learned
// verdict is repaired in-request, and once that level is learned the
// pre-flight follows it; and a replacement refused where the
// egress's injection and the repair both declared the effort is repaired
// again. A non-ranked row's pre-flight value refused stays terminal. A
// level the upstream accepts clears that level's own verdict, so two
// requests teaching opposite levels leave only the later verdict.
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

// -----------------------------------------------------------------------
// An accepted level clears its own verdict
// -----------------------------------------------------------------------

/// The same template refusing `high`, listing only higher levels, so the
/// repair picks the nearest higher one. Synthesized from the captured
/// template.
fn high_refused_listing_higher() -> Error {
    anthropic_rejection(
        "This model does not support effort level 'high'. Supported levels: max, xhigh.",
    )
}

/// A flapping upstream: refuse `xhigh` with the captured list at position 0
/// only, which is the first request, and `high` listing `max, xhigh` from
/// position 2 on; else serve.
fn refuse_xhigh_once_then_high_listing_higher(
    body: &serde_json::Value,
    position: usize,
) -> Option<Error> {
    match sent_effort(body) {
        Some("xhigh") if position == 0 => Some(xhigh_refused()),
        Some("high") if position >= 2 => Some(high_refused_listing_higher()),
        _ => None,
    }
}

/// The capability keys of every effort verdict resident on `router`.
fn resident_effort_keys(router: &Router) -> Vec<String> {
    let mut keys: Vec<_> = router
        .learned_capability_snapshot()
        .into_iter()
        .map(|entry| entry.feature_key)
        .filter(|feature| feature.contains("output_config.effort."))
        .collect();
    keys.sort();
    keys
}

fn cleared_keys(meta: &crate::router::DispatchMeta) -> Vec<String> {
    meta.cleared_capabilities
        .iter()
        .map(|event| event.capability_key.clone())
        .collect()
}

/// `xhigh` sent through the walk at `adaptive`; the request must succeed
/// with exactly one attempt at `xhigh` and no repair.
async fn assert_xhigh_goes_out_as_sent(router: &Router, seat: &ChainSeat) {
    let before = seat.calls();
    let third = router
        .complete_with_options(
            effort_req("xhigh", Some(json!({"type": "adaptive"}))),
            RouterOptions::new(),
        )
        .await;
    assert!(third.result.is_ok(), "{:?}", third.result.as_ref().err());
    let sent: Vec<_> = seat.bodies()[before..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(sent, [Some("xhigh".to_string())], "one attempt, as sent");
    assert!(third.meta.field_repairs.is_empty());
    assert!(
        third
            .meta
            .field_preflight
            .iter()
            .all(|record| !record.acted),
        "no learned level is applied",
    );
}

/// One request on the complete walk, or on the stream walk drained to its
/// end: its meta and whether it was served.
async fn dispatch_on(
    router: &Router,
    req: ChatRequest,
    stream: bool,
) -> (crate::router::DispatchMeta, bool) {
    if !stream {
        let dispatched = router
            .complete_with_options(req, RouterOptions::new())
            .await;
        return (dispatched.meta, dispatched.result.is_ok());
    }
    let opened = router.stream_with_options(req, RouterOptions::new()).await;
    let served = match opened.result {
        Ok(body) => body.collect::<Vec<_>>().await.iter().all(Result::is_ok),
        Err(_) => false,
    };
    (opened.meta, served)
}

/// Two requests teach opposite levels on one lane and the store keeps only
/// the later: A's `xhigh` is refused and learns `xhigh -> high`; B sends
/// `high`, refused listing only `max, xhigh`, is retried at `xhigh` and
/// served (on the complete walk, or on the stream walk at first content),
/// which learns `high -> xhigh` and, since `xhigh` was just accepted,
/// clears A's `xhigh` verdict in the same settlement. A third `xhigh`
/// request then goes out at `xhigh`, once, and is served.
async fn assert_a_later_acceptance_clears_the_refusing_verdict(stream: bool) {
    let seat = ChainSeat::new(
        Egress::Anthropic,
        refuse_xhigh_once_then_high_listing_higher,
    );
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || Some(json!({"type": "adaptive"}));
    let taught = router
        .complete_with_options(effort_req("xhigh", adaptive()), RouterOptions::new())
        .await;
    assert_eq!(
        verdicts(&taught.meta),
        vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("high")))],
        "premise: xhigh learns high",
    );
    acknowledge(&router, &taught);

    let (meta, served) = dispatch_on(&router, effort_req("high", adaptive()), stream).await;

    assert!(served, "stream {stream}");
    let sent: Vec<_> = seat.bodies()[2..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["high", "xhigh"].map(|level| Some(level.to_string())),
        "premise: high refused, xhigh served",
    );
    assert_eq!(
        verdicts(&meta),
        vec![(key(HIGH_PATH), Some(VerdictValue::Shape("xhigh")))],
    );
    assert_eq!(cleared_keys(&meta), [key(XHIGH_PATH)], "stream {stream}");
    assert_eq!(resident_effort_keys(&router), [key(HIGH_PATH)]);
    acknowledge_meta(&router, &meta);

    assert_xhigh_goes_out_as_sent(&router, &seat).await;
}

/// [`assert_a_later_acceptance_clears_the_refusing_verdict`] on the complete
/// walk.
///
/// Mutation checks: drop the accepted-effort clear in
/// `settle_field_success` -> red (the third request is pre-flighted to
/// `high`, stops at the repeat, and the refusal reaches the client); key the
/// clear on the client's level, which is the level B's step refused, -> red
/// (`high` is cleared instead of `xhigh`).
#[tokio::test]
async fn a_level_accepted_in_a_later_request_clears_the_verdict_that_refused_it() {
    assert_a_later_acceptance_clears_the_refusing_verdict(false).await;
}

/// The same on the stream walk, which settles at first content.
///
/// Mutation check: pass the client's request as the accepted body at the
/// stream walk's `on_success` -> red (`high` is cleared instead of `xhigh`)
/// while the complete row stays green.
#[tokio::test]
async fn a_level_accepted_at_first_content_clears_the_verdict_that_refused_it() {
    assert_a_later_acceptance_clears_the_refusing_verdict(true).await;
}

/// A token count proves nothing about whether a completion accepts a level:
/// with a `high -> medium` verdict resident (learned, not yet acknowledged,
/// so it drives no pre-flight), a settling count of a `high` request reaches
/// the upstream at `high` and is counted, and the verdict stays.
///
/// Mutation check: run the accepted-effort clear on the count surface -> red
/// (`high` is cleared).
#[tokio::test]
async fn a_token_count_at_a_level_keeps_that_levels_verdict() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_high_once_and_every_xhigh);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || Some(json!({"type": "adaptive"}));
    let taught = router
        .complete_with_options(effort_req("high", adaptive()), RouterOptions::new())
        .await;
    assert_eq!(
        verdicts(&taught.meta),
        vec![(key(HIGH_PATH), Some(VerdictValue::Shape("medium")))],
        "premise: high learns medium",
    );

    let counted = router
        .count_tokens_with_meta(effort_req("high", adaptive()))
        .await;

    assert!(counted.result.is_ok(), "{:?}", counted.result.err());
    assert_eq!(
        seat.bodies()[2..]
            .iter()
            .map(|b| sent_effort(b).map(str::to_string))
            .collect::<Vec<_>>(),
        [Some("high".to_string())],
        "premise: one count at high",
    );
    assert!(cleared_keys(&counted.meta).is_empty());
    assert_eq!(resident_effort_keys(&router), [key(HIGH_PATH)]);
}

/// The inverse of a level the pre-flight wrote: with `xhigh -> high`
/// resident, the next `xhigh` request goes out at `high`, which is refused
/// listing only `max, xhigh`, and is retried at `xhigh` and served. The step
/// learns `high -> xhigh` and the accepted `xhigh` clears the verdict that
/// wrote `high`, so a third `xhigh` request goes out as sent.
///
/// Mutation check: drop the accepted-effort clear in `settle_field_success`
/// -> red (both verdicts stay resident and the third request is refused).
#[tokio::test]
async fn a_preflight_written_level_repaired_back_to_the_original_clears_the_original_verdict() {
    let seat = ChainSeat::new(
        Egress::Anthropic,
        refuse_xhigh_once_then_high_listing_higher,
    );
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || effort_req("xhigh", Some(json!({"type": "adaptive"})));
    let taught = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;
    acknowledge(&router, &taught);

    let dispatched = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let sent: Vec<_> = seat.bodies()[2..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["high", "xhigh"].map(|level| Some(level.to_string())),
        "the pre-flight wrote high, the repair xhigh",
    );
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(HIGH_PATH), Some(VerdictValue::Shape("xhigh")))],
    );
    assert_eq!(cleared_keys(&dispatched.meta), [key(XHIGH_PATH)]);
    assert_eq!(resident_effort_keys(&router), [key(HIGH_PATH)]);
    acknowledge(&router, &dispatched);

    assert_xhigh_goes_out_as_sent(&router, &seat).await;
}

/// Refuse `high` listing `low, medium` at position 0 only and every `xhigh`
/// with the captured list; else serve.
fn refuse_high_once_and_every_xhigh(body: &serde_json::Value, position: usize) -> Option<Error> {
    match sent_effort(body) {
        Some("high") if position == 0 => Some(high_refused()),
        Some("xhigh") => Some(xhigh_refused()),
        _ => None,
    }
}

/// Refuse `high` listing `low, medium` at position 2 only, which is the
/// second request, and every `xhigh` with the captured list; else serve.
fn refuse_every_xhigh_and_high_once_later(
    body: &serde_json::Value,
    position: usize,
) -> Option<Error> {
    match sent_effort(body) {
        Some("high") if position == 2 => Some(high_refused()),
        Some("xhigh") => Some(xhigh_refused()),
        _ => None,
    }
}

/// An UNREPAIRED success at a level clears that level's verdict even when
/// the verdict kept its row out of the request's admission: a `high ->
/// medium` verdict, learned and not yet acknowledged, is resident and
/// acting but cannot drive pre-flight, so the next request reaches the
/// upstream at `high`; it is served, and the `high` verdict clears. One
/// row per way `high` reaches the wire: sent by the client, and written by
/// the pre-flight from an acknowledged `xhigh -> high` learned first, which
/// stays.
///
/// Mutation check: drop the accepted-effort clear in `settle_field_success`
/// -> red on both rows (nothing is cleared: the acting verdict admitted no
/// candidate to clear through).
#[tokio::test]
async fn an_unrepaired_success_at_a_level_clears_that_levels_verdict() {
    let adaptive = || Some(json!({"type": "adaptive"}));
    /// `(name, seat, levels taught in order, client level, verdicts left)`.
    type Row = (
        &'static str,
        Respond,
        &'static [&'static str],
        &'static str,
        &'static [&'static str],
    );
    let rows: [Row; 2] = [
        (
            "client sent high",
            refuse_high_once_and_every_xhigh,
            &["high"],
            "high",
            &[],
        ),
        (
            "pre-flight wrote high",
            refuse_every_xhigh_and_high_once_later,
            &["xhigh", "high"],
            "xhigh",
            &[XHIGH_PATH],
        ),
    ];

    for (name, respond, teach, client, stays) in rows {
        let seat = ChainSeat::new(Egress::Anthropic, respond);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
        let mut learned = Vec::new();
        for level in teach {
            let taught = router
                .complete_with_options(effort_req(level, adaptive()), RouterOptions::new())
                .await;
            if *level == "xhigh" {
                acknowledge(&router, &taught);
            }
            learned.extend(verdicts(&taught.meta));
        }
        assert_eq!(
            learned.last(),
            Some(&(key(HIGH_PATH), Some(VerdictValue::Shape("medium")))),
            "{name}: premise: high learns medium last, not acknowledged",
        );
        let before = seat.calls();

        let served = router
            .complete_with_options(effort_req(client, adaptive()), RouterOptions::new())
            .await;

        assert!(
            served.result.is_ok(),
            "{name}: {:?}",
            served.result.as_ref().err()
        );
        assert_eq!(
            seat.bodies()[before..]
                .iter()
                .map(|b| sent_effort(b).map(str::to_string))
                .collect::<Vec<_>>(),
            [Some("high".to_string())],
            "{name}: one attempt at high",
        );
        assert!(served.meta.field_repairs.is_empty(), "{name}");
        assert_eq!(cleared_keys(&served.meta), [key(HIGH_PATH)], "{name}");
        let stays: Vec<_> = stays.iter().map(|path| key(path)).collect();
        assert_eq!(resident_effort_keys(&router), stays, "{name}");
    }
}

/// The incarnation the learned registry hands its next mutation, read by
/// removing `capability` (resident nowhere) on `lane`; the read itself
/// consumes one.
fn next_incarnation(router: &Router, lane: &str, capability: &str) -> u64 {
    match router.learned_capabilities.remove_keyed_in_generation(
        router.registry_generation(),
        lane,
        capability,
        ANTHROPIC_API_KIND,
    ) {
        crate::learned_capability::GenerationOutcome::Applied {
            value: false,
            incarnation,
            ..
        } => incarnation,
        _ => panic!("premise: the sentinel key holds nothing"),
    }
}

/// An unrepaired success whose admitted effort candidate held a resident
/// verdict removes that key once: the guard's clear removes the lapsed
/// `high -> medium` verdict, and the accepted-level clear of the same `high`
/// does not take the registry lock to remove it again. The request makes
/// four registry mutations: the lapsed verdict's re-probe settles, and each
/// admitted row (`thinking.type.adaptive`, `max_tokens`, `high`) runs its
/// guard's clear.
///
/// Mutation check: hand `clear_accepted_effort` no removed keys in
/// `settle_field_success` -> red (three mutations).
#[tokio::test]
async fn an_unrepaired_success_removes_its_admitted_effort_verdict_once() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_high_once_and_every_xhigh);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || Some(json!({"type": "adaptive"}));
    let taught = router
        .complete_with_options(effort_req("high", adaptive()), RouterOptions::new())
        .await;
    let [event] = taught.meta.learned_capabilities.as_slice() else {
        panic!("premise: high learns one verdict");
    };
    assert_eq!(event.capability_key, key(HIGH_PATH), "premise");
    let lane = event.state_key.clone();
    assert!(
        matches!(
            router.learned_capabilities.expire_keyed_in_generation(
                router.registry_generation(),
                &lane,
                &event.capability_key,
                ANTHROPIC_API_KIND,
                std::time::Instant::now(),
            ),
            crate::learned_capability::GenerationOutcome::Applied { value: true, .. }
        ),
        "premise: the high verdict lapsed",
    );
    let sentinel = key(LOW_PATH);
    let before = next_incarnation(&router, &lane, &sentinel);

    let served = router
        .complete_with_options(effort_req("high", adaptive()), RouterOptions::new())
        .await;

    let after = next_incarnation(&router, &lane, &sentinel);
    assert!(served.result.is_ok(), "{:?}", served.result.as_ref().err());
    assert!(served.meta.field_repairs.is_empty(), "premise: unrepaired");
    assert_eq!(cleared_keys(&served.meta), [key(HIGH_PATH)]);
    assert!(resident_effort_keys(&router).is_empty());
    assert_eq!(after - before - 1, 4, "registry mutations");
}

/// Refuse `high` listing `low, medium` at position 0 only, every `xhigh`
/// with the captured list, and a `high` that carries a stop sequence for
/// the stop sequence; else serve.
fn high_once_xhigh_then_stop(body: &serde_json::Value, position: usize) -> Option<Error> {
    match sent_effort(body) {
        Some("high") if position == 0 => Some(high_refused()),
        Some("xhigh") => Some(xhigh_refused()),
        Some("high") if body.get("stop_sequences").is_some() => Some(stop_refused()),
        _ => None,
    }
}

/// A moved-on commit proves the level it sent: with `high -> medium`
/// resident, an `xhigh` request is refused and retried at `high`, and that
/// retry is refused for its stop sequence. The upstream read past `high`,
/// so the pending `xhigh` step commits `high` and the `high` verdict clears,
/// though the request itself ends refused.
///
/// Mutation check: drop the accepted-effort clear in `settle_field_moved_on`
/// -> red (the `high` verdict stays).
#[tokio::test]
async fn a_moved_on_effort_commit_clears_the_verdict_of_the_level_it_sent() {
    let seat = ChainSeat::new(Egress::Anthropic, high_once_xhigh_then_stop);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || Some(json!({"type": "adaptive"}));
    let taught = router
        .complete_with_options(effort_req("high", adaptive()), RouterOptions::new())
        .await;
    assert_eq!(
        verdicts(&taught.meta),
        vec![(key(HIGH_PATH), Some(VerdictValue::Shape("medium")))],
        "premise: high learns medium",
    );
    let mut req = effort_req("xhigh", adaptive());
    req.stop = Some(vec![STOP.to_string()]);
    if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
        let mut client: serde_json::Value = serde_json::from_slice(&wire.bytes).expect("json");
        client["stop_sequences"] = json!([STOP]);
        wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
    }

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "premise: the stop refusal ends it"
    );
    let sent: Vec<_> = seat.bodies()[2..]
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["xhigh", "high"].map(|level| Some(level.to_string())),
        "premise",
    );
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("high")))],
    );
    assert_eq!(cleared_keys(&dispatched.meta), [key(HIGH_PATH)]);
    assert_eq!(resident_effort_keys(&router), [key(XHIGH_PATH)]);
}
