// Lifecycle and health-reporting cases: what a reservation may answer once the
// subsystem is going away, and what a successful reservation must NOT claim
// about the rest of the writer.

/// Admission after shutdown has begun authorizes nothing, and does not even
/// queue -- the channel stays open while a handle holds a sender clone, so the
/// channel alone cannot express this.
#[tokio::test]
async fn admission_after_shutdown_begins_is_refused() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Assert the premise: this handle CAN reserve before shutdown, so the
    // refusal below is the lifecycle and not a broken fixture.
    assert!(matches!(
        reserve_through(&handle, "anthropic", 5).await,
        PaidProbeCommit::Committed { .. }
    ));

    // Act: shut the writer down while the handle stays alive, exactly as a
    // daemon teardown leaves it.
    writer.shutdown();
    let admission = handle.admit_paid_probe_reservation("anthropic", 5);

    // Assert
    assert!(
        matches!(admission, PaidProbeAdmission::Unavailable),
        "no reservation may be admitted once shutdown has begun",
    );
}

/// The race that the admission check alone cannot close: a reservation ALREADY
/// QUEUED behind a blocked writer when shutdown begins must not yield
/// `Committed`.
///
/// Deterministic by construction, with no timing: the writer is blocked on a
/// held exclusive lock, so the queued reservation provably has not run when
/// shutdown is signalled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_queued_when_shutdown_begins_cannot_yield_committed() {
    // Arrange: a writer whose first message will block on the DB write lock a
    // sibling connection holds, so everything behind it is provably unprocessed.
    let (_dir, path) = temp_path();
    let blocker = crate::db::open(&path).expect("migrating open").into_conn();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("hold the write lock");

    // Act: queue a reservation (admitted -- shutdown has not begun), confirm it
    // is stuck, then begin shutdown and only then release the writer.
    let admission = handle.admit_paid_probe_reservation("anthropic", 5);
    let receipt = match admission {
        PaidProbeAdmission::Admitted(receipt) => receipt,
        _ => panic!("a running writer must admit"),
    };

    // Assert the premise: the reservation is genuinely still queued. Nothing is
    // on disk, because the writer cannot get the lock.
    assert!(
        reservation_keys(&path).is_empty(),
        "the premise requires the queued reservation to be unprocessed",
    );

    // Watch for the EXACT moment shutdown marks the gate terminal. The signal is
    // emitted inside that critical section, immediately after the state change,
    // so receiving it is proof the transition has happened -- unlike a yield or a
    // sleep, which only proves time passed.
    let terminal = writer.lifecycle_for_tests().watch_terminal_for_tests();
    let shutdown = tokio::task::spawn_blocking(move || writer.shutdown());
    terminal
        .recv_timeout(Duration::from_secs(10))
        .expect("shutdown must mark the gate terminal");

    // Only now release the writer, so the queued reservation is provably
    // processed AFTER terminal was set.
    blocker.execute_batch("ROLLBACK").expect("release the lock");
    drop(blocker);

    let outcome = receipt.await_outcome().await;
    shutdown.await.expect("shutdown task");

    // Assert: whichever way the race fell, the answer never authorizes a call.
    assert_ne!(
        std::mem::discriminant(&outcome),
        std::mem::discriminant(&PaidProbeCommit::Committed { used: 1, cap: 5 }),
        "a reservation racing shutdown must never authorize a paid call: {outcome:?}",
    );
    drop(handle);
}

/// The commit-raced-shutdown case stated directly: when the transaction lands
/// and shutdown has begun by the time it returns, the unit is CONSUMED and the
/// caller is NOT authorized. No refund, no rollback.
///
/// Driven at the writer-state seam rather than through the channel, because that
/// is the only way to place the shutdown transition exactly between the commit
/// and the acknowledgement -- through the channel it would be a timing accident.
#[tokio::test]
async fn a_commit_that_races_shutdown_consumes_the_unit_without_authorizing() {
    // Arrange
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let shutdown = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    // Act: shutdown begins AFTER the transaction would commit. The seam sets the
    // flag from inside, between the primitive returning and the acknowledgement.
    let shutdown_mid = shutdown.clone();
    crate::paid_probe_lifecycle::reserve_and_answer_instrumented(
        &mut state,
        command,
        &counters,
        &shutdown,
        wall_clock_ms(),
        &move || shutdown_mid.begin_for_tests(),
        &|| {},
    );
    let outcome = receipt.await.expect("the writer answered");

    // Assert: the caller is refused ...
    assert_eq!(
        outcome,
        PaidProbeCommit::Unavailable,
        "a commit that raced shutdown must not authorize a paid call",
    );
    // ... and the unit is nonetheless SPENT, because nothing is given back.
    assert_eq!(
        stored_units(&path, day, "anthropic").as_deref(),
        Some("1"),
        "the committed unit stays committed -- there is no refund",
    );
}

