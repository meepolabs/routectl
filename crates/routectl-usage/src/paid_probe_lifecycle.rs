//! The paid-probe reservation's lifecycle: the gate that linearizes shutdown
//! against a reservation's answer, and the writer-side handler that spends a
//! unit and reports it.
//!
//! WHY A GATE AND NOT A FLAG. The property that has to hold is an ORDERING
//! between two events on different threads: the moment a caller is handed an
//! authorization, and the moment the subsystem that must honor it begins going
//! away. An atomic flag can only be sampled -- read it, and by the time the next
//! instruction runs the answer may already be stale. That leaves a window
//! between the final check and the send in which shutdown can begin, which is
//! exactly the case that matters: a caller holding `Committed` for a writer that
//! is already draining.
//!
//! So both sides take the same short lock. Shutdown marks the gate terminal
//! BEFORE it drains or drops the sender; the reservation performs its final
//! check AND its send inside one critical section. The two cannot interleave, so
//! for every reservation exactly one of these is true:
//!
//! * the answer was sent before shutdown began -- and the send happens-before
//!   the drain, so the writer is still there to honor it; or
//! * shutdown began first, and the answer is suppressed to
//!   [`PaidProbeCommit::Unavailable`].
//!
//! THE GATE IS NEVER HELD ACROSS THE TRANSACTION. SQLite work is unbounded in a
//! way a lifecycle lock must not be: holding it there would make shutdown wait
//! on a wedged database, turning a liveness property into a hang. The gate is
//! taken twice, briefly -- once to check before spending, once around the
//! check-and-send -- and the transaction runs between them, unlocked.
//!
//! A UNIT SUPPRESSED AFTER COMMIT IS STILL SPENT. There is no refund anywhere in
//! this design, so the commit stands; what is withheld is the authorization. That
//! divergence -- budget consumed, no call made -- is counted
//! (`UsageCounters::paid_probe_consumed_unauthorized`) rather than folded into
//! the writer's shared degraded state, because it is neither a storage fault nor
//! a healthy write, and an accounting-health surface has to be able to see it.

#![expect(
    clippy::redundant_pub_crate,
    reason = "this module is private, so a crate-wide visibility reads as \
              redundant -- but the writer actor and the producer handle both have \
              to name the gate and the reservation entry points, and widening \
              them to `pub` would publish the actor's transport and the one value \
              that authorizes a paid call"
)]

use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::handle::UsageCounters;
use crate::paid_probe_command::{PaidProbeCommand, PaidProbeCommit};

/// Whether the writer subsystem may still authorize a spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Reservations may be admitted, spent, and authorized.
    Running,
    /// Shutdown has begun. One-way: nothing returns to `Running`, because a
    /// subsystem does not un-begin a teardown, and a gate that could flip back
    /// would let a late answer authorize against a writer that already drained.
    Terminal,
}

/// How a lock attempt on the gate read.
///
/// Named rather than inlined because the POISONED reading is a decision, not a
/// detail: it is the one place the gate chooses what an unknown lifecycle means.
/// Naming it also makes the choice assertable on its own, without having to
/// produce a poisoned mutex -- which matters for any build where panics do not
/// unwind, since there no mutex can ever be poisoned and a fixture-based test
/// would be structurally unable to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateReading {
    /// The lock was healthy; the lifecycle is whatever it says.
    Live(Lifecycle),
    /// A thread panicked holding the gate, so the lifecycle is unknown.
    Unknown,
}

/// Whether a reading forbids authorizing a spend.
///
/// FAIL CLOSED on `Unknown`: an unknown lifecycle is exactly as untrustworthy as
/// a teardown, and the cost of the two mistakes is not symmetric -- refusing a
/// reservation wastes at most one unit of a cap, while authorizing one against a
/// subsystem in an unknown state hands out a spend nobody can honor.
const fn forbids_authorization(reading: GateReading) -> bool {
    match reading {
        GateReading::Live(Lifecycle::Running) => false,
        GateReading::Live(Lifecycle::Terminal) | GateReading::Unknown => true,
    }
}

/// The shared lifecycle gate, held by the writer and every producer handle.
///
/// See the module docs for why this is a lock rather than a flag.
#[derive(Clone)]
pub(crate) struct LifecycleGate {
    state: Arc<Mutex<Lifecycle>>,
    /// Test-only observer, signalled the instant the gate is marked terminal.
    ///
    /// Exists so a test can wait for the EXACT moment shutdown takes effect
    /// instead of guessing with a sleep or a yield. Signalled while the guard is
    /// still held, so the notification can never precede the state change it
    /// reports.
    #[cfg(test)]
    terminal_ack: Arc<std::sync::OnceLock<std::sync::mpsc::Sender<()>>>,
}

