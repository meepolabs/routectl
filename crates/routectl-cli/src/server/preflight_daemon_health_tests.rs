// The LIVE acknowledgment path: eligibility advancing within one process off a
// durable ledger row, health-endpoint persistence, and the confirmation/
// incarnation guards around it. `include!`d into preflight_daemon_tests.rs;
// the fixtures and imports live there, so do not add `use` lines here.

// ---------------------------------------------------------------------------
// The LIVE acknowledgment: eligibility advances within ONE process
// ---------------------------------------------------------------------------

/// Plant a resident ACTING verdict with ZERO acknowledged confirmations.
///
/// The ONE state the acknowledgment is observable from. Every other pre-flight
/// fixture in this file plants confirmations too, which makes the verdict eligible
/// before any event is written and leaves the acknowledgment doing nothing an
/// assertion could see.
///
/// Planted through the shared seam at `confirmations = 0`, which is what the canary
/// registry holds for a verdict nobody has acknowledged yet -- the state a reactive
/// repair leaves behind the moment it mints one.
fn plant_unacknowledged_verdict(router: &Router) {
    plant_acting_field_verdict_for_tests(router, STATE_KEY, FIELD_PATH, 0);
}

/// The `CapabilityLearnEvent` a reactive repair's commit rides out on, stamped with
/// the LIVE generation and the resident incarnation.
///
/// The stamps are read from the registry rather than written here, because the
/// acknowledgment validates both against live state: a hardcoded pair would produce
/// a refusal indistinguishable from the refusal a broken acknowledgment produces.
fn earned_learn_event(
    router: &Router,
    observations: u32,
) -> routectl_router::router::CapabilityLearnEvent {
    let (generation, incarnation) =
        routectl_router::field_verdict_event_stamps_for_tests(router, STATE_KEY, FIELD_PATH);
    routectl_router::router::CapabilityLearnEvent {
        persistence_generation: generation,
        incarnation,
        state_key: STATE_KEY.to_string(),
        capability_key: field_key(),
        provider_kind: "anthropic-api".to_string(),
        signal_tier: routectl_core::capability::SignalTier::SelfIdentifying,
        observations,
        upstream_status: 400,
        remapped: false,
        request_features: Vec::new(),
        phase: routectl_core::capability::FailurePhase::F1,
        source: routectl_core::capability::EvidenceSource::Live,
    }
}

/// Every `capability_events` row for this test's identity, read from the daemon's own
/// ledger FILE through a connection the writer does not own.
///
/// Read from the file rather than from a counter, because what the design requires is that
/// the row is on disk: a counter says the writer believed it succeeded, and the
/// acknowledgment's whole purpose is to rest on something stronger than a belief.
fn field_ledger_rows(db_path: &std::path::Path) -> Vec<String> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare("SELECT capability FROM capability_events WHERE lane_key = ?1 ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([STATE_KEY], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("rows")
        .into_iter()
        .filter(|capability| *capability == field_key())
        .collect()
}

