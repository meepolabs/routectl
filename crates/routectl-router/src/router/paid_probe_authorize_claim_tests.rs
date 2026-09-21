// Concurrent claiming, the preconditions that refuse before the ledger, and the
// missing-profile sweep.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// Two concurrent claimers: exactly one reservation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_claimers_of_one_candidate_produce_exactly_one_reservation() {
    // THE claim-atomicity assertion. A claim that spanned the reservation await
    // -- or read the list and then removed from it -- would let both tasks hold
    // one candidate and both commit a unit, which for a never-refunded day
    // counter is permanent overspend.
    //
    // The ledger is SLOW so the two tasks provably overlap inside the
    // reservation window: with an instant double, the first task could finish
    // before the second started and the test would pass on a racy claim.
    struct SlowLedger {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl PaidProbeLedger for SlowLedger {
        async fn reserve_paid_probe_unit(&self, _: &str, cap: u32) -> PaidProbeReservation {
            self.calls.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            PaidProbeReservation::Committed { used: 1, cap }
        }
    }
    let ledger = Arc::new(SlowLedger {
        calls: AtomicUsize::new(0),
    });
    let router = Arc::new(
        Fixture::default()
            .bare_router()
            .with_paid_probe_ledger(ledger.clone()),
    );
    seed_candidate(&router, &key());

    let left = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.authorize_paid_probe().await })
    };
    let right = {
        let router = Arc::clone(&router);
        tokio::spawn(async move { router.authorize_paid_probe().await })
    };
    let outcomes = [
        left.await.expect("claimer must not panic"),
        right.await.expect("claimer must not panic"),
    ];

    let authorized = outcomes
        .iter()
        .filter(|o| matches!(o, PaidProbeOutcome::Authorized(_)))
        .count();
    assert_eq!(
        authorized, 1,
        "exactly one claimer may authorize: {outcomes:?}",
    );
    assert_eq!(
        outcomes
            .iter()
            .filter_map(PaidProbeOutcome::refusal)
            .collect::<Vec<_>>(),
        vec![PaidProbeRefusal::NoCandidate],
        "the loser must refuse for having claimed nothing, not for a \
         precondition",
    );
    assert_eq!(
        ledger.calls.load(Ordering::Acquire),
        1,
        "one candidate must produce exactly ONE reservation, however the two \
         claimers interleave",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claimers_never_reserve_more_units_than_there_are_candidates() {
    // The same invariant at width: N candidates, far more claimers, and the
    // ledger commits every time it is asked -- so any double claim shows up as
    // a reservation count above N.
    let ledger = CountingLedger::committed();
    let router = Arc::new(
        Fixture::default()
            .bare_router()
            .with_paid_probe_ledger(ledger.clone()),
    );
    // Every lane must resolve `m1`'s seat, so each candidate is admissible on
    // every ground except its identity -- otherwise a claim could refuse for a
    // precondition and hide a double claim.
    let candidates = 4;
    for _ in 0..candidates {
        // Distinct identities on the SAME resolvable state key are not
        // constructible (the key IS the identity), so the list is seeded with
        // one candidate per claim opportunity by re-seeding after each drain.
        seed_candidate(&router, &key());
        let claimed = router.authorize_paid_probe().await;
        assert!(
            matches!(claimed, PaidProbeOutcome::Authorized(_)),
            "premise: each seeded candidate must be authorizable: {claimed:?}",
        );
    }
    assert_eq!(
        ledger.calls(),
        candidates,
        "one reservation per candidate, no more",
    );

    // Now the contended case: ONE candidate, sixteen claimers.
    seed_candidate(&router, &key());
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let router = Arc::clone(&router);
            tokio::spawn(async move { router.authorize_paid_probe().await })
        })
        .collect();
    let mut authorized = 0;
    for handle in handles {
        if matches!(
            handle.await.expect("claimer must not panic"),
            PaidProbeOutcome::Authorized(_)
        ) {
            authorized += 1;
        }
    }

    assert_eq!(authorized, 1, "one candidate, one authorization");
    assert_eq!(
        ledger.calls(),
        candidates + 1,
        "sixteen contending claimers must add exactly one reservation",
    );
}

// ---------------------------------------------------------------------------
// Preconditions that refuse BEFORE the ledger, each with a positive control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_incarnation_refuses_without_asking_the_ledger() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    let live = router.probe_incarnation();
    seed_candidate_at(&router, &key(), live + 1);

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::StaleIncarnation));
    assert_eq!(
        ledger.calls(),
        0,
        "a candidate from another incarnation describes router state that no \
         longer serves, so there is nothing to ask about",
    );
}

