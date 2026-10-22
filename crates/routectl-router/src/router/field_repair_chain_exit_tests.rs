// The counted exits of chained repair steps and of the request's repair
// allowance: each step a settling plan applied ends committed or dropped, a
// rejection whose proposed repair the allowance could not pay is counted once
// on its lane, and every request lands once in the bucket of the draws it
// spent. Counters live on each router's own metrics, so every case reads a
// fresh router and needs no serial guard.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain` module, sharing its seats and the seat harness of
// `field_repair_tests.rs`.

/// `(committed, dropped)` chain steps on `router`.
fn step_exits(router: &Router) -> (u64, u64) {
    let counters = router.field_repair_counters();
    (counters.chain_steps_committed, counters.chain_steps_dropped)
}

/// `(lane, exhausted)` rows on `router`.
fn exhausted_lanes(router: &Router) -> Vec<(String, u64)> {
    router
        .budget_exhaustions_by_lane()
        .into_iter()
        .map(|row| (row.lane, row.exhausted))
        .collect()
}

/// A histogram with one request in `bucket`.
fn one_request_in(bucket: usize) -> [u64; 6] {
    let mut buckets = [0; 6];
    buckets[bucket] = 1;
    buckets
}

/// Refuse disabled thinking and serve the rewrite.
fn disabled_then_serve(body: &serde_json::Value, _: usize) -> Option<Error> {
    (thinking_type(body) == Some("disabled")).then(anthropic_disabled_rejection)
}

// -----------------------------------------------------------------------
// Committed
// -----------------------------------------------------------------------

/// The rewrite is proven by the refusal of a field the body carried, and
/// that refusal has no row, so the request ends with nothing pending.
///
/// Mutation check: delete the `settle_field_moved_on` call in
/// `on_rejection` -> red (the step is dropped instead of committed).
#[tokio::test]
async fn a_moved_on_commit_counts_its_step_committed() {
    let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
        disabled_then(body, stop_refused)
    });
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "premise: the stop refusal ends it"
    );
    assert_eq!(step_exits(&router), (1, 0));
}

/// Mutation check: skip `commit_field_steps` in `settle_field_success` for
/// an applied plan -> red (the step is dropped instead of committed).
#[tokio::test]
async fn a_success_commit_counts_its_step_committed() {
    let seat = ChainSeat::new(Egress::Anthropic, disabled_then_serve);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(step_exits(&router), (1, 0));
    assert_eq!(router.field_repair_counters().verdicts_learned, 1);
}

/// A refused VALUE repaired by a drop is proven by its success and persists
/// nothing: committed counts the step, verdicts learned does not.
///
/// Mutation check: count a committed step only when its commit persisted a
/// verdict -> red on the committed count.
#[tokio::test]
async fn a_proven_step_that_teaches_nothing_is_committed_without_a_verdict() {
    let (router, _seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let served = router.complete(req_on(ALIAS)).await;

    assert!(served.is_ok(), "{served:?}");
    let counters = router.field_repair_counters();
    assert_eq!(
        counters.repair_lossy, 1,
        "premise: a refused value was dropped"
    );
    assert_eq!(counters.repair_succeeded, 1);
    assert_eq!(
        (counters.chain_steps_committed, counters.verdicts_learned),
        (1, 0),
    );
}

/// A non-settling count proves nothing it can keep, so its steps are
/// neither committed nor dropped.
///
/// Mutation check: give the non-settling plan the exits tally -> red.
#[tokio::test]
async fn a_non_settling_count_counts_no_step_exit() {
    let seat = ChainSeat::new(Egress::Anthropic, two_key);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let counted = router.count_tokens(two_key_req()).await;

    assert!(counted.is_ok(), "{:?}", counted.err());
    assert_eq!(
        router.field_repair_counters().repair_attempted,
        2,
        "premise"
    );
    assert_eq!(step_exits(&router), (0, 0));
}

// -----------------------------------------------------------------------
// Dropped
// -----------------------------------------------------------------------

/// The same field refused again: the step is still pending when the request
/// ends.
///
/// Mutation check: delete the `add_dropped` call in the plan's `Drop` -> red.
#[tokio::test]
async fn a_same_field_refusal_counts_its_step_dropped() {
    let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
        disabled_then(body, adaptive_refused)
    });
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise");
    assert_eq!(step_exits(&router), (0, 1));
}

/// A 503 after the repair moves the walk to the next target, which serves
/// unrepaired: the first target's step is dropped, and the fallback's own
/// unrepaired success commits nothing.
///
/// Mutation check: delete the `add_dropped` call in the plan's `Drop` -> red.
#[tokio::test]
async fn a_moved_target_counts_its_pending_step_dropped() {
    let seat0 = ChainSeat::new(Egress::Anthropic, |body, _| {
        disabled_then(body, unavailable)
    });
    let seat1 = ChainSeat::new(Egress::Anthropic, |_, _| None);
    let router = chain_router(
        &[(ANTHROPIC_API_TOML, &seat0), (ANTHROPIC_API_TOML, &seat1)],
        &wire_checked(ONE_ATTEMPT),
    );

    let dispatched = router
        .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!((seat0.calls(), seat1.calls()), (2, 1), "premise");
    assert_eq!(step_exits(&router), (0, 1));
}

// -----------------------------------------------------------------------
// Exhausted
// -----------------------------------------------------------------------

