//! Daemon-owned advancement of acknowledged field-verdict confirmations.
//!
//! # The window this closes
//!
//! A learned envelope-field verdict becomes eligible to rewrite requests ahead of a
//! rejection only once the ledger row describing its confirmation is durable. So the
//! advancement has two halves: admit the row, then -- once the writer acknowledges it
//! -- tell the router, which re-validates the event's generation and incarnation
//! against live state before moving anything.
//!
//! The second half cannot live in the request. An HTTP handler's future is CANCELLED
//! when the client goes away -- a closed connection, a CLI that timed out, a `curl`
//! interrupted at the wrong instant -- and the await of the write's outcome is a
//! cancellation point. Cancelling there drops the receipt mid-commit: the transaction
//! still lands, so the row is on disk and the process has a durable confirmation it
//! never applied. The verdict then stays dormant until a restart replays the ledger,
//! which is precisely the restart dependency this feature exists to remove. Worse, it
//! is silently intermittent: it happens only to the requests whose clients hung up.
//!
//! So once the row is ADMITTED, ownership of the receipt and the event's identity
//! transfers to a task this module owns, spawned BEFORE the first await. The handler
//! keeps nothing; there is no receiver to drop, because no caller needs the answer.
//! This is the same structure `purge_settlement` uses against the same bug class, for
//! the same reason.
//!
//! # NO REQUEST RESPONSE WAITS ON SQLITE FOR THIS
//!
//! Which is the second thing the spawn buys, and it is not incidental. The previous
//! shape awaited the commit inline, so a request that happened to mint a verdict paid
//! a database round trip before its response went out -- on the rare tail, but paid
//! by the client rather than by the daemon. Now the handler admits and returns; the
//! advancement is the daemon's own work.
//!
//! # What shutdown owes it, and why that is LESS than a purge settlement owes
//!
//! A purge settlement holds a lease and owes the registry either a finalize or a
//! release, so abandoning one leaves state nothing can reconcile -- hence its
//! ambiguity report and terminal shutdown. This task owes nothing of the kind, and
//! the difference is worth stating precisely rather than copying the stricter
//! machinery:
//!
//! - it holds no lease, no reservation, and no registry claim;
//! - its only effect is to RAISE an acknowledged confirmation count, which is pure
//!   in-memory state derived from a row that is already durable;
//! - so a task abandoned at shutdown loses exactly the same thing a process that
//!   never had this feature lost -- the count is reconstructed from the ledger by the
//!   next boot's cold-rebuild seed, which is the DOCUMENTED recovery path and the one
//!   that was the only path before this change.
//!
//! Abandonment is therefore SAFE, and it fails in the safe direction: a verdict whose
//! advancement was abandoned is dormant rather than acting, so the worst outcome is
//! one extra round trip per request on that lane until the next restart.
//!
//! Shutdown still WAITS, briefly and boundedly, because finishing is strictly better
//! than abandoning and an admitted row's commit needs the writer alive. It waits
//! after the server stops accepting and before the writer drains, exactly where the
//! purge settlements are awaited. What it does NOT do is treat a timeout as ambiguous
//! routing state: it logs what was abandoned and continues, because the recovery is a
//! restart the operator is already performing.

use std::sync::Arc;

use routectl_router::Router;
use routectl_usage::CapabilityEventReceipt;

/// How long shutdown waits for in-flight confirmation advancements.
///
/// SHORT deliberately, and shorter than the purge deadline: abandoning one of these
/// costs a dormant verdict the next boot restores, while abandoning a purge
/// settlement leaves a lease nothing can reconcile. A long wait here would trade real
/// shutdown latency for a benefit the restart already provides.
pub const CONFIRMATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// The identity and stamps one advancement must present to the router.
///
/// Carried VERBATIM from the guarded registry mutation that produced the event --
/// nothing is re-derived on the task. A task that re-read the live incarnation would
/// be acknowledging whatever lifecycle is resident when it happens to run rather than
/// the one whose row committed, which is exactly the stale-acknowledgment case the
/// router's own generation and incarnation checks exist to refuse.
#[derive(Debug, Clone)]
pub struct ConfirmationIdentity {
    /// Breaker state key the event names.
    pub state_key: String,
    /// Normalized capability key the event names.
    pub capability_key: String,
    /// Provider-kind token the event names.
    pub provider_kind: String,
    /// The registry generation the producing mutation ran under.
    pub generation: u64,
    /// The incarnation of the key's state the event describes.
    pub incarnation: u64,
    /// The observation count the mutation produced.
    pub observations: u32,
}

