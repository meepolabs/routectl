// Hostile paid-claim verification: 512 contenders, one candidate, one
// never-refunded reservation.
//
// An `include!`d FRAGMENT of `paid_probe_authorize_tests.rs`, not a module of its
// own: the host's imports and fixture helpers stay in scope and every test here
// keeps its original fully qualified name. Carries no top-level `use` for that
// reason.
//
// # Why 512 rather than the sixteen the sibling claim test uses
//
// The claim is a `pop_front` under one lock, and a claim that leaked would cost a
// permanently overspent unit -- the ledger has no refund. That class of race is
// thread-count sensitive in a measured way: the workspace's own gate rules record a
// counter race that came back 20/20 green at 128 threads and 7/20 red at 512, so a
// contention count at or below core count is not evidence about it. This drives
// 512 concurrent claimers well above any core count, several times over, against
// one candidate.
//
// # Why the contenders are gathered at a BARRIER
//
// Spawning 512 tasks does not make them contend: the first can finish before the
// last is scheduled, and then the test measures nothing. Every contender waits on
// one barrier and is released together, so the claim is genuinely contended. The
// ledger is also SLOW, so the winner is still inside its reservation window while
// the losers are claiming -- which is the interleaving a claim spanning an await
// would lose to.
//
// # What is deliberately NOT here
//
// Any process-signal test. A claim leak is observable as a reservation count above
// one, which is a value this test reads directly; reaching for a signal, a timing
// proxy, or a panic hook would be a weaker observation of the same fact.

/// How many contenders race one candidate.
///
/// Well above any core count, and the same figure the workspace's shared-state gate
/// rule names for a hostile run.
const HOSTILE_CLAIMERS: usize = 512;

/// How many times the whole race is repeated.
///
/// A single pass of a concurrency test is one sample of a distribution. Repeating
/// is what turns "it happened to be fine" into evidence, and the measured
/// precedent this figure answers to came back 7/20 red -- so a handful of rounds is
/// enough to see it while keeping the suite's runtime honest.
const HOSTILE_ROUNDS: usize = 8;

/// A ledger that commits every time it is asked, counts the asks, and is SLOW
/// enough that a winner is still inside its reservation window while the losers
/// claim.
///
/// The delay is the whole reason this is not `CountingLedger`: with an instant
/// double the winner can finish before the second contender reaches the claim, and
/// a claim that spanned the reservation await would still pass. The delay holds the
/// window open so that shape loses.
struct SlowCommittingLedger {
    calls: AtomicUsize,
}