/// The converse control for the case above: with no shutdown, the SAME seam
/// authorizes. Without this, a `reserve_paid_probe` that refused unconditionally
/// would satisfy the test above.
#[tokio::test]
async fn the_same_seam_authorizes_when_shutdown_has_not_begun() {
    // Arrange
    let (_dir, path) = temp_path();
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let shutdown = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    // Act
    crate::paid_probe_lifecycle::reserve_and_answer(
        &mut state,
        command,
        &counters,
        &shutdown,
        wall_clock_ms(),
    );
    let outcome = receipt.await.expect("the writer answered");

    // Assert
    assert_eq!(outcome, PaidProbeCommit::Committed { used: 1, cap: 3 });
}

/// SHUTDOWN LOSES when it arrives after the reservation has entered its
/// check-and-send critical section: the caller keeps its authorization, and the
/// writer is still there to honor it because shutdown cannot have begun draining.
///
/// The synchronization is a real rendezvous, not a timing hope. The reservation
/// parks INSIDE the critical section and signals readiness; the test then starts
/// a shutdown on another thread and PROVES it is blocked by observing the lock
/// itself (`is_held_for_tests`) plus the shutdown thread failing to complete
/// within a generous bound. Nothing here treats a yield or a sleep as evidence of
/// ordering -- the sleeps only bound how long the test is willing to wait for a
/// condition it then asserts directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_arriving_inside_the_critical_section_cannot_suppress_the_answer() {
    // Arrange
    let (_dir, path) = temp_path();
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    let inside = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let shutdown_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Act: the reservation parks inside the critical section, having already
    // passed its terminal check.
    let reservation = {
        let inside = Arc::clone(&inside);
        let release = Arc::clone(&release);
        let gate = gate.clone();
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            crate::paid_probe_lifecycle::reserve_and_answer_instrumented(
                &mut state,
                command,
                &counters,
                &gate,
                wall_clock_ms(),
                &|| {},
                &|| {
                    inside.wait();
                    release.wait();
                },
            );
        })
    };

    // Wait until the reservation is provably inside the section.
    inside.wait();

    // Assert the premise DIRECTLY: the gate is held, so any shutdown must block.
    assert!(
        gate.is_held_for_tests(),
        "the reservation must hold the lifecycle gate while inside its critical section",
    );

    // Start shutdown and prove it cannot proceed while the section is held.
    let shutdown_thread = {
        let gate = gate.clone();
        let finished = Arc::clone(&shutdown_finished);
        std::thread::spawn(move || {
            gate.begin_for_tests();
            finished.store(true, std::sync::atomic::Ordering::Release);
        })
    };
    let blocked_deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < blocked_deadline {
        assert!(
            !shutdown_finished.load(std::sync::atomic::Ordering::Acquire),
            "shutdown completed while the reservation held the gate -- the check and the \
             send are not inside one critical section",
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Release the section; shutdown may now proceed.
    release.wait();
    reservation.join().expect("reservation thread");
    shutdown_thread.join().expect("shutdown thread");

    // Assert: the answer formed before shutdown began is the REAL one.
    let outcome = receipt.await.expect("the writer answered");
    assert_eq!(
        outcome,
        PaidProbeCommit::Committed { used: 1, cap: 3 },
        "an answer sent before shutdown began must not be suppressed",
    );
    assert_eq!(
        counters.paid_probe_consumed_unauthorized(),
        0,
        "nothing diverged: the unit was spent AND authorized",
    );
}

/// SHUTDOWN WINS when it completes before the reservation reaches its
/// check-and-send: the unit stays consumed, the caller is refused, and the
/// divergence is counted.
///
/// The mirror image of the test above, and the pair is what makes either
/// meaningful -- one lock order each, both deterministic. Here the reservation
/// parks BEFORE taking the gate, so shutdown provably completes first (asserted
/// by joining that thread, not by waiting a while).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_completing_before_the_critical_section_suppresses_the_answer() {
    // Arrange
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    let parked = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));

    // Act: the reservation parks after its transaction but BEFORE the gate.
    let reservation = {
        let parked = Arc::clone(&parked);
        let resume = Arc::clone(&resume);
        let gate = gate.clone();
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            crate::paid_probe_lifecycle::reserve_and_answer_instrumented(
                &mut state,
                command,
                &counters,
                &gate,
                wall_clock_ms(),
                &|| {
                    parked.wait();
                    resume.wait();
                },
                &|| {},
            );
        })
    };

    // Wait until the transaction has run and the reservation is parked short of
    // the gate.
    parked.wait();

    // Shutdown runs to COMPLETION here -- joined, so there is no question of
    // ordering.
    let gate_for_shutdown = gate.clone();
    std::thread::spawn(move || gate_for_shutdown.begin_for_tests())
        .join()
        .expect("shutdown thread");
    assert!(
        gate.is_terminal_for_tests(),
        "the premise: shutdown must have completed before the section is entered",
    );

    resume.wait();
    reservation.join().expect("reservation thread");

    // Assert: refused ...
    let outcome = receipt.await.expect("the writer answered");
    assert_eq!(
        outcome,
        PaidProbeCommit::Unavailable,
        "an answer formed before shutdown must be suppressed once shutdown has begun",
    );
    // ... the unit is nonetheless SPENT ...
    assert_eq!(
        stored_units(&path, day, "anthropic").as_deref(),
        Some("1"),
        "the committed unit stays committed -- there is no refund",
    );
    // ... and the divergence is counted exactly once.
    assert_eq!(
        counters.paid_probe_consumed_unauthorized(),
        1,
        "a unit consumed without authorization must be counted",
    );
}