impl LifecycleGate {
    /// A gate for a subsystem that has not begun shutting down.
    pub(crate) fn running() -> Self {
        Self {
            state: Arc::new(Mutex::new(Lifecycle::Running)),
            #[cfg(test)]
            terminal_ack: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Take the gate, reporting how it read.
    ///
    /// A poisoned lock is recovered rather than propagating the panic, so a
    /// poisoned gate still refuses cleanly instead of unwinding a writer thread --
    /// and [`forbids_authorization`] is what decides that an unknown reading
    /// authorizes nothing.
    fn lock(&self) -> (MutexGuard<'_, Lifecycle>, bool) {
        match self.state.lock() {
            Ok(guard) => {
                let reading = GateReading::Live(*guard);
                (guard, forbids_authorization(reading))
            }
            Err(poisoned) => (
                poisoned.into_inner(),
                forbids_authorization(GateReading::Unknown),
            ),
        }
    }

    /// Mark the subsystem terminal. Called BEFORE the drain and before the
    /// sender is dropped, never unset.
    ///
    /// Returning only after the lock is released is what gives the ordering its
    /// teeth: once this returns, no reservation can still be inside a
    /// check-and-send that began before it.
    pub(crate) fn begin_shutdown(&self) {
        let (mut guard, _) = self.lock();
        *guard = Lifecycle::Terminal;
        // Signalled INSIDE the critical section, immediately after the state
        // change, so a waiting test learns the exact moment terminal took effect.
        #[cfg(test)]
        if let Some(ack) = self.terminal_ack.get() {
            let _ = ack.send(());
        }
    }

    /// Whether shutdown has begun.
    ///
    /// For the checks that only need to FAIL CLOSED -- admission, and the
    /// pre-transaction check. Both are allowed to be racy in the safe direction:
    /// a reservation admitted a moment before shutdown is still adjudicated by
    /// the gated check-and-send, which is not racy at all.
    pub(crate) fn is_terminal(&self) -> bool {
        let (_guard, terminal) = self.lock();
        terminal
    }

    /// Perform the final terminal check AND the answer inside ONE critical
    /// section.
    ///
    /// This is the whole point of the gate, and the guard's lifetime is the
    /// mechanism: `_guard` is bound for the whole function, so every `answer`
    /// below runs with the lock held. NOTHING may release it early -- a
    /// `drop(_guard)` anywhere in this body reopens the exact race the gate
    /// exists to close, which is why a source backstop refuses one.
    ///
    /// `before_answer` runs after the terminal check and immediately before the
    /// answer, with the guard still held, so a test can park precisely inside the
    /// critical section and prove a concurrent `begin_shutdown` cannot interleave.
    ///
    /// Returns whether the real outcome was delivered; `false` means it was
    /// suppressed because shutdown had begun.
    fn answer_unless_terminal(
        &self,
        command: PaidProbeCommand,
        outcome: PaidProbeCommit,
        before_answer: &dyn Fn(),
    ) -> bool {
        let (guard, terminal) = self.lock();
        // The guard is MOVED into the section and only ever BORROWED from it, so
        // no edit inside the send path can release the lock early -- the ordering
        // is structural rather than a convention. `section` stays alive across
        // both sends below and is dropped, releasing the gate, only on the way out.
        let section = GatedSection { _guard: guard };
        before_answer();
        if terminal {
            section.answer(command, PaidProbeCommit::Unavailable);
            return false;
        }
        section.answer(command, outcome);
        true
    }
}

/// The open critical section: proof, in the type system, that an answer is being
/// sent with the lifecycle gate held.
///
/// The guard is MOVED in here and the only way to send an answer is
/// [`Self::answer`], which BORROWS the section. Because the guard is private and
/// the method takes `&self`, safe Rust cannot move or drop it inside the send
/// path; the caller-owned `section` determines the release point after the send.
/// So "send inside the lock" is enforced by ownership rather than by a reviewer
/// noticing that no early `drop` was inserted.
struct GatedSection<'gate> {
    _guard: MutexGuard<'gate, Lifecycle>,
}

impl GatedSection<'_> {
    /// Send the answer with the gate held.
    ///
    /// Takes `&self`, NOT `self`. By value, the guard would be moved into this
    /// method and could be destructured or dropped inside it before the send --
    /// `let Self { _guard } = self; drop(_guard); command.answer(..)` compiles and
    /// reads fine. Borrowing makes the guard unreachable for moving or dropping
    /// here at all, so the lock provably outlives every send, and the section
    /// variable at the call site is what owns the release point.
    fn answer(&self, command: PaidProbeCommand, outcome: PaidProbeCommit) {
        command.answer(outcome);
    }
}