#[tokio::test]
async fn a_live_incarnation_on_the_same_fixture_does_reach_the_ledger() {
    // POSITIVE CONTROL for the staleness case: same router, same seeding helper,
    // the LIVE incarnation. Without it a zero call count above would be free on
    // a fixture that could never reserve at all.
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate_at(&router, &key(), router.probe_incarnation());

    let outcome = router.authorize_paid_probe().await;

    assert!(matches!(outcome, PaidProbeOutcome::Authorized(_)));
    assert_eq!(ledger.calls(), 1);
}

#[tokio::test]
async fn an_unattributable_entry_refuses_without_asking_the_ledger() {
    // A loopback base URL, which `probe_entry_is_attributable` refuses through
    // the SAME predicate both free stages call -- so a paid rejection this
    // stage could not attribute is never paid for.
    let ledger = CountingLedger::committed();
    let router = Fixture::default()
        .loopback_entry()
        .router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::Unattributable));
    assert_eq!(
        ledger.calls(),
        0,
        "an unattributable entry is knowable locally, so no unit may be \
         committed to find out",
    );
}

#[tokio::test]
async fn an_attributable_entry_on_the_same_fixture_does_reach_the_ledger() {
    // POSITIVE CONTROL for the attributability case: the identical fixture with
    // a remote base URL.
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert!(matches!(outcome, PaidProbeOutcome::Authorized(_)));
    assert_eq!(ledger.calls(), 1);
}

#[tokio::test]
async fn an_identity_naming_no_seat_refuses_without_asking_the_ledger() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default()
        .without_resolved_model()
        .router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::NoSeat));
    assert_eq!(ledger.calls(), 0);
}

#[tokio::test]
async fn a_zero_live_cap_refuses_without_asking_the_ledger_while_a_free_validator_still_runs() {
    // Cap zero blocks every paid call, and the assertion has two halves because
    // a fixture that blocked ALL probe work would satisfy the first half for the
    // wrong reason: the FREE count-token validator must still execute on the
    // very same router.
    let ledger = CountingLedger::committed();
    let free_provider = Arc::new(ZeroCountProvider {
        calls: AtomicUsize::new(0),
    });
    let mut router = Fixture::default().cap(0).bare_router();
    // The free control needs a provider whose count_tokens is observable, so the
    // resolved table is rebuilt around it -- everything else stays the fixture's.
    let model = ResolvedModel::new(
        "m1",
        PROVIDER,
        Arc::clone(&free_provider) as Arc<dyn Provider>,
        "claude-sonnet-4-5",
    )
    .with_effective_row(fully_priced());
    let mut models = std::collections::BTreeMap::new();
    models.insert("m1".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    let router = router.with_paid_probe_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::CapZero));
    assert_eq!(
        ledger.calls(),
        0,
        "a zero live cap is knowable without asking the accounting layer",
    );

    // THE FREE CONTROL, driven directly through the production validator on the
    // same router: cap zero must block the PAID class, not probing as such.
    let free = router
        .run_free_validator_for_tests(&key(), &payload(), ProbeValidator::CountTokens)
        .await;
    assert_eq!(
        free,
        FreeValidatorOutcome::Inconclusive,
        "the free validator must still run under a zero paid cap",
    );
    assert_eq!(
        free_provider.calls.load(Ordering::SeqCst),
        1,
        "and must actually have dialed the free count-token endpoint",
    );
}

#[tokio::test]
async fn a_nonzero_live_cap_on_the_same_fixture_does_reach_the_ledger() {
    // POSITIVE CONTROL for the cap-zero case: one knob different.
    let ledger = CountingLedger::committed();
    let router = Fixture::default().cap(1).router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert!(matches!(outcome, PaidProbeOutcome::Authorized(_)));
    assert_eq!(ledger.seen(), vec![(PROVIDER.to_string(), 1)]);
}

