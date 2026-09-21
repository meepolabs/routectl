//! The short synchronous lifecycle state every probe publication and the
//! shutdown serialize their transition through.
//!
//! # The race this closes
//!
//! A publication is not one instruction: it draws a ticket value, stamps it,
//! retires superseded work, clears the candidate list, and (through the callback
//! form) stores the replacement router. A shutdown does the same shape of work.
//! Run concurrently and interleaved, the two produce states neither intends --
//! most damagingly a publication whose STORE lands after a shutdown has already
//! cleared, publishing a router into a daemon that is going away.
//!
//! The shared ticket alone cannot fix that. It orders the GENERATION correctly,
//! and an authorization reading it abandons itself as designed; but the ticket
//! says nothing about whether the rest of a publication's own transition has
//! run, so a publication that drew a value before a shutdown can still be
//! mid-transition after it.
//!
//! # Why a lock here is not the exclusion that was withdrawn
//!
//! The design this replaces held an async guard ACROSS THE LEDGER AWAIT, so a
//! wedged accounting layer could hold a reload open. Nothing here spans an
//! await: this mutex is taken and released inside one synchronous transition
//! whose longest step is a bounded in-memory sweep. A paid authorization never
//! takes it at all -- it continues to order itself optimistically against the
//! ticket. So the availability property is unchanged: publication and shutdown
//! still wait on nothing that can be slow.
//!
//! `parking_lot::Mutex` rather than the async one for exactly that reason: this
//! is a short critical section between synchronous callers, and an async lock
//! here would be both slower and a standing invitation to await inside it.
//!
//! # Liveness is structural, not a convention
//!
//! [`ProbeLifecycleState::begin_publication`] performs the terminal check INSIDE
//! the acquisition and returns `Option<LiveLifecycleTransition>`. The stamping
//! logic accepts only that token, so "checked terminal" and "publishing" cannot
//! be two different acquisitions: there is no way to obtain the token without the
//! check, and no way to stamp without the token. A caller that dropped it and
//! re-acquired would have to call `begin_publication` again -- and get a fresh
//! check.
//!
//! # Re-entry is refused before the mutex is touched
//!
//! The callback form invokes a caller-supplied store while the transition is
//! held. A callback that re-entered publication or shutdown would deadlock on a
//! non-reentrant mutex, turning a contract violation into a hang. A thread-local
//! depth guard is therefore checked BEFORE any acquisition, so re-entry returns
//! immediately as a no-op instead.
//!
//! # Shared across replacement routers
//!
//! Attached by carry-over like the scheduler, the candidate list, and the
//! ticket. The publishing router and the shutting-down router are different
//! objects, so a per-router state would serialize nothing between them.

use std::cell::Cell;

use parking_lot::{Mutex, MutexGuard};

thread_local! {
    /// Whether THIS thread is already inside a lifecycle transition.
    ///
    /// Thread-local rather than shared state, because re-entry is a property of
    /// one call stack: a DIFFERENT thread contending is ordinary contention that
    /// the mutex handles correctly, and treating it as re-entry would refuse
    /// legitimate concurrent publications.
    ///
    /// A `bool` rather than a depth count: one level of nesting is already the
    /// violation, so there is nothing to count.
    static IN_TRANSITION: Cell<bool> = const { Cell::new(false) };
}

/// Shared lifecycle state for probe publication and shutdown.
///
/// Guards no data of its own beyond the terminal bit. What it provides is the
/// SERIALIZATION the ticket cannot: that one publication's or shutdown's whole
/// short transition completes before another begins.
#[derive(Debug, Default)]
pub(super) struct ProbeLifecycleState {
    inner: Mutex<LifecycleInner>,
}

#[derive(Debug, Default)]
struct LifecycleInner {
    /// Set once by shutdown, never cleared.
    ///
    /// TERMINAL rather than a phase enum, because there is exactly one
    /// irreversible transition here and nothing resumes after it.
    shut_down: bool,
}

impl ProbeLifecycleState {
    /// Begin a publication, or `None` when one may not run.
    ///
    /// THE only route to a [`LiveLifecycleTransition`], and therefore the only
    /// route to stamping: the terminal check happens INSIDE this acquisition and
    /// its result is carried by the token's existence. A caller cannot check
    /// through one acquisition and publish through another, because publishing
    /// requires a token this function alone mints.
    ///
    /// `None` for two distinct reasons, deliberately collapsed because the caller
    /// does the same thing for both (nothing at all): the lifecycle is terminal,
    /// or this thread is already inside a transition. The re-entry case is checked
    /// FIRST, before the mutex is touched, so a nested call returns rather than
    /// deadlocking on a non-reentrant lock.
    pub(super) fn begin_publication(&self) -> Option<LiveLifecycleTransition<'_>> {
        if IN_TRANSITION.get() {
            return None;
        }
        let guard = self.inner.lock();
        if guard.shut_down {
            return None;
        }
        IN_TRANSITION.set(true);
        Some(LiveLifecycleTransition { _guard: guard })
    }

    /// Begin the SHUTDOWN transition, or `None` on re-entry from this thread.
    ///
    /// Separate from [`Self::begin_publication`] because shutdown is the one
    /// caller that must proceed WHEN ALREADY TERMINAL -- it is idempotent, and a
    /// second shutdown must still be allowed to clear whatever a late
    /// reactivation left behind. So it gets a token that carries no liveness
    /// claim, only the exclusion and the ability to set the bit.
    pub(super) fn begin_shutdown(&self) -> Option<ShutdownTransition<'_>> {
        if IN_TRANSITION.get() {
            return None;
        }
        let mut guard = self.inner.lock();
        guard.shut_down = true;
        IN_TRANSITION.set(true);
        Some(ShutdownTransition { _guard: guard })
    }
}

/// Proof that a LIVE publication transition is held: the lifecycle was not
/// terminal at acquisition, and still is not, because this token holds the lock
/// that a shutdown would need.
///
/// Opaque and field-less to its users. It carries no accessor at all -- there is
/// nothing to ask it, because its EXISTENCE is the entire answer. Not `Clone`, so
/// one acquisition cannot become two.
pub(super) struct LiveLifecycleTransition<'a> {
    _guard: MutexGuard<'a, LifecycleInner>,
}

/// The shutdown counterpart. Holds the same exclusion and has already set the
/// terminal bit.
pub(super) struct ShutdownTransition<'a> {
    _guard: MutexGuard<'a, LifecycleInner>,
}

/// Clear this thread's re-entry marker when either token is dropped.
///
/// On `Drop` rather than at each call's end, so an early return or a panicking
/// callback cannot leave the marker set and make every later publication on this
/// thread a silent no-op.
impl Drop for LiveLifecycleTransition<'_> {
    fn drop(&mut self) {
        IN_TRANSITION.set(false);
    }
}

impl Drop for ShutdownTransition<'_> {
    fn drop(&mut self) {
        IN_TRANSITION.set(false);
    }
}
