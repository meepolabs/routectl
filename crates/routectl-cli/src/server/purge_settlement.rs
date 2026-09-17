//! Daemon-owned settlement of admitted capability purges.
//!
//! # The window this closes
//!
//! A purge is durable only once its `cleared` row commits, and the handler that
//! started it awaits that commit. But an HTTP handler's future is CANCELLED when
//! the client goes away -- a closed connection, a CLI that timed out, a
//! `curl` interrupted at the wrong instant. If the await lives in the handler,
//! cancelling it drops the receipt mid-commit: the transaction may still land,
//! and nothing is left to remove the entry from memory or to release the lease.
//! The daemon would then hold a ledger that says the verdict is cleared and a
//! registry that says it is not, with no task alive to reconcile them.
//!
//! So the settlement does not live in the request. Once the batch is ADMITTED,
//! ownership of the reservation, the receipt, and the router handle transfers to
//! a task this module owns, spawned BEFORE the first await. The handler keeps
//! only a result receiver; dropping that cancels nothing.
//!
//! # What shutdown owes it
//!
//! A settlement task is the one kind of work that must not be abandoned at
//! shutdown: it is holding a lease and owes the registry either a finalize or a
//! release. So shutdown stops admitting new purges, waits for the in-flight ones
//! WHILE THE WRITER IS STILL ALIVE (their commits need it), and only then drains
//! the writer. Draining first would leave every in-flight settlement unable to
//! commit.
//!
//! # When a settlement cannot be accounted for
//!
//! A task that panics, or whose join fails, after its batch was admitted leaves
//! the daemon unable to say whether the clear committed -- and therefore unable to
//! say whether its registry agrees with its ledger for that key. That is
//! ambiguous routing state, so the daemon does not continue serving on it: the
//! failure is logged at ERROR and terminal shutdown is triggered. A restart reads
//! the ledger and is authoritative again.

use std::sync::Arc;

use routectl_router::Router;
use routectl_usage::{BatchCommit, BatchReceipt};

/// The outcome the HTTP handler waits for.
///
/// Deliberately NOT the raw [`BatchCommit`]: the handler answers on what the
/// settlement DID (was the entry removed, was it left acting), and a commit
/// outcome alone does not say that -- a committed batch whose finalize found a
/// changed entry removed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementOutcome {
    /// The clear committed and the entry was removed.
    Purged,
    /// The clear did not commit, or the finalize refused: the entry is unchanged
    /// and still acting, and the lease is released.
    Failed(BatchCommit),
    /// The clear committed but the resident entry no longer matched what the
    /// reservation captured, so nothing was removed. Distinguished from
    /// `Failed` because the ledger DID receive the clear.
    Superseded,
}

/// Tracks in-flight settlement tasks so shutdown can wait for them.
///
/// Bounded by construction rather than by a cap: a settlement exists only for a
/// purge that reserved a key, and the per-key lease admits one at a time, so the
/// count cannot exceed the number of distinct keys an operator is purging at
/// once.
pub struct SettlementTracker {
    /// `{closed, in_flight}` under ONE mutex, because the two are read and
    /// written together and every interesting property is about their
    /// combination.
    ///
    /// Two atomics were wrong here: `settle` checked `closed` and then
    /// incremented `in_flight`, so a claimant admitted just before a close could
    /// increment AFTER `close_and_wait` had already observed zero -- and shutdown
    /// would drain the writer out from under a settlement that was about to need
    /// it. Under one lock the claim and the close cannot interleave: a claimant
    /// either takes its slot before the close (and shutdown waits for it) or
    /// finds the tracker closed (and never starts).
    state: Arc<std::sync::Mutex<TrackerState>>,
    /// Signalled whenever a settlement finishes, so shutdown waits without
    /// polling.
    finished: Arc<tokio::sync::Notify>,
    /// Fires when a settlement could not be accounted for. Held as a sender so
    /// the server loop can act on it (terminal shutdown) without this module
    /// knowing how the server shuts down.
    ambiguous: tokio::sync::mpsc::UnboundedSender<()>,
}

/// The tracker's two inseparable facts.
#[derive(Debug, Default)]
struct TrackerState {
    /// No further settlement may be admitted: a new one could outlive the writer
    /// whose commit it needs.
    closed: bool,
    /// Settlements that have CLAIMED a slot. Claimed before the durable batch is
    /// admitted, so a claim always precedes the obligation it accounts for.
    in_flight: usize,
}