/// Four repairs on seat 0 and one on seat 1 spend the allowance; seat 1's
/// next refusal proposes a repair the allowance cannot pay. Every applied
/// step is committed or dropped exactly once.
///
/// Mutation check: delete the `incr_repair_budget_exhausted` call in
/// `on_rejection` -> red on the lane row.
#[tokio::test]
async fn an_unpaid_repair_proposal_counts_once_on_its_lane() {
    let seat0 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
        refuse_first_of(&FOUR_KEYS, body).or_else(|| Some(unavailable()))
    });
    let seat1 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
        refuse_first_of(&FOUR_KEYS, body)
    });
    let router = chain_router(
        &[(OPENAI_COMPAT_TOML, &seat0), (OPENAI_COMPAT_TOML, &seat1)],
        &wire_checked(ONE_ATTEMPT),
    );

    let dispatched = router
        .complete_with_options(compat_req(&FOUR_KEYS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise");
    assert_eq!(seat1.calls(), 2, "premise: one repair, then no allowance");
    assert_eq!(
        exhausted_lanes(&router),
        vec![(format!("p1#{UNPROFILED_MODEL}"), 1)],
    );
    assert_eq!(router.field_repair_counters().budget_exhausted, 1);
    assert_eq!(
        step_exits(&router),
        (4, 1),
        "three committed and one dropped on seat 0, one committed on seat 1",
    );
}

/// Once the allowance is spent, a refusal no repair proposes for is not
/// exhaustion.
///
/// Mutation check: count every rejection met with an empty allowance in
/// `on_rejection` -> red.
#[tokio::test]
async fn a_plain_refusal_on_an_empty_allowance_is_not_exhaustion() {
    let seat0 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
        refuse_first_of(&FOUR_KEYS, body).or_else(|| Some(unavailable()))
    });
    let seat1 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
        Some(if body.get("temperature").is_some() {
            compat_rejection("temperature")
        } else {
            unlocalized_rejection()
        })
    });
    let router = chain_router(
        &[(OPENAI_COMPAT_TOML, &seat0), (OPENAI_COMPAT_TOML, &seat1)],
        &wire_checked(ONE_ATTEMPT),
    );

    let dispatched = router
        .complete_with_options(compat_req(&FOUR_KEYS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err(), "premise");
    assert_eq!(
        seat1.calls(),
        2,
        "premise: the plain refusal follows the fifth repair"
    );
    assert_eq!(
        router.field_repair_counters().repairs_per_request,
        one_request_in(5),
        "premise: the allowance was spent when the plain refusal arrived",
    );
    assert_eq!(router.field_repair_counters().budget_exhausted, 0);
    assert!(exhausted_lanes(&router).is_empty());
}

// -----------------------------------------------------------------------
// Repairs per request, on every walk
// -----------------------------------------------------------------------

/// Which walk a histogram case drives.
#[derive(Clone, Copy, Debug)]
enum Walk {
    Complete,
    Stream,
    Count,
}

const WALKS: [Walk; 3] = [Walk::Complete, Walk::Stream, Walk::Count];

/// Drive one request of `req` on `walk`, discarding the outcome.
async fn drive(router: &Router, walk: Walk, req: ChatRequest) {
    match walk {
        Walk::Complete => drop(
            router
                .complete_with_options(req, RouterOptions::new())
                .await,
        ),
        Walk::Stream => drop(router.stream_with_options(req, RouterOptions::new()).await),
        Walk::Count => drop(router.count_tokens_with_meta(req).await),
    }
}

/// Mutation check (one per walk): delete the `record_repairs_per_request`
/// call beside that walk's `emit_field_repair` -> red on that walk's row.
#[tokio::test]
async fn a_request_with_no_repair_lands_in_bucket_zero_on_every_walk() {
    for walk in WALKS {
        let (router, seen) = single_seat(ALIAS, Answer::ServeImmediately);

        drive(&router, walk, req_on(ALIAS)).await;

        assert_eq!(seen.calls(), 1, "{walk:?}: premise");
        assert_eq!(
            router.field_repair_counters().repairs_per_request,
            one_request_in(0),
            "{walk:?}",
        );
    }
}

/// Mutation check: record `spent() + 1` -> red on every walk.
#[tokio::test]
async fn a_two_key_request_lands_in_bucket_two_on_every_walk() {
    for walk in WALKS {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        drive(&router, walk, two_key_req()).await;

        assert_eq!(seat.calls(), 3, "{walk:?}: premise");
        assert_eq!(
            router.field_repair_counters().repairs_per_request,
            one_request_in(2),
            "{walk:?}",
        );
    }
}

/// The ceiling chain spends the whole allowance on every walk; the token
/// count needs the walking answer to reach past its first seat.
#[tokio::test]
async fn a_request_that_spends_the_allowance_lands_in_the_last_bucket_on_every_walk() {
    let _injection = provisional::inject(REJECTED_PATH);
    for walk in WALKS {
        let answer = match walk {
            Walk::Count => Answer::RejectThenWalk,
            Walk::Complete | Walk::Stream => Answer::AlwaysRejectField,
        };
        let (router, observed) = chain_of(ALIAS, CHAIN_TARGETS, answer);

        drive(&router, walk, req_on(ALIAS)).await;

        assert_eq!(
            repairs_observed(&observed),
            usize::from(REPAIRS_PER_REQUEST),
            "{walk:?}: premise",
        );
        assert_eq!(
            router.field_repair_counters().repairs_per_request,
            one_request_in(usize::from(REPAIRS_PER_REQUEST)),
            "{walk:?}",
        );
    }
}
