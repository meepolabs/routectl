// Gate-level guarantees: the answer is sent while the lifecycle lock is held, an
// unknown lifecycle authorizes nothing, and the divergence counter tracks
// consumed units rather than refusals.
//
// Every ordering assertion here rests on a real rendezvous (a barrier, or the
// gate's own terminal signal) plus a DIRECT observation of the lock. Where a
// bound appears it only limits how long the test waits for a condition it then
// asserts -- no sleep or yield is ever the evidence.

/// The send happens INSIDE the critical section: while a reservation is parked
/// between the terminal check and its answer, a concurrent shutdown provably
/// cannot complete, and the caller still receives `Committed`.
///
/// This is the pin for "send inside the lock". If the answer moved outside the
/// section, shutdown would complete while the reservation was parked and the
/// ordering the whole gate exists to provide would be gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_answer_is_sent_while_the_lifecycle_lock_is_held() {
    // Arrange
    let (_dir, path) = temp_path();
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);

    let parked = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let shutdown_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Act: park between the terminal check and the answer, guard still held.
    let reservation = {
        let parked = Arc::clone(&parked);
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
                    parked.wait();
                    release.wait();
                },
            );
        })
    };
    parked.wait();

    // Assert the premise DIRECTLY: the gate is held right now, so the answer that
    // follows is necessarily sent under the lock.
    assert!(
        gate.is_held_for_tests(),
        "the reservation must hold the lifecycle gate at the point it answers",
    );

    // A shutdown started now must not be able to finish.
    let shutdown = {
        let gate = gate.clone();
        let done = Arc::clone(&shutdown_done);
        std::thread::spawn(move || {
            gate.begin_for_tests();
            done.store(true, std::sync::atomic::Ordering::Release);
        })
    };
    let bound = std::time::Instant::now() + Duration::from_millis(400);
    while std::time::Instant::now() < bound {
        assert!(
            !shutdown_done.load(std::sync::atomic::Ordering::Acquire),
            "shutdown completed while the answer was still inside the critical \
             section -- the send is not under the lock",
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // The caller's answer must already be available, since the send precedes the
    // release of the guard -- read it BEFORE letting shutdown through.
    release.wait();
    reservation.join().expect("reservation thread");
    let outcome = receipt.await.expect("the writer answered");
    shutdown.join().expect("shutdown thread");

    // Assert
    assert_eq!(
        outcome,
        PaidProbeCommit::Committed { used: 1, cap: 3 },
        "an answer sent inside the section must not be suppressed by a later shutdown",
    );
    assert_eq!(counters.paid_probe_consumed_unauthorized(), 0);
}

/// The structural backstop for the test above: the answer is sent through a
/// CONSUMING section wrapper that owns the guard, so releasing the lock early
/// means destroying the only thing that can answer.
///
/// A source guard as well as a type-level one, because the type only holds while
/// the code keeps using it: a future edit that reverted to a bare `let _guard`
/// plus `command.answer(..)` would compile and read identically, and this is what
/// refuses it.
#[test]
fn the_final_answer_cannot_be_moved_outside_the_critical_section() {
    // Arrange
    let source = include_str!("paid_probe_lifecycle.rs");
    let section = text_between_in(source, "    fn answer_unless_terminal(", "\n    }\n");

    // Assert the premise: this really is the function that answers.
    assert!(
        section.contains("terminal"),
        "the guard must be reading the check-and-send function",
    );

    // Assert: the guard is MOVED into a section value, every answer goes through
    // that value, and nothing releases the lock early.
    assert!(
        section.contains("GatedSection { _guard: guard }"),
        "the guard must be moved into the section so it cannot be dropped early: {section}",
    );
    assert!(
        !section.contains("drop("),
        "nothing may release the lifecycle gate before the answer: {section}",
    );
    assert!(
        !section.contains("command.answer("),
        "the answer must go through the gated section, never straight from the \
         command: {section}",
    );
    assert_eq!(
        section.matches("section.answer(").count(),
        2,
        "both the suppressed and the real answer must go through the section: {section}",
    );

    // And the section type BORROWS to answer. By value the guard would be moved
    // into the method and could be destructured or dropped there before the send,
    // which compiles and reads fine; borrowing makes it unreachable for either.
    let section_impl = text_between_in(source, "impl GatedSection<'_> {", "\n}\n");
    assert!(
        section_impl.contains("fn answer(&self,"),
        "the section must answer through a BORROW, so its guard cannot be moved or \
         dropped before the send: {section_impl}",
    );
    assert!(
        !section_impl.contains("fn answer(self,"),
        "answering by value would let the guard be dropped inside the method: \
         {section_impl}",
    );
    let answer_body = text_between_in(
        source,
        "    fn answer(&self, command: PaidProbeCommand, outcome: PaidProbeCommit) {",
        "\n    }\n",
    );
    assert!(
        answer_body.contains("command.answer(outcome)"),
        "the section's answer must actually send: {answer_body}",
    );
    for forbidden in ["drop(", "let Self", "self._guard", "Self { _guard"] {
        assert!(
            !answer_body.contains(forbidden),
            "the send path must not release or extract the guard ({forbidden}): \
             {answer_body}",
        );
    }
    // The section variable must outlive both sends at the call site, which is what
    // makes the release point explicit rather than incidental.
    assert!(
        section.contains("let section = GatedSection { _guard: guard };"),
        "the guard must be moved into a named section binding: {section}",
    );
}

/// The fail-closed decision is pinned DIRECTLY, in every profile.
///
/// The poisoned-mutex test below can only run where panics unwind, so the rule
/// must not REST on that: a profile change would leave that fixture unable to
/// fail while still reading as coverage. The decision is pure, so it is asserted
/// on its own here, and all three readings are pinned as a set so a predicate
/// that answered one way for everything cannot satisfy it.
#[test]
fn an_unknown_gate_reading_forbids_authorization_in_every_profile() {
    // Assert: an unknown lifecycle authorizes nothing ...
    assert!(
        crate::paid_probe_lifecycle::unknown_reading_forbids_authorization_for_tests(),
        "an unknown lifecycle must authorize nothing, in every build profile",
    );
    // ... a terminal one likewise ...
    assert!(
        crate::paid_probe_lifecycle::terminal_reading_forbids_authorization_for_tests(),
        "a terminal lifecycle must authorize nothing",
    );
    // ... and a healthy running one DOES authorize, which is the control that
    // keeps the two assertions above from passing for a refuse-everything rule.
    assert!(
        crate::paid_probe_lifecycle::running_reading_permits_authorization_for_tests(),
        "a running lifecycle must still authorize, or the rule refuses everything",
    );
}

/// The same rule end to end over a REALLY poisoned mutex: the reservation
/// refuses, writes no row, and counts no divergence.
///
/// `cfg(panic = "unwind")` because poisoning REQUIRES an unwind. Measured: both
/// the debug and release test profiles unwind here (`panic = "abort"` in
/// `[profile.release]` governs the shipped binary, not cargo's test harnesses),
/// so this case does run under the gated suite -- enumerated in both to confirm
/// no silent drop. The pure decision test above is what keeps the rule pinned in
/// a build where this fixture could not fail at all.
#[cfg(panic = "unwind")]
#[tokio::test]
async fn a_poisoned_gate_refuses_and_consumes_nothing() {
    // Arrange
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();

    // Assert the premise: the gate really is poisoned now. If the profile cannot
    // poison a mutex at all, say so rather than passing vacuously.
    assert!(
        gate.poison_for_tests(),
        "the poisoning seam must actually poison the gate",
    );
    assert!(
        gate.forbids_authorization_for_tests(),
        "a poisoned gate must read as forbidding authorization",
    );

    // Act
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);
    crate::paid_probe_lifecycle::reserve_and_answer(
        &mut state,
        command,
        &counters,
        &gate,
        wall_clock_ms(),
    );
    let outcome = receipt.await.expect("the writer answered");

    // Assert: refused, nothing spent, nothing counted as consumed.
    assert_eq!(
        outcome,
        PaidProbeCommit::Unavailable,
        "an unknown lifecycle must authorize nothing",
    );
    assert_eq!(
        stored_units(&path, day, "anthropic"),
        None,
        "a refusal before the transaction must spend no unit",
    );
    assert!(reservation_keys(&path).is_empty());
    assert_eq!(
        counters.paid_probe_consumed_unauthorized(),
        0,
        "nothing was consumed, so nothing diverged",
    );
}