impl SlowCommittingLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl PaidProbeLedger for SlowCommittingLedger {
    async fn reserve_paid_probe_unit(&self, _: &str, cap: u32) -> PaidProbeReservation {
        // Counted BEFORE the sleep: what this test reads is how many callers
        // REACHED the ledger, and counting after would under-report a second caller
        // that was cancelled mid-window.
        self.calls.fetch_add(1, Ordering::AcqRel);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        PaidProbeReservation::Committed { used: 1, cap }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_hundred_twelve_contenders_for_one_candidate_produce_exactly_one_reservation() {
    // THE HOSTILE CLAIM PROOF. One candidate, one never-refunded reservation,
    // 512 contenders released together, repeated.
    //
    // THREE assertions per round, and all three are needed:
    //   - exactly ONE contender authorizes. A second authorization is a second dial
    //     against one committed unit.
    //   - exactly ONE reservation was committed. This is the money assertion: a
    //     claim leak shows up here even if the second authorization were somehow
    //     discarded downstream, because the unit is spent the moment the ledger
    //     answers and there is no refund.
    //   - every loser refuses with `NoCandidate`. A loser refusing for a
    //     PRECONDITION would mean the fixture was inadmissible for some unrelated
    //     reason, and then "one authorization" would be true of a router that could
    //     authorize nothing at all.
    //
    // Mutation check, MEASURED rather than asserted: replacing the claim's atomic
    // `pop_front` with a read-then-remove pair (`front().cloned()`, release, then
    // `pop_front()` under a second acquisition) made these two tests fail in 5 of 8
    // runs at `--test-threads=512`. Partial detection is the honest result for a
    // real race and it is stated rather than rounded up -- the pin is probabilistic,
    // so a single green run of these tests is not proof that the claim is atomic.
    // What makes the atomicity itself trustworthy is that it is one lock acquisition
    // by construction; these tests are the backstop that notices when it stops being
    // one.
    for round in 1..=HOSTILE_ROUNDS {
        let ledger = SlowCommittingLedger::new();
        let router = Arc::new(
            Fixture::default()
                .bare_router()
                .with_paid_probe_ledger(ledger.clone()),
        );
        seed_candidate(&router, &key());
        assert_eq!(
            router.all_recorded_paid_candidates_for_tests().len(),
            1,
            "round {round} premise: exactly ONE candidate is on the list, so exactly \
             one reservation is the correct answer",
        );

        // The barrier gathers every contender before any of them claims. An async
        // barrier rather than a blocking one, because these are tasks on a runtime:
        // a blocking barrier would park worker threads and could deadlock at a count
        // far above the pool size.
        let gate = Arc::new(tokio::sync::Barrier::new(HOSTILE_CLAIMERS));
        let contenders: Vec<_> = (0..HOSTILE_CLAIMERS)
            .map(|_| {
                let router = Arc::clone(&router);
                let gate = Arc::clone(&gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    router.authorize_paid_probe().await
                })
            })
            .collect();

        let mut authorized = 0usize;
        let mut refusals: Vec<PaidProbeRefusal> = Vec::with_capacity(HOSTILE_CLAIMERS);
        for contender in contenders {
            match contender.await.expect("no contender may panic") {
                PaidProbeOutcome::Authorized(_) => authorized += 1,
                PaidProbeOutcome::Refused(refusal) => refusals.push(refusal),
            }
        }

        assert_eq!(
            authorized, 1,
            "round {round}: exactly one of {HOSTILE_CLAIMERS} contenders may \
             authorize a paid call for one candidate",
        );
        assert_eq!(
            ledger.calls(),
            1,
            "round {round}: one candidate must reach the ledger exactly ONCE, \
             however {HOSTILE_CLAIMERS} contenders interleave. A committed unit is \
             never refunded, so a second reservation here is permanent overspend",
        );
        // Every loser claimed NOTHING, which is what `NoCandidate` means. Asserted
        // as a set rather than per element so the failure names the unexpected
        // refusal rather than an index.
        let distinct: std::collections::BTreeSet<&'static str> =
            refusals.iter().map(PaidProbeRefusal::as_str).collect();
        assert_eq!(
            distinct,
            std::iter::once(PaidProbeRefusal::NoCandidate.as_str()).collect(),
            "round {round}: every loser must refuse for having claimed nothing, not \
             for a precondition -- a precondition refusal would mean the fixture was \
             inadmissible and the single authorization above proved nothing",
        );
        assert_eq!(
            refusals.len(),
            HOSTILE_CLAIMERS - 1,
            "round {round}: every contender must be accounted for",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hostile_contention_never_leaves_the_candidate_on_the_list() {
    // The other half of the claim's contract, and it is not implied by the count
    // above: a claim is a REMOVAL, so after a contended pass the candidate must be
    // gone. A build that reserved once but left the record resident would let the
    // NEXT pass claim it again and commit a second unit -- the same overspend one
    // tick later, which a single-pass reservation count cannot see.
    //
    // The authorization is DROPPED before the list is read, so the check is about
    // the claim rather than about a candidate an outstanding authorization is
    // holding: `PaidProbeRefusal::requeues` is `false` for a commit, so nothing
    // legitimately puts it back.
    let ledger = SlowCommittingLedger::new();
    let router = Arc::new(
        Fixture::default()
            .bare_router()
            .with_paid_probe_ledger(ledger.clone()),
    );
    seed_candidate(&router, &key());

    let gate = Arc::new(tokio::sync::Barrier::new(HOSTILE_CLAIMERS));
    let contenders: Vec<_> = (0..HOSTILE_CLAIMERS)
        .map(|_| {
            let router = Arc::clone(&router);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                gate.wait().await;
                // The outcome is DROPPED inside the task: an authorization held
                // across the assertion would keep a concurrency slot, and the list
                // read below would be about a pass still in flight.
                matches!(
                    router.authorize_paid_probe().await,
                    PaidProbeOutcome::Authorized(_)
                )
            })
        })
        .collect();
    let mut authorized = 0usize;
    for contender in contenders {
        if contender.await.expect("no contender may panic") {
            authorized += 1;
        }
    }

    assert_eq!(authorized, 1, "premise: exactly one contender authorized");
    assert_eq!(ledger.calls(), 1, "premise: exactly one reservation");
    assert!(
        router.all_recorded_paid_candidates_for_tests().is_empty(),
        "a committed candidate must be OFF the list: a resident record would be \
         re-claimed on the next pass and commit a second never-refunded unit. \
         Resident: {:?}",
        router.all_recorded_paid_candidates_for_tests().len(),
    );
    assert_eq!(
        router.probe_scheduler_snapshot().in_flight,
        0,
        "and no concurrency slot may be stranded: every authorization was dropped, \
         so every slot it held is released",
    );
}
