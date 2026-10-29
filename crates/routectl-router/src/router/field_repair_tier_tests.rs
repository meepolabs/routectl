// The attribution tier a repaired success commits at: a self-identifying
// rejection acts on its one observation, an inferred one persists pending and
// acts only once an independent request corroborates it.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

use routectl_core::capability::SignalTier;

/// The resident learned entry for the rejected field on `nickname`'s lane.
fn resident_entry(
    router: &Router,
    nickname: &str,
) -> crate::learned_capability::LearnedRegistryEntry {
    router
        .learned_capability_snapshot()
        .into_iter()
        .find(|entry| {
            entry.state_key
                == crate::router::probe_test_support::seat_chain_lane(nickname).as_lane_key()
                && entry.feature_key == rejected_key()
        })
        .expect("a committed verdict is resident")
}

fn rejected_verdict_acts(router: &Router, nickname: &str) -> bool {
    let key = crate::field_verdict::FieldVerdictKey::new(
        &crate::router::probe_test_support::seat_chain_lane(nickname),
        REJECTED_PATH,
        ANTHROPIC_API_KIND,
    )
    .expect("the grounded path mints a key");
    router
        .field_verdicts()
        .is_negative_acting(&key, std::time::Instant::now())
}

/// Dispatch one request on a one-seat chain whose rejection is attributed at
/// `tier`, returning the router and the one learned row it committed.
async fn commit_once_at(tier: SignalTier) -> (Router, crate::router::CapabilityLearnEvent) {
    let _injection = provisional::inject_with_tier(REJECTED_PATH, tier);
    let (router, _seen) = single_seat(ALIAS, Answer::ServeRepaired);
    let mut dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(dispatched.result.is_ok(), "premise: the repair served");
    assert_eq!(
        dispatched.meta.learned_capabilities.len(),
        1,
        "premise: the repaired success committed one row",
    );
    let event = dispatched.meta.learned_capabilities.remove(0);
    (router, event)
}

#[tokio::test]
async fn a_self_identifying_repaired_success_persists_and_acts_after_one_observation() {
    let (router, event) = commit_once_at(SignalTier::SelfIdentifying).await;

    assert_eq!(event.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(event.observations, 1);
    let entry = resident_entry(&router, "m0");
    assert_eq!(entry.signal_tier, SignalTier::SelfIdentifying);
    assert!(
        rejected_verdict_acts(&router, "m0"),
        "a self-identifying attribution acts on its single repaired success",
    );
}

/// Mutation check: pass `SignalTier::SelfIdentifying` instead of `tier` to the
/// observation in `FieldRepairGuard::commit` -> red on the acting assertion.
#[tokio::test]
async fn an_inferred_repaired_success_persists_one_observation_and_does_not_act() {
    let (router, event) = commit_once_at(SignalTier::Inferred).await;

    assert_eq!(
        event.signal_tier,
        SignalTier::Inferred,
        "the event row carries the tier the rejection was attributed at, so a warm \
         rebuild replays it at the same confidence",
    );
    assert_eq!(event.observations, 1);
    let entry = resident_entry(&router, "m0");
    assert_eq!(entry.signal_tier, SignalTier::Inferred);
    assert_eq!(entry.observations, 1, "the verdict persists pending");
    assert!(
        !rejected_verdict_acts(&router, "m0"),
        "an inferred attribution must not act on one observation",
    );
}

#[tokio::test]
async fn a_second_independent_inferred_confirmation_acts() {
    let _injection = provisional::inject_with_tier(REJECTED_PATH, SignalTier::Inferred);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let first = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;
    assert!(first.result.is_ok(), "premise: the first repair served");
    assert!(
        !rejected_verdict_acts(&router, "m0"),
        "premise: one inferred observation does not act",
    );

    let second = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(second.result.is_ok());
    assert_eq!(
        seen.carried_per_attempt(),
        vec![true, false, true, false],
        "a PENDING verdict leaves the slot open, so the second request carried the \
         field and repaired it on its own rejection",
    );
    let event = second
        .meta
        .learned_capabilities
        .first()
        .expect("the corroborating commit rides out a row");
    assert_eq!(event.signal_tier, SignalTier::Inferred);
    assert_eq!(event.observations, 2);
    assert!(
        rejected_verdict_acts(&router, "m0"),
        "a second independent inferred confirmation acts",
    );
}

/// Two chain seats on ONE lane, so one request meets the same identity twice.
/// Seat `m0` rejects the field and answers its repaired attempt with a 503, so
/// the walk falls back; seat `m1` rejects the field and serves the repair.
fn shared_lane_chain() -> (Router, Arc<Observed>, Arc<Observed>) {
    let config = chain_config(ALIAS, 2);
    let mut router = Router::new(Arc::new(config));
    let seen0 = Arc::new(Observed::default());
    let seen1 = Arc::new(Observed::default());
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, answer, seen) in [
        ("m0", Answer::RejectThenUnavailable, &seen0),
        ("m1", Answer::ServeRepaired, &seen1),
    ] {
        models.insert(
            nickname.to_string(),
            Arc::new(ResolvedModel::new(
                nickname,
                "p0",
                Arc::new(MockSeat::new(answer, Arc::clone(seen))) as Arc<dyn Provider>,
                "wire-0",
            )),
        );
    }
    router.install_resolved_models(models);
    (router, seen0, seen1)
}

/// Mutation check: commit each repaired attempt's guard as soon as `apply_named`
/// repairs it, instead of once at the request's successful settlement -> two
/// rows, and the second corroborates the first.
#[tokio::test]
async fn one_request_meeting_the_same_lane_twice_counts_one_inferred_observation() {
    let _injection = provisional::inject_with_tier(REJECTED_PATH, SignalTier::Inferred);
    let (router, seen0, seen1) = shared_lane_chain();

    let dispatched = router
        .complete_with_options(req_on(ALIAS), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "premise: the second seat served");
    assert!(
        seen0.repairs() > 0 && seen1.repairs() > 0,
        "premise: both seats repaired the field, so the lane saw two rejections in \
         this one request",
    );
    assert_eq!(
        dispatched.meta.learned_capabilities.len(),
        1,
        "one request commits at most one row per identity",
    );
    let entry = resident_entry(&router, "m0");
    assert_eq!(
        entry.observations, 1,
        "two rejections inside one request are not independent: they must not \
         corroborate each other",
    );
    assert!(!rejected_verdict_acts(&router, "m0"));
}
