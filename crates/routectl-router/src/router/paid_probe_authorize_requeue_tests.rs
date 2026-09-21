// The requeue rules, the shared concurrency slot, commit finality, and the
// surface guards.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason -- imports live in the host.

// ---------------------------------------------------------------------------
// Which refusals requeue, and that a requeue respects the bound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_recoverable_accounting_refusal_requeues_the_candidate_exactly_once() {
    // The four RECOVERABLE accounting outcomes: each can change WITHOUT a new
    // router generation -- a cap turns over at the UTC rollover, an overloaded
    // layer sheds load, a failed write may land, malformed state may be repaired
    // externally -- so the claimed candidate goes back, once, with no duplicate.
    //
    // `Unavailable` is deliberately NOT in this set: it means no accounting is
    // reachable, which nothing in this process reverses, and
    // `an_installed_ledger_answering_unavailable_is_terminal_and_discards` pins
    // that direction with `Overloaded` as its control.
    for refusal in [
        PaidProbeReservation::CapExhausted,
        PaidProbeReservation::Overloaded,
        PaidProbeReservation::WriteFailed,
        PaidProbeReservation::MalformedState,
    ] {
        let ledger = CountingLedger::answering(refusal.clone());
        let router = Fixture::default().router_with_ledger(ledger.clone());
        seed_candidate(&router, &key());

        let outcome = router.authorize_paid_probe().await;

        assert_eq!(
            outcome.refusal(),
            Some(PaidProbeRefusal::Accounting(refusal.clone())),
        );
        let requeued = router.all_recorded_paid_candidates_for_tests();
        assert_eq!(
            requeued.len(),
            1,
            "{} must requeue the claimed candidate exactly once: {requeued:?}",
            refusal.as_str(),
        );
        assert_eq!(requeued[0].key, key());
        assert_eq!(
            requeued[0].payload,
            payload(),
            "the requeued candidate must keep its payload, or the retry would \
             ask a different question",
        );

        // A SECOND pass must not duplicate it: claim, refuse, requeue again.
        let _second = router.authorize_paid_probe().await;
        assert_eq!(
            router.all_recorded_paid_candidates_for_tests().len(),
            1,
            "{}: repeated refusals must not grow the list",
            refusal.as_str(),
        );
    }
}

#[tokio::test]
async fn every_locally_knowable_refusal_discards_the_candidate() {
    // The converse set. Each condition is a property of THIS published Router's
    // config or resolved table -- both fixed within an incarnation -- so its
    // correction publishes a new generation (which clears the list) or arrives
    // as new traffic (which records a fresh candidate). Requeueing would
    // re-refuse on every pass forever.
    let cases: Vec<(&str, Router)> = vec![
        ("stale incarnation", Fixture::default().router()),
        (
            "unattributable",
            Fixture::default().loopback_entry().router(),
        ),
        (
            "no seat",
            Fixture::default().without_resolved_model().router(),
        ),
        ("cap zero", Fixture::default().cap(0).router()),
        (
            "no profile",
            Fixture::default()
                .effective_row(EffectiveRow::Missing)
                .router(),
        ),
    ];

    for (name, router) in cases {
        if name == "stale incarnation" {
            seed_candidate_at(&router, &key(), router.probe_incarnation() + 1);
        } else {
            seed_candidate(&router, &key());
        }

        let outcome = router.authorize_paid_probe().await;

        let refusal = outcome.refusal().expect("each case must refuse");
        assert!(
            !refusal.requeues(),
            "{name} ({}) must not requeue",
            refusal.as_str(),
        );
        assert!(
            router.all_recorded_paid_candidates_for_tests().is_empty(),
            "{name}: the candidate must be discarded, not held for a retry \
             that can only re-refuse",
        );
    }
}

#[tokio::test]
async fn a_requeue_at_the_list_bound_is_refused_and_counted() {
    // A requeue is a WRITE to the same bounded list, so it obeys the same depth
    // ceiling the original recording does -- and the refusal is counted on the
    // same snapshot counter rather than dropped silently.
    //
    // Driven through the requeue directly, because the full-list state is not
    // reachable end-to-end: the claim frees a slot before the requeue runs, so
    // only a concurrent recording could take it, and that window cannot be held
    // open from a test. What is under test is the requeue's own bound.
    let router = Fixture::default().router();
    for n in 0..PROBE_QUEUE_DEPTH {
        seed_candidate(&router, &key_n(n));
    }
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        PROBE_QUEUE_DEPTH,
        "premise: the list must be AT its bound",
    );
    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .paid_candidate_capacity_refusals_total,
        0,
        "NEGATIVE control: nothing refused before the bound is breached",
    );

    // A identity NOT already on the list, so the bound is what refuses rather
    // than the duplicate check -- `key()` is `m1`, which the seeding loop above
    // already covers.
    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key_n(PROBE_QUEUE_DEPTH + 1),
        incarnation: router.probe_incarnation(),
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        PROBE_QUEUE_DEPTH,
        "a requeue must never push the list past its depth bound",
    );
    assert_eq!(
        router
            .probe_scheduler_snapshot()
            .paid_candidate_capacity_refusals_total,
        1,
        "and the refused requeue must be counted, not dropped silently",
    );
}