/// The positive control for the poison test: an UNPOISONED gate on the same
/// fixture commits. Without it, a reservation that refused unconditionally would
/// satisfy the test above.
#[cfg(panic = "unwind")]
#[tokio::test]
async fn an_unpoisoned_gate_on_the_same_fixture_commits() {
    // Arrange
    let (_dir, path) = temp_path();
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
    assert!(
        !gate.forbids_authorization_for_tests(),
        "the control's gate must be healthy",
    );

    // Act
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 3);
    crate::paid_probe_lifecycle::reserve_and_answer(
        &mut state,
        command,
        &counters,
        &gate,
        wall_clock_ms(),
    );

    // Assert
    assert_eq!(
        receipt.await.expect("answered"),
        PaidProbeCommit::Committed { used: 1, cap: 3 },
    );
}

/// A SUPPRESSED NON-COMMIT does not touch the divergence counter: an exhausted
/// cap consumed no unit, so shutdown suppressing its answer is not a divergence
/// between budget spent and calls made.
///
/// The counter's meaning is "a unit is on disk that nobody was authorized to
/// use". Counting a refusal here would inflate it with cases where nothing was
/// spent at all, which is precisely the signal an accounting-health surface must
/// be able to trust.
#[tokio::test]
async fn a_suppressed_non_commit_does_not_count_as_a_consumed_unit() {
    // Arrange: cap zero, so the reservation can only reach CapExhausted.
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut state = crate::writer::writer_state_for_tests(&path);
    let counters = Arc::new(UsageCounters::default());
    let gate = crate::paid_probe_lifecycle::LifecycleGate::running();
    let (command, receipt) = crate::writer::paid_probe_command_for_tests("anthropic", 0);

    // Act: shutdown lands at the final-send hook, so the (non-commit) answer is
    // suppressed exactly as a committed one would be.
    let gate_mid = gate.clone();
    crate::paid_probe_lifecycle::reserve_and_answer_instrumented(
        &mut state,
        command,
        &counters,
        &gate,
        wall_clock_ms(),
        &move || gate_mid.begin_for_tests(),
        &|| {},
    );
    let outcome = receipt.await.expect("the writer answered");

    // Assert: suppressed ...
    assert_eq!(
        outcome,
        PaidProbeCommit::Unavailable,
        "a suppressed answer is Unavailable whatever the underlying outcome",
    );
    // ... nothing on disk, because a zero cap writes nothing ...
    assert_eq!(stored_units(&path, day, "anthropic"), None);
    assert!(reservation_keys(&path).is_empty());
    // ... and NOT counted, because no unit was consumed.
    assert_eq!(
        counters.paid_probe_consumed_unauthorized(),
        0,
        "a suppressed refusal consumed no unit, so it is not a divergence",
    );
}