/// What the reservation handler needs from the writer it runs inside.
///
/// A narrow trait rather than a direct dependency on the writer's state: this
/// module owns the reservation's lifecycle, and the actor owns the connection
/// and the health accounting. Keeping the seam this small is what lets the whole
/// reservation path live here while `writer.rs` keeps only its generic loop.
pub(crate) trait ReservationHost {
    /// The writer's connection, or `None` when the open failed.
    fn reservation_connection(&mut self) -> Option<&mut Connection>;

    /// Record a storage failure, with the writer's usual degraded accounting.
    fn note_reservation_failure(&mut self, counters: &Arc<UsageCounters>);
}

/// Run one acknowledged reservation and report what it established.
///
/// The sequence, and why it is this one:
///
/// 1. Check the gate. Terminal here means the unit is never SPENT -- distinct
///    from the post-commit case, and the reason this check is not redundant.
/// 2. Sample the clock and run the transaction, UNLOCKED. The day a unit lands
///    in is this thread's own and cannot drift between the cap gate and the
///    commit.
/// 3. Apply the outcome to the writer's health accounting.
/// 4. Re-take the gate and, inside that one critical section, either send the
///    real outcome or suppress it.
///
/// The enabled gate is deliberately NOT consulted: it was already bypassed at
/// admission, because a telemetry preference must not disable a spend ceiling.
/// A dropped ack receiver is ignored -- the writer never blocks on a caller that
/// left, and a committed unit stays committed.
pub(crate) fn reserve_and_answer<H: ReservationHost>(
    host: &mut H,
    command: PaidProbeCommand,
    counters: &Arc<UsageCounters>,
    gate: &LifecycleGate,
    now_epoch_ms: i64,
) {
    reserve_and_answer_instrumented(host, command, counters, gate, now_epoch_ms, &|| (), &|| ())
}

/// The whole reservation body, with two synchronization points.
///
/// `before_final_send` runs after the outcome is formed and BEFORE the gate is
/// re-taken; `in_critical_section` runs INSIDE that critical section, between
/// the terminal check and the send. Together they let a test place a concurrent
/// shutdown on either side of the lock and observe which one wins, with no
/// sleeping and no yielding. Production reaches this same body through
/// [`reserve_and_answer`] with hooks that do nothing, so there is no second
/// implementation to drift.
pub(crate) fn reserve_and_answer_instrumented<H: ReservationHost>(
    host: &mut H,
    command: PaidProbeCommand,
    counters: &Arc<UsageCounters>,
    gate: &LifecycleGate,
    now_epoch_ms: i64,
    before_final_send: &dyn Fn(),
    in_critical_section: &dyn Fn(),
) {
    if gate.is_terminal() {
        command.answer(PaidProbeCommit::Unavailable);
        return;
    }

    let outcome =
        crate::paid_probe_command::reserve(host.reservation_connection(), &command, now_epoch_ms);
    note_outcome(host, outcome, counters);

    before_final_send();

    let committed = matches!(outcome, PaidProbeCommit::Committed { .. });
    let delivered = gate.answer_unless_terminal(command, outcome, in_critical_section);

    if !delivered && committed {
        // Shutdown won the race with the transaction. The unit is on disk and
        // stays there -- nothing is ever given back -- but the caller was not
        // authorized, so budget was consumed for no call. Counted so an
        // accounting-health surface can show the divergence, and logged so it is
        // visible without one.
        counters.incr_paid_probe_consumed_unauthorized();
        tracing::warn!(
            target: "routectl_usage::paid_probe",
            "paid-probe unit committed as shutdown began -- unit consumed, not authorized"
        );
    }
}

/// Apply a reservation outcome to the writer's health accounting.
///
/// DELIBERATELY NOT the writer's recovery edge. That edge means "the
/// request-row path is working again", and a reservation proves nothing of the
/// kind: it writes a single control row through a different statement against a
/// different table. A writer whose `requests` INSERTs are failing would
/// otherwise announce a recovery it has not made, and the next row would fail
/// again -- turning a silent fault into a false all-clear on the surface
/// operators read to decide whether the ledger is trustworthy.
///
/// Failures still count and still degrade, because a control write that cannot
/// land IS evidence the storage is broken. The asymmetry is the point: bad news
/// from any writer generalizes, good news does not.
fn note_outcome<H: ReservationHost>(
    host: &mut H,
    outcome: PaidProbeCommit,
    counters: &Arc<UsageCounters>,
) {
    match outcome {
        PaidProbeCommit::Committed { .. }
        | PaidProbeCommit::CapExhausted
        | PaidProbeCommit::Unavailable => {}
        PaidProbeCommit::MalformedState | PaidProbeCommit::WriteFailed => {
            host.note_reservation_failure(counters);
        }
    }
}