/// A claimed tracker slot: the right to run one settlement.
///
/// Claimed BEFORE the batch is admitted, which is the ordering that makes
/// shutdown correct -- if the claim succeeds, `close_and_wait` waits for this
/// settlement; if it fails, no batch was ever admitted and there is no lease
/// obligation to account for.
///
/// Releasing is [`SettlementTracker::settle`]'s job (it transfers the claim into
/// the spawned task) or the drop below (the caller never got that far).
#[must_use = "a claimed settlement slot must be spent or released"]
pub struct SettlementClaim {
    state: Arc<std::sync::Mutex<TrackerState>>,
    finished: Arc<tokio::sync::Notify>,
    ambiguous: tokio::sync::mpsc::UnboundedSender<()>,
    /// Set once the claim has been handed to a settlement task, so the drop below
    /// does not release a slot the task now owns.
    spent: bool,
}

impl Drop for SettlementClaim {
    fn drop(&mut self) {
        if self.spent {
            return;
        }
        // The caller claimed a slot and then did not settle -- an admission
        // failure, typically. Release it: no batch was admitted, so there is no
        // obligation, and holding the slot would make shutdown wait for nothing.
        release(&self.state, &self.finished);
    }
}

/// Decrement the in-flight count and wake any waiter.
fn release(state: &std::sync::Mutex<TrackerState>, finished: &tokio::sync::Notify) {
    let mut guard = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.in_flight = guard.in_flight.saturating_sub(1);
    drop(guard);
    finished.notify_waiters();
}

impl SettlementTracker {
    /// Build a tracker plus the receiver that fires on an unaccounted-for
    /// settlement.
    #[must_use]
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (ambiguous, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                state: Arc::new(std::sync::Mutex::new(TrackerState::default())),
                finished: Arc::new(tokio::sync::Notify::new()),
                ambiguous,
            },
            rx,
        )
    }

    /// In-flight settlement count, for tests and diagnostics.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.locked().in_flight
    }

    /// Whether the tracker has stopped admitting settlements.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.locked().closed
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claim a slot, BEFORE admitting the durable batch.
    ///
    /// `None` when the tracker is closed: shutdown is in progress, so the caller
    /// must not admit a batch at all -- a settlement started now could outlive the
    /// writer whose commit it needs, and the operator is told to retry against
    /// state that never moved.
    pub fn claim(&self) -> Option<SettlementClaim> {
        let mut state = self.locked();
        if state.closed {
            return None;
        }
        state.in_flight += 1;
        drop(state);
        Some(SettlementClaim {
            state: Arc::clone(&self.state),
            finished: Arc::clone(&self.finished),
            ambiguous: self.ambiguous.clone(),
            spent: false,
        })
    }

    /// Spend a claim: take ownership of the admitted purge and settle it on a
    /// daemon-owned task.
    ///
    /// The claim was taken before admission, so the slot this spends is already
    /// counted -- shutdown is already waiting for it. Returns the receiver the
    /// caller waits on; dropping that cancels nothing.
    pub fn settle(
        &self,
        mut claim: SettlementClaim,
        router: Arc<Router>,
        reserved: Box<routectl_router::router::ReservedPurge>,
        receipt: BatchReceipt,
    ) -> tokio::sync::oneshot::Receiver<SettlementOutcome> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        claim.spent = true;
        let guard = AccountingGuard {
            state: Arc::clone(&claim.state),
            finished: Arc::clone(&claim.finished),
            ambiguous: claim.ambiguous.clone(),
            accounted: false,
        };
        tokio::spawn(async move {
            // The reservation is settled on EVERY path, including a panic unwind
            // through the guard: a dropped lease with no settlement would leave
            // the key frozen for the process's life.
            let outcome = settle_owned(&router, reserved, receipt).await;
            // Sending is best effort: the HTTP caller may be long gone, and its
            // absence must not change what the settlement did.
            let _ = tx.send(outcome);
            drop(guard.accounted_for());
        });
        rx
    }

    /// Drop a claim's accounting guard WITHOUT marking it accounted. Test-only.
    ///
    /// Reproduces what a panicking settlement task produces: the runtime catches
    /// the panic, so the guard's drop during unwind is the observable seam, and
    /// an unreachable-by-design report is exactly the kind that rots.
    #[cfg(test)]
    pub(crate) fn drop_unaccounted_guard_for_tests(&self, mut claim: SettlementClaim) {
        claim.spent = true;
        drop(AccountingGuard {
            state: Arc::clone(&claim.state),
            finished: Arc::clone(&claim.finished),
            ambiguous: claim.ambiguous.clone(),
            accounted: false,
        });
    }

    /// Drop a claim's accounting guard AS ACCOUNTED. Test-only control for the
    /// case above.
    #[cfg(test)]
    pub(crate) fn drop_accounted_guard_for_tests(&self, mut claim: SettlementClaim) {
        claim.spent = true;
        drop(
            AccountingGuard {
                state: Arc::clone(&claim.state),
                finished: Arc::clone(&claim.finished),
                ambiguous: claim.ambiguous.clone(),
                accounted: false,
            }
            .accounted_for(),
        );
    }

    /// Stop admitting settlements and wait for the in-flight ones.
    ///
    /// Called at shutdown BEFORE the writer drains: an in-flight settlement is
    /// waiting on a commit, so draining first would strand it.
    ///
    /// Closing and reading the count happen under ONE lock acquisition, so this
    /// cannot observe zero while a pre-close claimant is still about to increment.
    pub async fn close_and_wait(&self, deadline: std::time::Duration) {
        {
            let mut state = self.locked();
            state.closed = true;
        }
        let waited = tokio::time::timeout(deadline, async {
            loop {
                // The notified future is created BEFORE the count is re-read, so a
                // completion landing between the two still wakes this wait rather
                // than being missed.
                let woken = self.finished.notified();
                if self.in_flight() == 0 {
                    return;
                }
                woken.await;
            }
        })
        .await;
        if waited.is_err() {
            // Not silently continued: an unfinished settlement holds a lease and
            // owes the registry a settlement, so a timeout here is the same
            // ambiguity a panic is.
            tracing::error!(
                in_flight = self.in_flight(),
                deadline_secs = deadline.as_secs(),
                "capability purge settlements did not finish before the shutdown deadline; \
                 the registry and the ledger may disagree for those keys until the next boot",
            );
        }
    }
}