/// Drive the production acknowledged drain against the DAEMON's own tracker and wait
/// for the advancement it spawns.
///
/// The wait is the test's, not the daemon's: the drain hands the row to a daemon-owned
/// task and returns immediately -- which is the behavior under test -- so a test that
/// asserted straight after it would be racing the task it just started. Waiting on the
/// tracker's own in-flight count is the exact observation, and it is bounded so a
/// stalled advancement fails rather than hangs.
async fn advance_through_the_daemon(
    daemon: &Daemon,
    live: &Arc<Router>,
    meta: &routectl_router::DispatchMeta,
) {
    let tracker = daemon.confirmations();
    crate::handlers::capability_ack_drain::acknowledge_field_confirmations(
        live,
        daemon.usage(),
        tracker,
        meta,
        live.catalog_version(),
        live.overlay_revision(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while tracker.in_flight() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon-owned confirmation advancement did not finish",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// THE LIVE-ACKNOWLEDGMENT PROOF: a verdict with no acknowledged confirmation
/// becomes pre-flight eligible the moment its event write is acknowledged, and the
/// NEXT request in the SAME running process is rewritten. No restart.
///
/// # What this establishes that nothing else does
///
/// Before this change the only production writer of the acknowledged confirmation
/// count was the cold-rebuild seed, so a verdict a reactive repair minted became
/// pre-flight eligible only after a RESTART. Here the verdict starts with ZERO
/// acknowledged confirmations on a running daemon -- request one proves it does not
/// pre-flight -- and the only thing that happens between the two requests is the
/// production drain writing its event row and acknowledging it.
///
/// # Why the learn event is constructed rather than earned from the rejection
///
/// It is not for want of trying, and the reason is a deliberate property of this
/// build rather than a gap in the test: the envelope-to-path rejection parser is
/// PRODUCTION-INERT by design (the governing decision says so in as many words --
/// pre-flight acts only on verdicts minted by grounded reactive or probe evidence,
/// and no grounded parser has landed). So no HTTP rejection, however shaped, can
/// mint a field verdict on a release build, and a test that posted one would be
/// asserting against the inert parser rather than against the acknowledgment.
///
/// What is NOT substituted is the thing under test. The event is handed to the REAL
/// production drain (`UsageCapture::acknowledge_field_confirmations`, the same call
/// the three ingress walks make), which writes through the REAL usage writer to the
/// daemon's REAL ledger file and acknowledges the REAL commit; the advance goes
/// through the REAL registry entry point, which re-validates generation and
/// incarnation. Only the producer of the event is stood in for -- and that producer
/// is inert in this build by design.
///
/// # The assertions, and what each rules out
///
/// - REQUEST ONE carries the field: a verdict at zero acknowledged confirmations
///   does not pre-flight. Rules out a fixture that was eligible all along, which
///   would make the second request's rewrite prove nothing.
/// - The ledger ROW is on disk. Rules out an acknowledgment wired to the in-memory
///   mutation: a build that advanced on `GenerationOutcome::Applied` would pass the
///   byte assertions with the row dropped.
/// - REQUEST TWO adds exactly ONE attempt with the field already gone. That is a
///   PRE-FLIGHT rewrite: a reactive repair would add TWO (carried, then repaired),
///   because it acts only after a rejection. And it happened with no reload between
///   the two requests.
///
/// Mutation checks, each turning this test red by name:
///   - remove the `router.acknowledge_durable_field_confirmation` call from
///     `capability_ack_drain` -> request two still carries the field;
///   - acknowledge BEFORE the write (move the call above the `await_outcome`) -> the
///     `is_durable` guard no longer gates it, so a failing writer would advance;
///     pinned as its own test below, since a healthy writer cannot show it;
///   - advance the count from the in-memory mutation instead (call the registry
///     entry point from `FieldRepairGuard::commit`) -> request one is ALREADY
///     rewritten and the zero-confirmation premise reds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unacknowledged_verdict_preflights_the_next_request_once_its_write_is_acknowledged() {
    let (config, dir) = daemon_config(None);
    let db_path = config.usage.db_path.clone();
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_unacknowledged_verdict(&router);
    let daemon = spawn_daemon(config, dir, router).await;

    // REQUEST ONE: the verdict is resident and acting, but nothing has acknowledged
    // a confirmation for it, so pre-flight refuses and the field goes out.
    let first = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(first, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "premise: at ZERO acknowledged confirmations the verdict does not \
         pre-flight, so the field reaches the provider. Without this the rewrite \
         below would prove nothing -- an already-eligible verdict rewrites either \
         way",
    );
    assert!(
        field_ledger_rows(&db_path).is_empty(),
        "premise: no event row for this identity exists yet",
    );

    // THE ACKNOWLEDGMENT, through the production drain on the daemon's LIVE router
    // and its own usage handle -- the same call the ingress walks make.
    let live = daemon.live_router();
    let mut meta = routectl_router::DispatchMeta::for_alias(ALIAS);
    meta.learned_capabilities.push(earned_learn_event(&live, 1));
    advance_through_the_daemon(&daemon, &live, &meta).await;

    // THE DURABLE ROW, on disk in the daemon's own ledger.
    assert_eq!(
        field_ledger_rows(&db_path),
        vec![field_key()],
        "the acknowledged path wrote exactly ONE row for this identity, and it is \
         readable from the file -- which is what the advance rests on",
    );

    // REQUEST TWO: same process, same router swap, same registry, no reload.
    let second = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(second, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true, false],
        "the SECOND request added exactly ONE attempt with the field already gone: \
         that is a PRE-FLIGHT rewrite, in the same process, with no restart. A \
         reactive repair would have added TWO attempts (carried, then repaired), so \
         the shape of this log is what distinguishes the two",
    );
    assert_eq!(
        recorder.calls(),
        2,
        "two dispatches in total: one unrewritten, one pre-flighted",
    );

    daemon.shutdown().await;
}

/// The NO-ACK-NO-ADVANCE half: when the row is never written, eligibility does not
/// move and the next request still carries the field.
///
/// This is the mutation the healthy daemon above cannot catch. An acknowledgment
/// wired BEFORE the write, or one that ignored the durable outcome, is
/// indistinguishable from a correct one while the writer is healthy -- both advance.
/// Only a write that does NOT land separates them.
///
/// # Why capture-disabled is the failure used here
///
/// It is a real production state, reached by one operator setting, and the
/// acknowledged admission refuses it by design (an operator who turned capture off
/// wrote no row, so no verdict may become eligible on the strength of one).
///
/// The obvious alternative does not work, and the reason is worth recording: making
/// the ledger FILE unwritable after boot does not fail the writer's inserts at all.
/// SQLite holds the open descriptor, so a removed or replaced file leaves the writer
/// happily appending to the original inode -- measured, and it made an earlier
/// version of this test report a rewrite while claiming the write had failed. A
/// filesystem trick that the writer does not observe is not a failure injection.
///
/// Mutation checks, each turning THIS test red:
///   - acknowledge before awaiting the write's outcome -> the advance happens and
///     the request below is rewritten;
///   - drop the `is_durable()` guard in `capability_ack_drain` -> same;
///   - drop the enabled check from `admit_acknowledged_capability_event` -> the row
///     is queued and the advance follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unwritten_event_advances_nothing_and_the_next_request_still_carries_the_field() {
    let (config, dir) = daemon_config(None);
    let db_path = config.usage.db_path.clone();
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_unacknowledged_verdict(&router);
    let daemon = spawn_daemon(config, dir, router).await;

    // Capture OFF: the acknowledged admission refuses, so no row is written and
    // nothing can be reported durable.
    daemon.usage().set_enabled(false);
    assert!(
        !daemon.usage().is_enabled(),
        "premise: capture must be off, or the write would land",
    );

    let live = daemon.live_router();
    let mut meta = routectl_router::DispatchMeta::for_alias(ALIAS);
    meta.learned_capabilities.push(earned_learn_event(&live, 1));
    advance_through_the_daemon(&daemon, &live, &meta).await;

    // PREMISE: nothing was written. Asserted against the ledger FILE, so the test
    // is about an absent row rather than about an outcome value.
    assert!(
        field_ledger_rows(&db_path).is_empty(),
        "premise: no row for this identity landed, which is what makes the refusal \
         below about the acknowledgment rather than about the incarnation",
    );

    // The request after the refused write: still carries the field. The write was
    // not acknowledged, so no confirmation was recorded and pre-flight has no
    // grounds to act.
    let after = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(after, reqwest::StatusCode::OK, "the request still serves");
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "a write that did not land advances nothing: the request carries the field \
         exactly as it did before, and the lane is still fully served by reactive \
         forward-and-repair",
    );

    daemon.shutdown().await;
}