#[tokio::test]
async fn a_requeue_of_an_already_present_identity_adds_no_duplicate() {
    // The identity half of the requeue's rules. A duplicate record for one lane
    // would let two claimers each commit a unit for the same exhausted lane, and
    // no refund exists to undo the second.
    let router = Fixture::default().router();
    seed_candidate(&router, &key());
    let live = router.probe_incarnation();

    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key(),
        incarnation: live,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });

    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "one identity at one incarnation may hold at most one record",
    );
    // POSITIVE control: a DIFFERENT identity on the same list is accepted, so
    // the refusal above is about the identity rather than about any requeue
    // being dropped.
    router.requeue_paid_probe_candidate_for_tests(PaidProbeCandidate {
        key: key_n(9),
        incarnation: live,
        validator: ProbeValidator::PaidCompletion,
        payload: payload(),
    });
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        2,
        "a distinct identity must still be requeueable",
    );
}

// ---------------------------------------------------------------------------
// The shared concurrency slot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_authorization_holds_a_paid_slot_that_free_leases_see() {
    // The slot travels INSIDE the authorization, so it is still held after
    // `authorize_paid_probe` returns -- the concurrency the ceiling bounds is
    // the CALL, which has not happened yet.
    let router = Fixture::default().router();
    seed_candidate(&router, &key());

    let auth = match router.authorize_paid_probe().await {
        PaidProbeOutcome::Authorized(auth) => auth,
        other => panic!("expected an authorization: {other:?}"),
    };

    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        1,
        "the held paid slot must be visible in the ONE in_flight reading free \
         leases refuse at",
    );

    drop(auth);

    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "dropping the authorization must release the slot",
    );
}

#[tokio::test]
async fn a_saturated_concurrency_pool_refuses_without_asking_the_ledger() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());
    // Hold every background slot from the PAID side, which is the only side a
    // test can saturate without a live free worker.
    let held: Vec<_> = (0..PROBE_MAX_CONCURRENCY)
        .map(|n| {
            router
                .probe_scheduler
                .try_acquire_paid_slot()
                .unwrap_or_else(|| panic!("slot {n} must be available"))
        })
        .collect();

    let outcome = router.authorize_paid_probe().await;

    assert_eq!(outcome.refusal(), Some(PaidProbeRefusal::NoConcurrencySlot));
    assert_eq!(
        ledger.calls(),
        0,
        "a unit committed for a call that cannot run yet would be spent for \
         nothing, and no refund exists",
    );
    assert_eq!(
        router.all_recorded_paid_candidates_for_tests().len(),
        1,
        "a saturated pool empties without a publication, so the candidate is \
         held for a later pass",
    );

    drop(held);

    // And the retry now succeeds, which proves the refusal was about load.
    let outcome = router.authorize_paid_probe().await;
    assert!(matches!(outcome, PaidProbeOutcome::Authorized(_)));
    assert_eq!(ledger.calls(), 1);
}

#[tokio::test]
async fn a_cancelled_authorization_releases_the_slot_and_never_refunds() {
    // Cancellation in the shape production produces it: the paid stage runs
    // inside a timeout that DROPS the future. The slot comes back; the committed
    // unit does not -- there is no release method on the ledger to give it back
    // with, which is the design.
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());

    let expired = tokio::time::timeout(std::time::Duration::from_millis(1), async {
        let _auth = router.authorize_paid_probe().await;
        std::future::pending::<()>().await;
    })
    .await;

    assert!(expired.is_err(), "premise: the future must be cancelled");
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "a cancelled paid stage must release its slot",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "the unit was committed, and cancellation neither un-commits nor \
         re-reserves it",
    );
}