#[tokio::test]
async fn every_missing_profile_class_refuses_without_asking_the_ledger() {
    // EVERY class of missing catalog fact, by name, each on the otherwise
    // fully-admissible fixture. The profile derivation has its own per-fact
    // sidecar; what this pins is that each class reaches the paid stage as a
    // refusal with ZERO reservations.
    let mut row_absent_input = CatalogRow::sentinel();
    row_absent_input.output_cost_per_token = Some(1.5e-5);
    row_absent_input.max_output_tokens = Some(64_000);
    let mut row_absent_output = CatalogRow::sentinel();
    row_absent_output.input_cost_per_token = Some(3.0e-6);
    row_absent_output.max_output_tokens = Some(64_000);
    let mut row_zero_rate = CatalogRow::sentinel();
    row_zero_rate.input_cost_per_token = Some(0.0);
    row_zero_rate.output_cost_per_token = Some(1.5e-5);
    row_zero_rate.max_output_tokens = Some(64_000);
    let mut row_nonfinite = CatalogRow::sentinel();
    row_nonfinite.input_cost_per_token = Some(f32::NAN);
    row_nonfinite.output_cost_per_token = Some(1.5e-5);
    row_nonfinite.max_output_tokens = Some(64_000);
    let mut row_no_ceiling = CatalogRow::sentinel();
    row_no_ceiling.input_cost_per_token = Some(3.0e-6);
    row_no_ceiling.output_cost_per_token = Some(1.5e-5);
    row_no_ceiling.max_output_tokens = None;
    let mut row_ceiling_below_floor = CatalogRow::sentinel();
    row_ceiling_below_floor.input_cost_per_token = Some(3.0e-6);
    row_ceiling_below_floor.output_cost_per_token = Some(1.5e-5);
    row_ceiling_below_floor.max_output_tokens = Some(LEGACY_MIN_VIABLE_MAX_TOKENS - 1);

    let present = |row: CatalogRow| EffectiveRow::Present {
        row,
        source: Source::Baked,
        verified_at: STAMP.to_string(),
    };
    let classes: Vec<(&str, EffectiveRow)> = vec![
        ("missing cell", EffectiveRow::Missing),
        ("disabled cell", EffectiveRow::Disabled),
        ("absent input rate", present(row_absent_input)),
        ("absent output rate", present(row_absent_output)),
        ("zero rate", present(row_zero_rate)),
        ("non-finite rate", present(row_nonfinite)),
        ("no output ceiling", present(row_no_ceiling)),
        ("ceiling below the floor", present(row_ceiling_below_floor)),
    ];

    for (name, cell) in classes {
        let ledger = CountingLedger::committed();
        let router = Fixture::default()
            .effective_row(cell)
            .router_with_ledger(ledger.clone());
        seed_candidate(&router, &key());

        let outcome = router.authorize_paid_probe().await;

        assert_eq!(
            outcome.refusal(),
            Some(PaidProbeRefusal::NoProfile),
            "{name} must refuse for want of a profile",
        );
        assert_eq!(
            ledger.calls(),
            0,
            "{name}: no unit may be committed for a body that cannot be sized",
        );
    }
}

#[tokio::test]
async fn an_absent_ledger_refuses_closed_and_discards_without_a_trait_call() {
    // Every local check PASSES here, so the refusal is the fail-closed default
    // of the seam itself: a Router nobody installed accounting on cannot spend.
    //
    // Reported as its OWN refusal rather than as the ledger's `Unavailable`,
    // because absence is knowable WITHOUT a call and is terminal for the process
    // (the installation is a consuming boot-time builder). So the candidate is
    // discarded rather than requeued onto a queue where it could only be
    // re-refused for the daemon's life.
    let router = Fixture::default().bare_router();
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::NoLedger),
        "an absent ledger is its own terminal refusal, not a reservation outcome",
    );
    assert!(
        router.paid_probe_ledger().is_none(),
        "premise: no ledger may be installed",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a terminal absence must DISCARD the candidate: nothing can install a          ledger later in this process, so a requeue could only be re-refused",
    );
    // No slot may have been taken either: the absence is checked before the
    // acquisition, so a refusal here costs neither a unit nor capacity.
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "no concurrency slot may be held after a terminal refusal",
    );
}

#[tokio::test]
async fn an_installed_ledger_answering_unavailable_is_terminal_and_discards() {
    // The OTHER absence case, and the one that looks retryable but is not: the
    // ledger exists and answers `Unavailable`, which means no accounting is
    // reachable (shutting down). Neither a missing installation nor a shutdown
    // reverses, so a requeue could only be re-refused on every later pass while
    // holding a bounded slot a probeable lane could use.
    //
    // Contrast `Overloaded`, asserted as the control below: that is LOAD, and
    // load passes.
    let ledger = CountingLedger::answering(PaidProbeReservation::Unavailable);
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Accounting(
            PaidProbeReservation::Unavailable
        )),
        "the refusal passes through as the ledger's own outcome",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "premise: the ledger WAS asked -- this is not the absent-installation          path, which refuses without a call",
    );
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "an unreachable accounting layer is terminal for this process, so the          candidate is discarded",
    );

    // POSITIVE CONTROL on the same fixture shape: Overloaded DOES requeue, so
    // the discard above is about unreachability rather than about every
    // accounting refusal being terminal.
    let overloaded = CountingLedger::answering(PaidProbeReservation::Overloaded);
    let router = Fixture::default().router_with_ledger(overloaded.clone());
    seed_candidate(&router, &key());

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(
        outcome.refusal(),
        Some(PaidProbeRefusal::Accounting(
            PaidProbeReservation::Overloaded
        )),
    );
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "load passes without a publication, so this one is held for a retry",
    );
}

// The requeue / slot / finality / surface-guard groups live in a sibling file to
// keep every file under the size ceiling. They compile into THIS module via
// `include!`, so the fixture helpers above stay in scope and no test's module
// path changes.

// The fence group lives in a sibling file to keep every file under the size
// ceiling. It compiles into THIS module via `include!`, so the fixture helpers
// above stay in scope and no test's module path changes.