/// A MISMATCHED-incarnation acknowledgment advances nothing: an event whose
/// incarnation is not the resident row's is refused even though its row lands
/// durably.
///
/// This is the generation/incarnation half of the rule, and it is separable from the
/// durability half: the write here SUCCEEDS, so a build that advanced on a
/// successful write alone would pass the durable tests above and fail this one. What
/// it rules out is an acknowledgment that credits whatever lifecycle is resident now
/// rather than the one whose row committed.
///
/// Mutation check: delete the incarnation comparison from
/// `FieldVerdictRegistry::acknowledge_durable_confirmation` -> the stale event
/// advances the count and the request below is rewritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mismatched_incarnation_acknowledgment_advances_nothing_even_when_its_row_lands() {
    let (config, dir) = daemon_config(None);
    let db_path = config.usage.db_path.clone();
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_unacknowledged_verdict(&router);
    let daemon = spawn_daemon(config, dir, router).await;

    let live = daemon.live_router();
    // A MISMATCHED incarnation, above the resident row's rather than below it. Both
    // directions must be refused and this is the one a fresh fixture can express: a
    // learned incarnation starts at zero, so there is no lower value to plant.
    //
    // The high side is not the lesser case either -- it is the one that would
    // RESEED the canary state if it were admitted, dropping a live cadence and a
    // live claim on the strength of an event whose own mutation this registry has
    // never seen.
    let mut stale = earned_learn_event(&live, 1);
    stale.incarnation = stale
        .incarnation
        .checked_add(1)
        .expect("an incarnation one above the resident one");
    let mut meta = routectl_router::DispatchMeta::for_alias(ALIAS);
    meta.learned_capabilities.push(stale);
    advance_through_the_daemon(&daemon, &live, &meta).await;

    // PREMISE: the row DID land. That is what makes this test about the incarnation
    // rather than about the write.
    assert_eq!(
        field_ledger_rows(&db_path),
        vec![field_key()],
        "premise: the stale event's row landed durably, so a build that advanced on \
         a successful write alone would advance here",
    );

    let after = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(after, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![true],
        "a durable write for a DIFFERENT lifecycle advances nothing: crediting the \
         resident verdict with another incarnation's evidence is exactly what the \
         incarnation check refuses",
    );

    daemon.shutdown().await;
}