// ---------------------------------------------------------------------------
// A commit is final
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_later_reload_and_cap_lowering_do_not_invalidate_a_committed_authorization() {
    // The unit is spent. Refusing to use the authorization afterwards would pay
    // for nothing, so a commit is final against everything a later generation
    // can change.
    let ledger = CountingLedger::committed();
    let mut router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());
    let auth = match router.authorize_paid_probe().await {
        PaidProbeOutcome::Authorized(auth) => auth,
        other => panic!("expected an authorization: {other:?}"),
    };
    let authorized_incarnation = auth.incarnation();

    // Lower the cap to zero and publish a new incarnation -- the two changes
    // that would refuse a FRESH attempt.
    let mut config = (*router.config).clone();
    config
        .fidelity
        .paid_probe_daily_caps
        .insert(PROVIDER.to_string(), 0);
    router.config = Arc::new(config);
    router.publish_probe_incarnation();

    assert_eq!(
        auth.committed().cap(),
        CAP,
        "the authorization still reports the cap its unit was committed under",
    );
    assert_eq!(
        auth.incarnation(),
        authorized_incarnation,
        "and the incarnation it was authorized at",
    );
    assert_eq!(
        auth.payload(),
        &payload(),
        "and the payload its body would be built from",
    );
    assert_eq!(
        ledger.calls(),
        1,
        "nothing re-reserved and nothing refunded",
    );

    // A FRESH attempt under the new state refuses, which is what makes the
    // assertions above about the COMMITTED value rather than about a router
    // that would authorize anything.
    seed_candidate(&router, &key());
    let fresh = router.authorize_paid_probe().await;
    assert_eq!(fresh.refusal(), Some(PaidProbeRefusal::CapZero));
    assert_eq!(ledger.calls(), 1, "and asked the ledger nothing new");
}

#[tokio::test]
async fn shutdown_drops_every_candidate_without_refunding_a_committed_unit() {
    let ledger = CountingLedger::committed();
    let router = Fixture::default().router_with_ledger(ledger.clone());
    seed_candidate(&router, &key());
    let auth = match router.authorize_paid_probe().await {
        PaidProbeOutcome::Authorized(auth) => auth,
        other => panic!("expected an authorization: {other:?}"),
    };
    seed_candidate(&router, &key_n(7));

    router.shutdown_probe_work();

    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "shutdown clears the candidate list",
    );
    assert_eq!(
        auth.committed().used(),
        2,
        "the committed unit stands; shutdown drops work, not spend",
    );
    drop(auth);
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "and the dropped authorization releases its slot",
    );
    assert_eq!(ledger.calls(), 1);
}

// ---------------------------------------------------------------------------
// Surface guards
// ---------------------------------------------------------------------------

#[test]
fn neither_the_authorization_nor_the_paid_slot_is_clonable() {
    // SOURCE-TEXT guard, because the property is the ABSENCE of an impl and no
    // behavioral test can observe one that does not exist. A cloned
    // authorization would be two dials against one committed unit, permanently,
    // since the ledger has no refund method; a cloned slot would release twice
    // and show capacity that is still held.
    //
    // Checked as "no `Clone` reaches these types by any route" rather than by
    // reading one derive line: the authorization carries NO derive at all now
    // (its `Debug` is hand-written), so a guard keyed on finding a derive line
    // would fail on its own `expect` rather than on the property.
    for (source, ty) in [
        (
            include_str!("paid_probe_authorize.rs"),
            "PaidProbeAuthorization",
        ),
        (
            include_str!("../probe_scheduler/paid_slot.rs"),
            "PaidProbeSlot",
        ),
    ] {
        let (before_struct, _) = source
            .split_once(&format!("pub struct {ty} {{"))
            .unwrap_or_else(|| panic!("{ty} must be declared in its own module"));
        // The derive line, IF the struct carries one. Absence is fine and is the
        // current shape; what must not appear is `Clone` or `Copy` in it.
        let derive = before_struct
            .lines()
            .rev()
            .take_while(|line| !line.trim().is_empty())
            .find(|line| line.trim_start().starts_with("#[derive("));
        if let Some(derive) = derive {
            assert!(
                !derive.contains("Clone"),
                "{ty} must not derive Clone: {derive}",
            );
            assert!(
                !derive.contains("Copy"),
                "{ty} must not derive Copy: {derive}"
            );
        }
        // And no hand-written impl either, which a derive check alone would miss.
        assert!(
            !source.contains(&format!("impl Clone for {ty}")),
            "{ty} must carry no hand-written Clone",
        );
    }
}