/// Decrements the in-flight count and notifies, and reports an UNACCOUNTED
/// settlement if dropped without being marked.
///
/// A settlement task that panics unwinds through this drop, which is how a panic
/// becomes a reported ambiguity rather than a silently missing decrement.
struct AccountingGuard {
    state: Arc<std::sync::Mutex<TrackerState>>,
    finished: Arc<tokio::sync::Notify>,
    ambiguous: tokio::sync::mpsc::UnboundedSender<()>,
    accounted: bool,
}

impl AccountingGuard {
    /// Mark the settlement as accounted for, so the drop below is the ordinary
    /// path rather than the ambiguity report.
    const fn accounted_for(mut self) -> Self {
        self.accounted = true;
        self
    }
}

impl Drop for AccountingGuard {
    fn drop(&mut self) {
        release(&self.state, &self.finished);
        if !self.accounted {
            tracing::error!(
                "a capability purge settlement did not complete after its batch was admitted; \
                 the daemon cannot tell whether the clear committed, so it is shutting down \
                 rather than serving on routing state it cannot account for",
            );
            // Best effort: if the receiver is gone the server is already stopping.
            let _ = self.ambiguous.send(());
        }
    }
}

/// Await the commit and settle the reservation. Runs on the daemon's task, so no
/// client can cancel it.
async fn settle_owned(
    router: &Router,
    reserved: Box<routectl_router::router::ReservedPurge>,
    receipt: BatchReceipt,
) -> SettlementOutcome {
    match receipt.await_outcome().await {
        BatchCommit::Committed { .. } => {
            if router.finalize_learned_capability_purge(reserved) {
                SettlementOutcome::Purged
            } else {
                // Committed but nothing removed: the resident entry no longer
                // matched what the reservation captured. The ledger has the
                // clear, so this is not a durability failure -- it is a
                // superseded purge, and the caller is told so distinctly.
                SettlementOutcome::Superseded
            }
        }
        failure => {
            router.abandon_learned_capability_purge(reserved);
            SettlementOutcome::Failed(failure)
        }
    }
}

#[cfg(test)]
#[path = "purge_settlement_tests.rs"]
mod tests;