/// A refused capability BATCH -- the class a purge, a canary's durable clear, and the
/// boot tombstone all use -- suspends pre-flight on a running daemon, and the next
/// request dispatches the client's own bytes.
///
/// # What this covers that the wiring sidecar cannot
///
/// `server::capability_health`'s tests assert that such a refusal turns the HEALTH
/// READ false. This asserts the consequence the operator actually experiences: the
/// daemon's own live router stops rewriting requests, observed on the bytes a
/// provider received. A gate that reported unhealthy while the planner went on
/// rewriting would pass every test in that sidecar.
///
/// The verdict is planted ACKNOWLEDGED and proven to pre-flight FIRST, so the
/// suspension below is attributable to the refusal rather than to a fixture that
/// could never rewrite. That ordering is the whole design of this test: the same
/// daemon, the same verdict, the same request body, one thing changed between the
/// two requests.
///
/// The refusal is produced by saturating the writer's channel with batches and
/// letting one be refused -- a real admission refusal on the daemon's own handle,
/// not a fabricated counter.
///
/// Mutation checks, each turning THIS test red:
///   - delete the `note_capability_refusal` call from `admit_capability_batch_at`
///     (the hole this change closes) -> the second request is still rewritten;
///   - drop the `capability_events_dropped_full` or `capability_writer_unavailable`
///     term from the health predicate -> same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_capability_batch_suspends_preflight_and_the_next_request_forwards_unchanged() {
    let (config, dir) = daemon_config(None);
    let recorder = Arc::new(Recorder::default());
    let router = router_with(&config, Arc::clone(&recorder));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);
    let daemon = spawn_daemon(config, dir, router).await;

    // PREMISE, proven rather than assumed: this daemon DOES pre-flight right now.
    let first = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(first, reqwest::StatusCode::OK);
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![false],
        "premise: the verdict is acknowledged and the writer healthy, so this request \
         is rewritten -- without that the suspension below would prove nothing",
    );
    assert_eq!(
        daemon.usage().counters().capability_events_dropped_full()
            + daemon.usage().counters().capability_writer_unavailable(),
        0,
        "premise: no capability write has been refused yet",
    );

    // Saturate the daemon's OWN writer channel with acknowledged batches until one is
    // refused. Every receipt is held, so nothing drains: the writer is processing one
    // message while the rest fill the channel.
    //
    // Bounded by the channel capacity plus a margin, and ASSERTED to have actually
    // produced a refusal -- a loop that silently failed to saturate would leave this
    // test asserting about a healthy daemon.
    let mut held = Vec::new();
    for _ in 0..(routectl_usage::CHANNEL_CAPACITY * 2 + 8) {
        match daemon.usage().admit_capability_batch(
            vec![routectl_usage::CapabilityEvent::tombstone(900, 8, 1)],
            1,
        ) {
            Ok(receipt) => held.push(receipt),
            Err(_) => break,
        }
    }
    let refused = daemon.usage().counters().capability_events_dropped_full()
        + daemon.usage().counters().capability_writer_unavailable();
    assert!(
        refused >= 1,
        "premise: the channel must actually have refused a capability batch, or this \
         test is about a healthy daemon",
    );

    // THE CONSEQUENCE, on the daemon's own live router: the next request is NOT
    // rewritten. Nothing about the verdict changed -- it is still resident, still
    // acknowledged, still acting.
    let second = post_inference(&daemon, &body_with_field()).await;
    assert_eq!(
        second,
        reqwest::StatusCode::OK,
        "the request still serves: reactive forward-and-repair is unaffected",
    );
    assert_eq!(
        recorder.carried_per_attempt(),
        vec![false, true],
        "the SECOND request dispatched the client's OWN bytes: a capability write \
         that could not be admitted means a wrong verdict could not be durably \
         retracted, so pre-flight stands down while the same verdict stays resident",
    );
    assert!(
        !daemon.live_router().capability_writes_durable_for_tests(),
        "and the daemon's live router reports the writer unhealthy, which is what \
         the planner refused on",
    );

    drop(held);
    daemon.shutdown().await;
}