/// The divergence counter moves ONLY on post-commit suppression -- not when a
/// reservation is refused before spending, and not on an ordinary commit.
///
/// Without this the counter could be incremented on every refusal and the test
/// above would still pass.
#[tokio::test]
async fn the_divergence_counter_moves_only_on_post_commit_suppression() {
    // Arrange
    let (_dir, path) = temp_path();
    let counters = Arc::new(UsageCounters::default());

    // Act + Assert: an ordinary commit does not move it.
    {
        let mut state = crate::writer::writer_state_for_tests(&path);
        let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
        let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);
        crate::paid_probe_lifecycle::reserve_and_answer(
            &mut state,
            command,
            &counters,
            &gate,
            wall_clock_ms(),
        );
        assert!(matches!(
            receipt.await.expect("answered"),
            PaidProbeCommit::Committed { .. }
        ));
        assert_eq!(counters.paid_probe_consumed_unauthorized(), 0);
    }

    // A refusal BEFORE spending does not move it either -- no unit was consumed.
    {
        let mut state = crate::writer::writer_state_for_tests(&path);
        let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
        gate.begin_for_tests();
        let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);
        crate::paid_probe_lifecycle::reserve_and_answer(
            &mut state,
            command,
            &counters,
            &gate,
            wall_clock_ms(),
        );
        assert_eq!(
            receipt.await.expect("answered"),
            PaidProbeCommit::Unavailable
        );
        assert_eq!(
            counters.paid_probe_consumed_unauthorized(),
            0,
            "a reservation refused before spending consumed nothing",
        );
    }
}

/// The pre-transaction check earns its place separately from the post-commit
/// one: a reservation the writer reaches AFTER shutdown began must not SPEND a
/// unit at all.
///
/// The post-commit check alone would refuse the caller correctly while still
/// burning a unit of a cap nobody can use -- invisible in the answer, visible on
/// disk, and permanent because nothing is given back.
#[tokio::test]
async fn a_reservation_reached_after_shutdown_began_consumes_no_unit() {
    // Arrange
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let shutdown = crate::paid_probe_lifecycle::LifecycleGate::running();
    shutdown.begin_for_tests();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    // Act
    crate::paid_probe_lifecycle::reserve_and_answer(
        &mut state,
        command,
        &counters,
        &shutdown,
        wall_clock_ms(),
    );
    let outcome = receipt.await.expect("the writer answered");

    // Assert: refused ...
    assert_eq!(outcome, PaidProbeCommit::Unavailable);
    // ... and NO unit was spent, which is what distinguishes this check from the
    // post-commit one.
    assert_eq!(
        stored_units(&path, day, "anthropic"),
        None,
        "a reservation reached after shutdown must not spend a unit of the day's cap",
    );
    assert!(
        reservation_keys(&path).is_empty(),
        "no accounting row may be created at all",
    );
}