/// Crate-private test support for the lifecycle gate.
#[cfg(test)]
impl LifecycleGate {
    /// Mark terminal, for the tests that place the transition at an exact point
    /// relative to a transaction or a critical section.
    pub(crate) fn begin_for_tests(&self) {
        self.begin_shutdown();
    }

    /// Register the channel that [`Self::begin_shutdown`] signals the instant it
    /// marks terminal.
    ///
    /// THE ordering primitive for the shutdown tests: waiting on this receiver is
    /// waiting for the state change itself, so no test has to approximate the
    /// moment with a sleep or a yield. Returns the receiver; may be called once.
    pub(crate) fn watch_terminal_for_tests(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.terminal_ack
            .set(tx)
            .expect("the terminal watcher is registered at most once");
        rx
    }

    /// Whether the gate is currently held by somebody else.
    ///
    /// `try_lock` failing is the only DIRECT evidence a test can take that the
    /// critical section is genuinely exclusive: it observes the lock itself
    /// rather than inferring exclusivity from timing.
    pub(crate) fn is_held_for_tests(&self) -> bool {
        self.state.try_lock().is_err()
    }

    /// Whether the gate is terminal, for the tests that assert a shutdown
    /// provably completed.
    pub(crate) fn is_terminal_for_tests(&self) -> bool {
        self.is_terminal()
    }

    /// Poison the gate the way a panicking holder would.
    ///
    /// `cfg(panic = "unwind")` because poisoning REQUIRES an unwind. Measured on
    /// this workspace: both the debug and the release TEST profiles unwind
    /// (`panic = "abort"` in `[profile.release]` governs the shipped binary, not
    /// cargo's test harnesses), so this does run under the gated suite. The gate is
    /// there for correctness of the claim rather than for a profile we currently
    /// build -- and the pure decision test is what covers a non-unwinding build,
    /// where this fixture could not fail.
    ///
    /// Returns whether poisoning actually took effect, so a caller can assert its
    /// own premise rather than assume it.
    #[cfg(panic = "unwind")]
    pub(crate) fn poison_for_tests(&self) -> bool {
        let state = Arc::clone(&self.state);
        // A dedicated thread whose panic is the MECHANISM, not a failure. Its
        // output is accepted as-is: the panic hook is process-global, so taking
        // and restoring it would race every other test running concurrently and
        // could swallow a genuine panic message from an unrelated thread. A few
        // expected lines of panic output cost nothing by comparison.
        let joined = std::thread::spawn(move || {
            let _guard = state.lock().expect("a fresh gate is not yet poisoned");
            panic!("poisoning the lifecycle gate deliberately");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread must have panicked");
        self.state.lock().is_err()
    }

    /// How the gate currently reads, for the poison tests.
    pub(crate) fn forbids_authorization_for_tests(&self) -> bool {
        self.is_terminal()
    }
}

/// The fail-closed decision itself, reachable without producing a poisoned
/// mutex.
///
/// The poisoned-mutex fixture can only exist where panics unwind. Both test
/// profiles here do (measured), but the guarantee must not REST on that: a
/// profile change, or a future `-Zpanic_abort_tests` run, would leave the
/// fixture unable to fail while still reading as coverage. The decision is pure,
/// so it is pinned directly and holds in every profile.
#[cfg(test)]
pub(crate) const fn unknown_reading_forbids_authorization_for_tests() -> bool {
    forbids_authorization(GateReading::Unknown)
}

/// The same decision for a healthy running gate, as the paired control: without
/// it, a predicate that refused everything would satisfy the assertion above.
#[cfg(test)]
pub(crate) const fn running_reading_permits_authorization_for_tests() -> bool {
    !forbids_authorization(GateReading::Live(Lifecycle::Running))
}

/// And for a healthy terminal gate, so the three readings are pinned as a set.
#[cfg(test)]
pub(crate) const fn terminal_reading_forbids_authorization_for_tests() -> bool {
    forbids_authorization(GateReading::Live(Lifecycle::Terminal))
}