/// Tracks in-flight confirmation advancements so shutdown can wait for them.
///
/// Bounded by construction rather than by a cap: one advancement exists per
/// field-verdict event a request actually minted, which is the rare tail of traffic
/// rather than a per-request cost -- a request carrying no field rejection produces no
/// event and claims nothing.
pub struct ConfirmationTracker {
    /// `{closed, in_flight}` under ONE mutex, for the reason
    /// `purge_settlement::SettlementTracker` documents at length: two atomics let a
    /// claimant admitted just before a close increment AFTER the close observed
    /// zero, and shutdown would then drain the writer out from under a task that
    /// still needs it. Under one lock the claim and the close cannot interleave.
    state: Arc<std::sync::Mutex<TrackerState>>,
    /// Signalled whenever an advancement finishes, so shutdown waits without
    /// polling.
    finished: Arc<tokio::sync::Notify>,
}

/// The tracker's two inseparable facts.
#[derive(Debug, Default)]
struct TrackerState {
    /// No further advancement may be admitted: one started now could outlive the
    /// writer whose commit it awaits.
    closed: bool,
    /// Advancements that have CLAIMED a slot. Claimed before the row is admitted, so
    /// a claim always precedes the work it accounts for.
    in_flight: usize,
}

/// A claimed tracker slot: the right to run one advancement.
///
/// Claimed BEFORE the row is admitted, which is the ordering that makes shutdown
/// correct -- if the claim succeeds, `close_and_wait` waits for this task; if it
/// fails, nothing was admitted and there is nothing to wait for.
#[must_use = "a claimed advancement slot must be spent or released"]
pub struct ConfirmationClaim {
    state: Arc<std::sync::Mutex<TrackerState>>,
    finished: Arc<tokio::sync::Notify>,
    /// Set once the claim has been handed to a task, so the drop below does not
    /// release a slot the task now owns.
    spent: bool,
}