/// A working reservation must NOT announce that the request-row path recovered.
///
/// The two writes are different statements against different tables, so a
/// control row landing says nothing about whether usage rows can. A writer that
/// claimed recovery here would turn a live fault into a false all-clear on the
/// surface operators read to decide whether the ledger is trustworthy.
///
/// Driven at the writer-state seam because the degraded flag is the writer
/// thread's own: the health EDGE is what is under test, and only the state that
/// carries it can witness whether the edge fired.
#[tokio::test]
async fn a_successful_reservation_does_not_claim_the_request_path_recovered() {
    // Arrange: a real migrated database whose `requests` INSERTs are rejected by
    // a trigger while the control table keeps working -- exactly the split this
    // guard is about.
    let (_dir, path) = temp_path();
    {
        let db = crate::db::open(&path).expect("migrating open");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_rows BEFORE INSERT ON requests \
                 BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
            )
            .expect("install trigger");
    }
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let shutdown = crate::paid_probe_lifecycle::LifecycleGate::running();

    // Act: a row fails (degrading the writer) ...
    state.persist_for_tests(&UsageRecord::default(), &counters);
    let degraded_after_row = state.is_degraded_for_tests();

    // ... a reservation succeeds ...
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);
    crate::paid_probe_lifecycle::reserve_and_answer(
        &mut state,
        command,
        &counters,
        &shutdown,
        wall_clock_ms(),
    );
    let reservation = receipt.await.expect("the writer answered");
    let degraded_after_reservation = state.is_degraded_for_tests();

    // ... and the next row still fails.
    let errors_before_second_row = counters.write_errors();
    state.persist_for_tests(&UsageRecord::default(), &counters);

    // Assert the premise: the row path really was degraded, or there is no
    // recovery edge to falsely emit and the test is vacuous.
    assert!(
        degraded_after_row,
        "the premise requires the failing row to degrade the writer",
    );

    // Assert: the reservation committed ...
    assert_eq!(reservation, PaidProbeCommit::Committed { used: 1, cap: 3 });
    // ... without clearing the degraded state (a `mark_healthy` here is exactly
    // what a false all-clear consists of) ...
    assert!(
        degraded_after_reservation,
        "a successful control write must not clear the request path's degraded state",
    );
    // ... and the row path is still broken afterwards.
    assert!(
        counters.write_errors() > errors_before_second_row,
        "the second row must still fail, so the degraded state was real",
    );
    assert_eq!(counters.persisted(), 0, "no usage row may have persisted");
}

/// The converse control: a request row that SUCCEEDS does clear the edge. Without
/// it, a `mark_healthy` deleted altogether would satisfy the test above.
#[tokio::test]
async fn a_successful_request_row_does_clear_the_degraded_state() {
    // Arrange: degrade the writer with a failing row, then remove the cause.
    let (_dir, path) = temp_path();
    {
        let db = crate::db::open(&path).expect("migrating open");
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_rows BEFORE INSERT ON requests \
                 BEGIN SELECT RAISE(ABORT, 'forced row failure'); END",
            )
            .expect("install trigger");
    }
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    state.persist_for_tests(&UsageRecord::default(), &counters);
    assert!(
        state.is_degraded_for_tests(),
        "the premise requires a degraded writer",
    );

    // Act
    {
        let db = crate::db::open_rw(&path).expect("rw open");
        db.conn()
            .execute_batch("DROP TRIGGER reject_rows")
            .expect("remove the cause");
    }
    state.persist_for_tests(&UsageRecord::default(), &counters);

    // Assert: the ROW path's own success is what clears the edge.
    assert!(
        !state.is_degraded_for_tests(),
        "a successful request row must clear the degraded state",
    );
    assert_eq!(counters.persisted(), 1);
}
