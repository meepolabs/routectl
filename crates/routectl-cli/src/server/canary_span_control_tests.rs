// Cadence CONTROLS for the spanning canary proof: the surfaces that must not advance
// the countdown, and the feature-absent case that makes every assertion in the host
// non-vacuous.
//
// An `include!`d FRAGMENT of `canary_span_tests.rs` -- no top-level `use`, no `mod`;
// the imports and the serial-group rationale live in that host.

/// A STREAM at the cadence boundary does not advance the countdown or claim the
/// canary.
///
/// Pinned here at the integration level as well as in the router's own unit
/// sidecar, because the consequence is a cross-seam one: a stream that consumed
/// the interval would spend the slot a completion is supposed to fill, and the
/// identity would then be re-verified on a surface whose outcome the walk cannot
/// settle -- there is no assembled response to read a verdict from.
///
/// Asserted as a PAIR with the completion below: the stream leaves the cadence
/// exactly where it was, and the completion that follows is the one that trips it.
#[tokio::test]
#[serial_test::serial]
async fn a_stream_at_the_boundary_neither_advances_the_cadence_nor_claims_the_canary() {
    let (config, _dir) = spanning_config();
    let seat = Arc::new(Seat::default());
    let router = router_with(&config, Arc::clone(&seat));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);

    // Walk to one before the trip through completions.
    for _ in 0..(EXPECTED_CADENCE - 1) {
        let _ = router
            .complete_with_options(req(), RouterOptions::new())
            .await;
    }

    // Act: a stream AND a token count at the boundary. Neither may advance it.
    let streamed = router.stream(req()).await;
    assert!(streamed.is_ok(), "the stream serves");
    let counted = router.count_tokens_with_meta(req()).await;
    assert!(counted.result.is_ok(), "the token count serves");

    // Assert: still nothing carried -- both were repaired, neither restored.
    assert_eq!(
        seat.carried_per_attempt().iter().filter(|c| **c).count(),
        0,
        "neither a stream nor a token count claims the canary: only a non-streaming \
         completion can settle its outcome",
    );

    // And the COMPLETION that follows is the one that trips it, which is what
    // proves the cadence was not advanced rather than merely not claimed.
    let dispatched = router
        .complete_with_options(req(), RouterOptions::new())
        .await;
    assert!(dispatched.result.is_ok());
    assert_eq!(
        seat.carried_per_attempt().last(),
        Some(&true),
        "the next eligible COMPLETION carries the canary -- so the stream and the \
         count left the countdown exactly where it was",
    );
}

/// The feature-ABSENT control for the whole file: with the verdict already gone,
/// a full cadence of requests forwards unchanged and no canary ever runs.
///
/// Without it every assertion above would pass against a build that restored the
/// field on some fixed schedule regardless of any verdict -- which is a fidelity
/// defect wearing the feature's clothes.
#[tokio::test]
#[serial_test::serial]
async fn with_no_resident_verdict_a_full_cadence_forwards_unchanged_and_runs_no_canary() {
    let (config, _dir) = spanning_config();
    let seat = Arc::new(Seat::default());
    let router = router_with(&config, Arc::clone(&seat));
    // No verdict planted.

    for _ in 0..EXPECTED_CADENCE {
        let dispatched = router
            .complete_with_options(req(), RouterOptions::new())
            .await;
        assert!(dispatched.result.is_ok());
        assert!(
            dispatched.meta.cleared_capabilities.is_empty(),
            "nothing is cleared when nothing was learned",
        );
    }

    let carried = seat.carried_per_attempt();
    assert_eq!(carried.len(), EXPECTED_CADENCE as usize);
    assert!(
        carried.iter().all(|c| *c),
        "EVERY request forwards the client's field verbatim: with no resident \
         verdict nothing authorizes a rewrite, and nothing schedules a canary",
    );
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        0,
        "and no adopted row rewrite is counted",
    );
}