impl Drop for ConfirmationClaim {
    fn drop(&mut self) {
        if self.spent {
            return;
        }
        // Claimed and then not spent -- an admission refusal, typically. Release it:
        // nothing was admitted, so holding the slot would make shutdown wait for
        // work that does not exist.
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

impl Default for ConfirmationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmationTracker {
    /// A fresh tracker, admitting and counting nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(TrackerState::default())),
            finished: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// In-flight advancement count, for tests and diagnostics.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.locked().in_flight
    }

    /// Whether the tracker has stopped admitting advancements.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.locked().closed
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claim a slot, BEFORE admitting the row.
    ///
    /// `None` when the tracker is closed: shutdown is in progress, so the caller
    /// must not admit at all. That refusal is SAFE rather than a loss -- the verdict
    /// simply stays dormant and the next boot's ledger replay seeds its count, which
    /// is the same recovery this feature's absence relied on entirely.
    pub fn claim(&self) -> Option<ConfirmationClaim> {
        self.claim_observing_closed().map(|(claim, _)| claim)
    }

    /// [`Self::claim`], additionally reporting the `closed` flag AS READ IN THE SAME
    /// critical section the slot was taken in.
    ///
    /// THE one implementation -- `claim` delegates here rather than the other way
    /// around, so the observation comes from the production path instead of a test-only
    /// copy that could drift from it.
    ///
    /// The flag exists for one reason: a slot taken after the close is the bug the
    /// single critical section prevents, and it is otherwise UNOBSERVABLE from outside.
    /// Such a claimant still releases before shutdown finishes, so every end-state
    /// assertion holds; and a `closed` read taken after `claim` RETURNS is racy by
    /// construction, since a close landing in that gap would wrongly accuse a
    /// legitimate pre-close claim. Read under the same guard as the increment, the
    /// answer is exact: with the pair atomic it is always `false` for a successful
    /// claim, and with the two split a claimant that read `closed` false can increment
    /// after the close set it and this reports `true`.
    ///
    /// Production ignores the flag (`claim` drops it), which is correct -- a caller has
    /// nothing to do about it.
    fn claim_observing_closed(&self) -> Option<(ConfirmationClaim, bool)> {
        let mut state = self.locked();
        if state.closed {
            return None;
        }
        state.in_flight += 1;
        let was_closed = state.closed;
        drop(state);
        Some((
            ConfirmationClaim {
                state: Arc::clone(&self.state),
                finished: Arc::clone(&self.finished),
                spent: false,
            },
            was_closed,
        ))
    }

    /// [`Self::claim_observing_closed`], for the hostile concurrency test.
    #[cfg(test)]
    pub(crate) fn claim_reporting_closed(&self) -> Option<(ConfirmationClaim, bool)> {
        self.claim_observing_closed()
    }

    /// Spend a claim: take ownership of the admitted row's receipt and advance the
    /// confirmation on a daemon-owned task.
    ///
    /// Returns NOTHING, deliberately. The purge settlement hands its caller a result
    /// receiver because an HTTP response depends on the outcome; nothing depends on
    /// this one, so handing back a receiver would invite a caller to await it and
    /// reintroduce the very cancellation point the spawn removes.
    ///
    /// The claim was taken before admission, so the slot this spends is already
    /// counted and shutdown is already waiting for it.
    pub fn advance(
        &self,
        mut claim: ConfirmationClaim,
        router: Arc<Router>,
        identity: ConfirmationIdentity,
        receipt: CapabilityEventReceipt,
    ) {
        claim.spent = true;
        let guard = AccountingGuard {
            state: Arc::clone(&claim.state),
            finished: Arc::clone(&claim.finished),
        };
        tokio::spawn(async move {
            advance_owned(&router, &identity, receipt).await;
            // Dropped explicitly at the end rather than left to scope exit, so the
            // decrement is visibly the last thing the task does. A panic unwinds
            // through the same drop, so the count cannot strand above zero and hold
            // shutdown for work that has already stopped.
            drop(guard);
        });
    }

    /// Stop admitting advancements and wait, boundedly, for the in-flight ones.
    ///
    /// Called at shutdown BEFORE the writer drains: an in-flight advancement is
    /// waiting on a commit, so draining first would strand it.
    ///
    /// A TIMEOUT IS NOT AN ERROR HERE, unlike its purge counterpart, and that
    /// asymmetry is the design rather than an oversight. An abandoned advancement
    /// loses only an in-memory count derived from a row that is already durable, so
    /// the next boot's cold-rebuild seed reconstructs it -- the documented recovery,
    /// and the only path that existed before this feature. It is logged at INFO with
    /// what was abandoned, so an operator reading a shutdown can tell which lanes
    /// will need one extra round trip until the restart completes.
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
            tracing::info!(
                in_flight = self.in_flight(),
                deadline_secs = deadline.as_secs(),
                "abandoning in-flight field-verdict confirmation advancements at \
                 shutdown; their rows are durable, so the next boot's ledger replay \
                 restores the counts and those lanes repair on rejection until then",
            );
        }
    }
}

/// Decrements the in-flight count and notifies.
///
/// No ambiguity report, unlike the purge settlement's guard: this task holds no lease
/// and owes the registry nothing, so a task that dies leaves a dormant verdict rather
/// than routing state the daemon cannot account for.
struct AccountingGuard {
    state: Arc<std::sync::Mutex<TrackerState>>,
    finished: Arc<tokio::sync::Notify>,
}

impl Drop for AccountingGuard {
    fn drop(&mut self) {
        release(&self.state, &self.finished);
    }
}

/// Await the commit and advance the confirmation. Runs on the daemon's task, so no
/// client can cancel it.
///
/// The router re-validates the generation and the incarnation against live state, so
/// a row that landed for a lifecycle since superseded advances nothing -- which is
/// why the identity is carried verbatim rather than re-read here.
async fn advance_owned(
    router: &Router,
    identity: &ConfirmationIdentity,
    receipt: CapabilityEventReceipt,
) {
    let outcome = receipt.await_outcome().await;
    if !outcome.is_durable() {
        tracing::debug!(
            outcome = outcome.as_str(),
            "field-verdict event did not land durably; its confirmation count is not \
             advanced and pre-flight stays dormant for this identity"
        );
        return;
    }
    let acknowledged = router.acknowledge_durable_field_confirmation(
        &identity.state_key,
        &identity.capability_key,
        &identity.provider_kind,
        identity.generation,
        identity.incarnation,
        identity.observations,
    );
    if acknowledged {
        tracing::debug!(
            outcome = outcome.as_str(),
            "field-verdict event landed durably; its acknowledged confirmation count \
             now backs pre-flight eligibility"
        );
    }
}

#[cfg(test)]
#[path = "confirmation_advance_tests.rs"]
mod tests;