#[test]
fn every_refusal_has_a_distinct_log_token_and_a_stated_requeue_decision() {
    // The closed set, enumerated exhaustively so a variant added without a token
    // or without a requeue decision cannot land silently.
    // One entry per NON-accounting variant, plus every reservation outcome the
    // accounting variant can wrap -- because the requeue decision now discriminates
    // INSIDE that variant, so sampling one outcome would leave the rest unpinned.
    let all = vec![
        PaidProbeRefusal::NoCandidate,
        PaidProbeRefusal::StaleIncarnation,
        PaidProbeRefusal::NoSeat,
        PaidProbeRefusal::Unattributable,
        PaidProbeRefusal::CapZero,
        PaidProbeRefusal::NoProfile,
        PaidProbeRefusal::NoConcurrencySlot,
        PaidProbeRefusal::NoLedger,
        PaidProbeRefusal::Superseded,
        PaidProbeRefusal::ReservationTimeout,
        PaidProbeRefusal::Accounting(PaidProbeReservation::CapExhausted),
        PaidProbeRefusal::Accounting(PaidProbeReservation::Overloaded),
        PaidProbeRefusal::Accounting(PaidProbeReservation::WriteFailed),
        PaidProbeRefusal::Accounting(PaidProbeReservation::MalformedState),
        PaidProbeRefusal::Accounting(PaidProbeReservation::Unavailable),
    ];
    for refusal in &all {
        match refusal {
            PaidProbeRefusal::NoCandidate
            | PaidProbeRefusal::StaleIncarnation
            | PaidProbeRefusal::NoSeat
            | PaidProbeRefusal::Unattributable
            | PaidProbeRefusal::CapZero
            | PaidProbeRefusal::NoProfile
            | PaidProbeRefusal::NoConcurrencySlot
            | PaidProbeRefusal::NoLedger
            | PaidProbeRefusal::Superseded
            | PaidProbeRefusal::ReservationTimeout
            | PaidProbeRefusal::Accounting(_) => {}
        }
    }

    // Token distinctness is asserted over the NON-accounting variants plus one
    // accounting outcome: the accounting arm deliberately DELEGATES to the
    // reservation's own token, so its five outcomes carry five different tokens
    // that are the ledger's vocabulary rather than this enum's, and the ledger
    // sidecar already pins their distinctness.
    let own: Vec<&str> = all
        .iter()
        .filter(|r| !matches!(r, PaidProbeRefusal::Accounting(_)))
        .map(PaidProbeRefusal::as_str)
        .collect();
    let distinct: std::collections::BTreeSet<&str> = own.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        own.len(),
        "every refusal needs its own token: {own:?}",
    );
    assert_eq!(
        own.len(),
        10,
        "one token per non-accounting variant: {own:?}",
    );

    // The requeue partition, stated as the exact SETS rather than a count: a
    // count is satisfied by the wrong five.
    let requeued: Vec<&str> = all
        .iter()
        .filter(|r| r.requeues())
        .map(PaidProbeRefusal::as_str)
        .collect();
    assert_eq!(
        requeued,
        vec![
            "no_concurrency_slot",
            "cap_exhausted",
            "overloaded",
            "write_failed",
            "malformed_state",
        ],
        "only load and the four RECOVERABLE accounting outcomes requeue",
    );
    assert!(
        !PaidProbeRefusal::Accounting(PaidProbeReservation::Unavailable).requeues(),
        "an installed ledger's Unavailable is terminal for this process: no          accounting is reachable, and neither an absent installation nor a          shutdown reverses, so a requeue could only be re-refused forever",
    );
    assert!(
        !PaidProbeRefusal::NoLedger.requeues(),
        "and so is having no ledger installed at all",
    );
    assert!(
        !PaidProbeRefusal::Superseded.requeues(),
        "a superseded attempt names router state that no longer serves, and the \
         list it would return to has already been cleared",
    );
    assert!(
        !PaidProbeRefusal::ReservationTimeout.requeues(),
        "and a timed-out reservation may yet commit, so a retry could pair a \
         second unit with the first",
    );
}

#[test]
fn no_refusal_permits_a_paid_call_through_the_reservation_predicate() {
    // The ledger's own predicate stays authoritative: every accounting refusal
    // this stage wraps is one that permits nothing.
    for reservation in [
        PaidProbeReservation::CapExhausted,
        PaidProbeReservation::MalformedState,
        PaidProbeReservation::WriteFailed,
        PaidProbeReservation::Overloaded,
        PaidProbeReservation::Unavailable,
    ] {
        assert!(
            !reservation.permits_paid_call(),
            "{} must permit nothing",
            reservation.as_str(),
        );
    }
    assert!(
        PaidProbeReservation::Committed { used: 1, cap: 1 }.permits_paid_call(),
        "and only a commit does",
    );
}