/// Shutdown marks the gate terminal BEFORE it drops the sender or drains, and
/// this pins the precedence structurally.
///
/// Its behavioural sibling (the queued-reservation test) can no longer catch a
/// reordering on its own: once the ordering moved behind a real rendezvous, a
/// writer that marked terminal only after draining still passes, because the
/// drain deadline elapses and the signal arrives before the test times out. The
/// mutation survived exactly that way. A structural pin is the honest guard for
/// a precedence whose violation is otherwise only visible as a rare race.
#[test]
fn shutdown_marks_the_gate_terminal_before_it_drains() {
    // Arrange
    let source = include_str!("writer.rs");
    let body = text_between_in(source, "    fn shutdown_inner(&mut self) {", "\n    }\n");

    // Assert the premise: this is the teardown, and it does all three things.
    for required in ["begin_shutdown()", "self.sender.take()", "recv_timeout("] {
        assert!(
            body.contains(required),
            "the guard must be reading the full teardown ({required}): {body}",
        );
    }

    // Assert: terminal is marked before the sender is dropped, and before the
    // drain is awaited. A reservation still queued at that point can then only be
    // adjudicated by the gated check-and-send, never authorized.
    let terminal_at = body.find("begin_shutdown()").expect("marks terminal");
    let sender_at = body.find("self.sender.take()").expect("drops the sender");
    let drain_at = body.find("recv_timeout(").expect("awaits the drain");
    assert!(
        terminal_at < sender_at,
        "the gate must be marked terminal before the sender is dropped: {body}",
    );
    assert!(
        terminal_at < drain_at,
        "the gate must be marked terminal before the drain is awaited: {body}",
    );
}

/// The terminal signal the shutdown tests wait on is emitted AFTER the state
/// change and while the guard is held, so receiving it proves terminal has taken
/// effect.
///
/// Pinned structurally because the failure mode is a WEAKENED TEST rather than
/// broken production code: an ack emitted before the state change still lets the
/// waiting test proceed, just a few instructions too early, so the race it exists
/// to exclude reopens by a hair and the behavioural test passes anyway (measured:
/// 6/6 green with the signal moved before the assignment). A test's own ordering
/// primitive has to be verified where the ordering is written.
#[test]
fn the_terminal_signal_is_emitted_after_the_state_change() {
    // Arrange
    let source = include_str!("paid_probe_lifecycle.rs");
    let body = text_between_in(
        source,
        "    pub(crate) fn begin_shutdown(&self) {",
        "\n    }\n",
    );

    // Assert the premise: this is the transition, and it does signal.
    assert!(
        body.contains("Lifecycle::Terminal") && body.contains("terminal_ack"),
        "the guard must be reading the transition that signals: {body}",
    );

    // Assert: the state is assigned before the signal is sent, and both happen
    // while the guard from `lock()` is still alive.
    let assign_at = body
        .find("*guard = Lifecycle::Terminal;")
        .expect("the state assignment");
    let signal_at = body.find("terminal_ack").expect("the signal");
    assert!(
        assign_at < signal_at,
        "the signal must follow the state change, or a waiter can proceed before \
         terminal has taken effect: {body}",
    );
    assert!(
        !body.contains("drop(guard)"),
        "the signal must be emitted while the gate is still held: {body}",
    );
}

/// The text between `opener` and the first `terminator` after it, comments
/// stripped.
///
/// Local to this fragment: the sibling surface guards cut different files with
/// their own conventions, and sharing one helper across both would couple two
/// unrelated scans.
fn text_between_in(source: &str, opener: &str, terminator: &str) -> String {
    let start = source
        .find(opener)
        .unwrap_or_else(|| panic!("declaration not found: {opener}"));
    let rest = &source[start..];
    let end = rest
        .find(terminator)
        .unwrap_or_else(|| panic!("terminator not found after {opener}"));
    rest[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
