// Fairness across passes, and what a Debug rendering must not print.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// Fairness: a repeatedly-refusing candidate must not starve a healthy one
// ---------------------------------------------------------------------------

/// A ledger that always refuses with a REQUEUEING outcome, recording WHICH
/// provider each call named.
///
/// The recorded provider is the discriminating observation: a fairness question
/// is about which LANE gets asked, and the provider argument is the only thing
/// distinguishing them at this boundary.
struct AlwaysOverloadedLedger {
    asked: parking_lot::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl PaidProbeLedger for AlwaysOverloadedLedger {
    async fn reserve_paid_probe_unit(&self, provider: &str, _: u32) -> PaidProbeReservation {
        self.asked.lock().push(provider.to_string());
        PaidProbeReservation::Overloaded
    }
}

#[tokio::test]
async fn a_repeatedly_refused_candidate_does_not_starve_the_queue_behind_it() {
    // FAIRNESS, and the reason the claim/requeue pair is FIFO. With a LIFO pair
    // (pop back, push back) the candidate just refused is re-claimed on the very
    // next pass, so one lane whose accounting keeps refusing -- a cap exhausted
    // until the UTC rollover, an overloaded layer -- is the ONLY lane ever tried,
    // for as long as the condition lasts. Every other exhausted lane waits behind
    // it and is never asked.
    let ledger = Arc::new(AlwaysOverloadedLedger {
        asked: parking_lot::Mutex::new(Vec::new()),
    });
    let router = two_lane_router(ledger.clone());
    seed_candidate(&router, &lane_key("m1"));
    seed_candidate(&router, &lane_key("m2"));

    // Four passes over a two-candidate queue.
    for _ in 0..4 {
        let outcome = router.authorize_paid_probe().await;
        assert_eq!(
            outcome.refusal(),
            Some(PaidProbeRefusal::Accounting(
                PaidProbeReservation::Overloaded
            )),
            "premise: every pass must refuse with a REQUEUEING outcome",
        );
        assert_eq!(
            router.all_recorded_paid_candidates_for_tests().len(),
            2,
            "both candidates must stay queued across a refusing pass",
        );
    }

    // THE ASSERTION: the two lanes ALTERNATE. Under LIFO the same provider is
    // asked four times and the sibling never.
    let asked = ledger.asked.lock().clone();
    assert_eq!(
        asked,
        vec![PROVIDER, SECOND_PROVIDER, PROVIDER, SECOND_PROVIDER],
        "a refused candidate must go BEHIND the other one, or one lane starves \
         the queue for as long as its condition lasts",
    );
}

#[tokio::test]
async fn a_healthy_candidate_is_reached_on_the_pass_after_a_refused_one() {
    // The same fairness property stated as the OUTCOME an operator cares about:
    // a lane that CAN commit does commit, on the pass after a sibling refused,
    // without waiting for the sibling's condition to clear.
    //
    // The ledger refuses only the FIRST provider it is asked about and commits for
    // anything else -- keyed on the provider rather than on call order, so the
    // discriminating fact is WHICH lane committed. Under LIFO the second pass
    // re-claims the refused lane and refuses again.
    struct RefuseOneProviderLedger {
        refuse: String,
        asked: parking_lot::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl PaidProbeLedger for RefuseOneProviderLedger {
        async fn reserve_paid_probe_unit(&self, provider: &str, cap: u32) -> PaidProbeReservation {
            self.asked.lock().push(provider.to_string());
            if provider == self.refuse {
                return PaidProbeReservation::Overloaded;
            }
            PaidProbeReservation::Committed { used: 1, cap }
        }
    }
    let ledger = Arc::new(RefuseOneProviderLedger {
        refuse: PROVIDER.to_string(),
        asked: parking_lot::Mutex::new(Vec::new()),
    });
    let router = two_lane_router(ledger.clone());
    seed_candidate(&router, &lane_key("m1"));
    seed_candidate(&router, &lane_key("m2"));

    let refused = router.authorize_paid_probe().await;
    assert_eq!(
        refused.refusal(),
        Some(PaidProbeRefusal::Accounting(
            PaidProbeReservation::Overloaded
        )),
        "premise: the first pass must hit the refusing lane and requeue",
    );

    let committed = router.authorize_paid_probe().await;

    let PaidProbeOutcome::Authorized(auth) = committed else {
        panic!("the second pass must authorize: {committed:?}");
    };
    assert_eq!(
        auth.key(),
        &lane_key("m2"),
        "the pass after a refusal must reach the OTHER lane -- reaching the same \
         one again is the starvation this rotation exists to prevent",
    );
    assert_eq!(
        auth.seat().provider_name,
        SECOND_PROVIDER,
        "and its reservation was committed for that lane's own provider",
    );
    assert_eq!(
        ledger.asked.lock().clone(),
        vec![PROVIDER, SECOND_PROVIDER],
        "the accounting layer saw each lane exactly once, in rotation order",
    );
}

// ---------------------------------------------------------------------------
// Debug must not print the request
// ---------------------------------------------------------------------------

/// A beta token carrying a distinctive sentinel, planted in the payload of the
/// lane UNDER TEST.
///
/// Realistic in shape (a dated slug, which is what
/// `is_retainable_beta` accepts) so the payload constructor admits it -- a
/// sentinel the retention bound refused would never reach the value being
/// formatted, and the test would pass for that reason instead.
const OWN_SENTINEL: &str = "own-secret-beta-2026-01-01";

/// The same, planted in an UNRELATED lane's queued payload.
///
/// Needed separately because the two leak by different routes: the authorization
/// leaks its OWN payload through its fields, while a slot printing its scheduler
/// would leak every OTHER lane's payload out of the job table. One sentinel could
/// not tell those apart.
const OTHER_SENTINEL: &str = "other-lane-beta-2026-01-01";

/// A payload carrying `beta` as its client token.
fn payload_with_beta(beta: &str) -> ProbePayload {
    ProbePayload::new(
        GROUNDED_PATH,
        "summarized".to_string(),
        &[beta.to_string()],
        &[],
        true,
    )
    .expect("a dated slug is within every retention bound")
}

#[tokio::test]
async fn neither_debug_rendering_prints_any_request_content() {
    // A paid authorization is held on a money-spending path, which is exactly
    // where a diagnostic reaches for `{:?}`. A DERIVED Debug would print the
    // captured `ProbePayload` -- the probed value and both retained beta sets,
    // all client-supplied -- plus the seat and its provider object; and a derived
    // Debug on the SLOT would recurse into `Arc<ProbeScheduler>` and render the
    // whole job table, i.e. every OTHER lane's captured request context.
    //
    // Both routes are planted separately, because one sentinel cannot tell them
    // apart.
    let router = Fixture::default().router();
    // The unrelated lane's payload goes into the scheduler's job table, which is
    // what a slot printing its scheduler would expose.
    assert_eq!(
        router.activate_probe_plan_for_tests(&key_n(5), payload_with_beta(OTHER_SENTINEL)),
        crate::probe_scheduler::ProbeActivation::Queued,
        "premise: the unrelated lane's payload must actually be queued, or the \
         slot half of this test cannot leak it",
    );
    // The lane under test carries its own sentinel in the candidate payload the
    // authorization will own.
    router
        .paid_probe_candidates
        .lock()
        .push_back(PaidProbeCandidate {
            key: key(),
            incarnation: router.probe_incarnation(),
            validator: ProbeValidator::PaidCompletion,
            payload: payload_with_beta(OWN_SENTINEL),
        });

    let outcome = router.authorize_paid_probe().await;
    let PaidProbeOutcome::Authorized(auth) = outcome else {
        panic!("premise: the lane must authorize so there is a value to format: {outcome:?}");
    };

    // PREMISE, asserted rather than assumed: both sentinels must really be
    // reachable from the value being formatted, or "absent from the output" is
    // free and this test proves nothing.
    assert_eq!(
        auth.payload().client_betas(),
        &[OWN_SENTINEL.to_string()],
        "premise: the authorization must actually own the sentinel payload",
    );
    assert!(
        router
            .probe_scheduler_snapshot()
            .queued
            .saturating_add(router.probe_scheduler_snapshot().backing_off)
            > 0,
        "premise: the unrelated lane's job must still be in the table",
    );

    let rendered = format!("{auth:?}");

    for sentinel in [OWN_SENTINEL, OTHER_SENTINEL] {
        assert!(
            !rendered.contains(sentinel),
            "the authorization's Debug leaked request content ({sentinel}): \
             {rendered}",
        );
    }
    // The seat's provider object and the identity must not appear either.
    assert!(
        !rendered.contains(PROVIDER),
        "the authorization's Debug leaked the provider identity: {rendered}",
    );
    assert!(
        !rendered.contains("summarized"),
        "nor may it leak the probed field VALUE: {rendered}",
    );
    // What it DOES print is the accounting shape, so the impl is not merely
    // empty -- an empty Debug would pass every assertion above while being
    // useless, and a later author would replace it with a derive.
    assert!(
        rendered.contains("committed_used"),
        "the Debug must still report the accounting shape: {rendered}",
    );
    assert!(
        rendered.contains("<elided>"),
        "and must name the withheld fields rather than omitting them silently: \
         {rendered}",
    );
}

#[test]
fn the_paid_slots_debug_prints_no_lane_from_the_scheduler() {
    // The slot half, driven on the SCHEDULER directly so the job table is
    // populated with several lanes' payloads. A derived Debug here renders
    // `Arc<ProbeScheduler>` and therefore every one of them.
    let scheduler = Arc::new(crate::probe_scheduler::ProbeScheduler::new());
    for n in 0..3 {
        let identity = FieldVerdictKey::new(&format!("lane-{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        assert_eq!(
            scheduler.activate(
                &identity,
                1,
                vec![ProbeValidator::CountTokens],
                payload_with_beta(OTHER_SENTINEL),
            ),
            crate::probe_scheduler::ProbeActivation::Queued,
            "premise: lane {n}'s payload must be in the table",
        );
    }
    let slot = scheduler
        .try_acquire_paid_slot()
        .expect("a slot is available");

    let rendered = format!("{slot:?}");

    assert!(
        !rendered.contains(OTHER_SENTINEL),
        "the slot's Debug leaked another lane's captured beta context: {rendered}",
    );
    assert!(
        !rendered.contains("lane-0"),
        "nor another lane's identity: {rendered}",
    );
    assert!(
        !rendered.contains("summarized"),
        "nor any probed field value: {rendered}",
    );
    assert!(
        rendered.contains("PaidProbeSlot"),
        "it must still identify itself: {rendered}",
    );
}
