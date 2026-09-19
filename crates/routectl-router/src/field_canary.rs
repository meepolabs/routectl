//! Per-verdict canary and confirmation-quorum state for envelope-field
//! verdicts, shared behind an `Arc` alongside [`FieldVerdictRegistry`]'s
//! own in-flight set.
//!
//! This module is STATE ONLY: it holds nothing an eligibility rule or a
//! dispatch decision computes, it only records the atomic counters and
//! claims those decisions read and mutate. No new table, schema version,
//! or parallel persisted store backs it -- every field here is either
//! reconciled from the existing learned-capability row's own
//! `observations` counter (the confirmation count) or is purely
//! request-local coordination that a cold boot correctly starts empty
//! (the cadence countdown, the canary claim, the outstanding-request
//! count, the last canary outcome).
//!
//! # Why one shared map, not fields on [`FieldVerdictKey`]'s resident row
//!
//! The learned-capability row is the durable, replicated truth and is
//! read model plus decay engine for every capability class, not only
//! field verdicts. Bolting canary/quorum bookkeeping onto it would leak
//! a field-verdict-specific concern into a shared store every other
//! capability class also relies on. Keeping it here, keyed on the same
//! [`FieldVerdictKey`] identity, gets the same generation-independent
//! catalog-independence field verdicts already have (see the
//! `field_verdict` module docs) without touching that shared store's
//! shape.
//!
//! # Incarnation discipline
//!
//! Each per-key state carries the incarnation it was seeded for. A
//! caller that presents a DIFFERENT incarnation than the resident state
//! is observing a fresh verdict lifecycle for the same identity (the old
//! one was cleared and re-learned), so the resident state is replaced
//! wholesale rather than patched -- a stale cadence countdown or a
//! confirmation count left over from a verdict that no longer exists
//! must never leak into the new one's eligibility math.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::config::CANARY_INTERVAL;
use crate::field_verdict::FieldVerdictKey;

/// The result of one settled canary probe for an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryOutcome {
    /// The tested field rejected as expected AND the repaired retry
    /// succeeded: the resident verdict is re-confirmed.
    Confirmed,
    /// The tested field was accepted unrepaired: the resident verdict no
    /// longer reflects live upstream behavior and every request repaired
    /// since the last confirmation was affected by it.
    Regressed,
    /// The probe proved nothing either way -- an unrelated rejection, auth,
    /// rate limit, availability failure, unlocalized error, or a repaired
    /// retry that itself failed. The verdict stands and another bounded
    /// interval is scheduled.
    Inconclusive,
}

/// Resident per-identity canary/quorum state.
#[derive(Debug, Clone, Copy)]
struct CanaryState {
    /// The verdict incarnation this state was seeded for. See module docs.
    incarnation: u64,
    /// The confirmation count backing later eligibility (quorum) checks,
    /// reconciled from the learned-capability row's own acknowledged
    /// `observations` counter -- never an independent tally.
    confirmations: u32,
    /// Eligible-request countdown until the next due canary. Reset to
    /// [`CANARY_INTERVAL`] on seed and on every trip.
    cadence: u32,
    /// How many requests are currently applying this identity's repair.
    outstanding: u64,
    /// How many requests have applied this identity's repair since its last
    /// confirmation -- the wrong-repair exposure a later disproof charges to
    /// the lifetime alarm.
    ///
    /// Deliberately NOT the same number as `outstanding`, which falls again
    /// as each request finishes. This one only ever rises within an
    /// incarnation, and falls to zero exactly on a confirmation (those
    /// requests are now vouched for) or on a disproof (they are transferred
    /// to the alarm).
    modified_since_confirmation: u64,
    /// Whether the single canary slot for this identity is claimed.
    canary_claimed: bool,
    /// Whether pre-flight is suspended for this identity because a canary
    /// disproved its verdict.
    ///
    /// Set the instant an unrepaired canary succeeds, BEFORE the durable
    /// clear is attempted: the clear can be refused (a purge lease, a stale
    /// generation, an unhealthy writer), and a verdict known to be wrong must
    /// stop moving traffic immediately rather than while a retry is
    /// negotiated. Only dropping the identity's state lifts it.
    preflight_suspended: bool,
    /// The most recently settled canary's outcome, if any.
    last_outcome: Option<CanaryOutcome>,
}

impl CanaryState {
    /// The CONFIRMED state transition, shared by both settlement paths so the
    /// two cannot drift: restart the full interval and VOUCH for every request
    /// repaired since the previous confirmation, so a later disproof cannot
    /// charge them a second time.
    const fn apply_confirmed(&mut self) {
        self.cadence = CANARY_INTERVAL;
        self.modified_since_confirmation = 0;
    }

    /// The REGRESSED state transition, shared by both settlement paths so the
    /// two cannot drift: suspend pre-flight and TAKE the affected-request tally
    /// for the caller to charge to the lifetime alarm. Transferred rather than
    /// copied, so a second disproof of the same lifecycle cannot charge the same
    /// requests twice.
    const fn apply_regressed(&mut self) -> u64 {
        self.preflight_suspended = true;
        std::mem::replace(&mut self.modified_since_confirmation, 0)
    }

    const fn fresh(incarnation: u64, confirmations: u32) -> Self {
        Self {
            incarnation,
            confirmations,
            cadence: CANARY_INTERVAL,
            outstanding: 0,
            modified_since_confirmation: 0,
            canary_claimed: false,
            preflight_suspended: false,
            last_outcome: None,
        }
    }
}

/// A read-only view of one identity's resident canary state, for callers
/// that need more than one field at once without holding the lock across
/// several calls.
///
/// Read by the field pre-flight eligibility check
/// (the field-verdict registry's `preflight_eligible_incarnation`) and
/// by tests that need more than one field at once without holding the lock
/// across several calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanaryStateSnapshot {
    pub incarnation: u64,
    pub confirmations: u32,
    pub cadence: u32,
    pub outstanding: u64,
    pub modified_since_confirmation: u64,
    pub canary_claimed: bool,
    pub preflight_suspended: bool,
    pub last_outcome: Option<CanaryOutcome>,
}

/// Shared, per-[`FieldVerdictKey`] canary and confirmation-quorum state.
///
/// Every mutating operation takes the identity's slot under one lock for
/// its whole critical section, so a countdown trip, a claim, or an
/// incarnation reseed is atomic against a concurrent caller on the same
/// key -- required for the claim to mean "exactly one in-flight canary"
/// under real concurrency, not merely under a single thread.
#[derive(Debug, Default)]
pub struct FieldCanaryRegistry {
    states: Mutex<HashMap<FieldVerdictKey, CanaryState>>,
    /// Monotonic process-lifetime count of requests that applied a repair a
    /// canary later disproved, summed across every identity and every
    /// incarnation.
    ///
    /// Outside the per-key map deliberately: a disproved verdict's own state
    /// is dropped by the clear that follows, and an alarm that vanished with
    /// it would report zero exactly when an operator needs the number. It
    /// counts REQUESTS AFFECTED, never canary attempts -- one disproof of a
    /// verdict that repaired forty requests charges forty.
    disproved_requests_total: AtomicU64,
}

/// How a caller's incarnation relates to the one resident for its identity.
///
/// Incarnation transitions are MONOTONIC: state only ever moves forward. A
/// caller arriving with an older incarnation than the resident one is a
/// straggler from a superseded lifecycle -- its request was planned before a
/// confirmation carried the identity forward -- and it must not write, because
/// every field it would touch (confirmations, the affected-request tally, a live
/// claim, the cadence countdown, the incarnation itself) describes the CURRENT
/// lifecycle. A direction-blind reseed would let it reset all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncarnationOrder {
    /// The caller names a newer lifecycle: reseed the slot for it.
    Newer,
    /// The caller names the resident lifecycle: continue against it.
    Current,
    /// The caller names a superseded lifecycle: it may not mutate anything.
    Stale,
}

impl FieldCanaryRegistry {
    /// A fresh, empty registry. A cold rebuild does not leave it empty for
    /// long: [`Self::seed_from_rebuild`] repopulates every resident
    /// field-namespace identity's confirmation count, incarnation, and due
    /// state from the learned-capability row's own reconciled facts right
    /// after the ledger replay that produces this registry's caller. Absent
    /// a rebuild, a confirmation count is reconciled in later via
    /// [`Self::acknowledge_confirmation`], which only a caller holding a
    /// durable writer acknowledgment for the count may call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            disproved_requests_total: AtomicU64::new(0),
        }
    }

    /// Reconcile `entry` against a caller arriving at `incarnation`, reseeding
    /// the slot when the caller is NEWER and refusing it when the caller is
    /// older. See [`IncarnationOrder`].
    fn admit_incarnation(
        entry: &mut CanaryState,
        incarnation: u64,
        confirmations: u32,
    ) -> IncarnationOrder {
        match incarnation.cmp(&entry.incarnation) {
            std::cmp::Ordering::Greater => {
                *entry = CanaryState::fresh(incarnation, confirmations);
                IncarnationOrder::Newer
            }
            std::cmp::Ordering::Equal => IncarnationOrder::Current,
            std::cmp::Ordering::Less => IncarnationOrder::Stale,
        }
    }

    /// Reconcile the confirmation count for `key` at `incarnation` to
    /// `observations`. Returns the reconciled count.
    ///
    /// This is the sole write path for confirmations, and it exists for a
    /// caller that holds a DURABLE writer acknowledgment for `observations`
    /// -- a ledger write that has actually landed, not merely a
    /// `GenerationOutcome::Applied` admission through the in-memory
    /// generation barrier. `Applied` only means the shared registry
    /// accepted the mutation in memory; it says nothing about whether the
    /// event describing it has been durably written. No caller in this
    /// build holds that acknowledgment yet, so this is state-only surface
    /// for the durable-writer-ack caller that lands in a follow-up change --
    /// wiring it to `Applied` alone would let a later eligibility check
    /// observe a verdict as confirmed before its event write actually
    /// landed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn acknowledge_confirmation(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
        observations: u32,
    ) -> u32 {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, observations));
        if Self::admit_incarnation(entry, incarnation, observations) == IncarnationOrder::Stale {
            return entry.confirmations;
        }
        entry.confirmations = observations;
        entry.confirmations
    }

    /// Drop all resident state for `key`, e.g. because its verdict was
    /// cleared. The next repair for this identity starts a fresh
    /// incarnation with no inherited cadence, claim, or confirmation
    /// count.
    pub fn reset(&self, key: &FieldVerdictKey) {
        self.states.lock().remove(key);
    }

    /// Tick the eligible-request cadence countdown for `key` at
    /// `incarnation`. Returns `true` exactly on the tick that trips the
    /// countdown to zero, atomically resetting it to [`CANARY_INTERVAL`]
    /// in the same critical section -- so two concurrent callers can never
    /// both observe the trip for the same cycle.
    ///
    /// `false` for a caller whose `incarnation` is SUPERSEDED, without touching
    /// the countdown: a straggler from an older lifecycle must not move the
    /// current one's cadence, and reporting "not due" makes its planner fail
    /// open (forward the request unchanged) rather than claim a canary.
    pub fn tick_cadence(&self, key: &FieldVerdictKey, incarnation: u64) -> bool {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        if Self::admit_incarnation(entry, incarnation, 0) == IncarnationOrder::Stale {
            return false;
        }
        if entry.cadence <= 1 {
            entry.cadence = CANARY_INTERVAL;
            true
        } else {
            entry.cadence -= 1;
            false
        }
    }

    /// Mark one modified (repaired) request outstanding for `key` at
    /// `incarnation`, RAII: the IN-FLIGHT count is decremented on the
    /// returned guard's `Drop` regardless of how the request ends, so an
    /// early return, a `?`, or a client disconnect can never strand the count
    /// above zero.
    ///
    /// The wrong-repair TALLY is not decremented by that drop and must not be:
    /// it answers "how many requests did this verdict modify since it was last
    /// confirmed", which a finished request does not undo. Only a confirmation
    /// (they are vouched for) or a disproof (they are charged to the alarm)
    /// clears it.
    ///
    /// Reseeds the resident state for a NEWER `incarnation`, and returns `None`
    /// for a SUPERSEDED one without touching anything: a straggler from an older
    /// lifecycle must not raise the current lifecycle's tally, and its planner
    /// must fail open (forward unchanged) rather than apply a repair it cannot
    /// account for. Saturates rather than wraps, since neither count is a value
    /// any eligibility check divides by.
    pub fn begin_modified_request(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
    ) -> Option<ModifiedRequestGuard<'_>> {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        if Self::admit_incarnation(entry, incarnation, 0) == IncarnationOrder::Stale {
            return None;
        }
        entry.outstanding = entry.outstanding.saturating_add(1);
        entry.modified_since_confirmation = entry.modified_since_confirmation.saturating_add(1);
        Some(ModifiedRequestGuard {
            registry: self,
            key: key.clone(),
            incarnation,
        })
    }

    fn end_modified_request(&self, key: &FieldVerdictKey, incarnation: u64) {
        if let Some(entry) = self.states.lock().get_mut(key) {
            if entry.incarnation != incarnation {
                return;
            }
            entry.outstanding = entry.outstanding.saturating_sub(1);
        }
    }

    /// Claim the single in-flight canary slot for `key` at `incarnation`.
    /// `None` when a canary is already claimed for this identity at
    /// `incarnation`, and `None` for a SUPERSEDED `incarnation` -- a straggler
    /// from an older lifecycle must not be admitted alongside the live canary of
    /// the current one, which is exactly the second-concurrent-canary the slot
    /// exists to prevent.
    ///
    /// A NEWER `incarnation` reseeds the slot, so the fresh lifecycle starts
    /// unclaimed. Any claim still outstanding from the lifecycle it replaced
    /// stays sound because release and settlement are both incarnation-scoped
    /// (see [`Self::release_canary_claim`]) -- it can only ever retire its own
    /// incarnation's state, never the live slot that replaced it.
    pub fn claim_canary(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
    ) -> Option<CanaryClaimGuard<'_>> {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        if Self::admit_incarnation(entry, incarnation, 0) == IncarnationOrder::Stale {
            return None;
        }
        if entry.canary_claimed {
            return None;
        }
        entry.canary_claimed = true;
        Some(CanaryClaimGuard {
            registry: self,
            key: key.clone(),
            incarnation,
            settled: false,
        })
    }

    /// Settle a claim taken at `planned_incarnation` as CONFIRMED and move the
    /// identity's state onto `minted_incarnation` -- the incarnation the
    /// confirmation's own re-observation minted -- under ONE acquisition of the
    /// state lock.
    ///
    /// Atomic for a reason the two-step version could not satisfy: settling
    /// releases the claim, so a settle-then-carry sequence leaves a window in
    /// which a concurrent eligible request can claim a canary against the
    /// PRE-CARRY incarnation. That claim is then stranded -- the carry moves the
    /// lifecycle out from under it, and every effect it later attempts no-ops,
    /// including its release, so the slot stays occupied until the identity is
    /// reseeded.
    ///
    /// A no-op unless `planned_incarnation` is what is actually resident, which
    /// preserves the stale-settlement semantics of
    /// [`Self::settle_canary`]. The confirmation COUNT is carried untouched:
    /// raising it still requires a durable writer acknowledgment through
    /// [`Self::acknowledge_confirmation`].
    ///
    /// The IN-FLIGHT count is zeroed rather than carried. Every guard still
    /// outstanding was opened against `planned_incarnation` and drops
    /// incarnation-scoped, so carrying the number forward would leave a total no
    /// drop can ever reach. The wrong-repair TALLY is zeroed too, but for the
    /// unrelated reason every confirmation zeroes it: those requests are now
    /// vouched for.
    ///
    /// For what a stranded incarnation costs a later request, see
    /// `FieldVerdictRegistry::record_canary_confirmation`.
    ///
    /// COVERAGE LIMITATION, stated rather than papered over and scoped narrowly:
    /// what has no executable check is the single-LOCK property alone -- that
    /// these writes share one acquisition of `states` rather than two. Splitting
    /// release from carry requires releasing the guard between them, and there is
    /// no seam inside this critical section to interpose on; adding one would
    /// change the very structure under test, so a "mutation" that split the two
    /// would be testing a different function. No interposition seam is introduced
    /// here for that reason. Every individually observable EFFECT is pinned by a
    /// test that goes red when it is removed (the ownership guard, the cadence
    /// restart, the tally vouch, the in-flight zeroing, the incarnation move), as
    /// is every consequence of a split that a caller can observe (a straggler
    /// cannot tick, claim, or account against a carried identity; a stale claim
    /// confirms and observes nothing). The grouping of those effects under one
    /// lock is enforced by construction and by review, not by a test.
    pub(crate) fn settle_confirmed_and_carry(
        &self,
        key: &FieldVerdictKey,
        planned_incarnation: u64,
        minted_incarnation: u64,
    ) {
        let mut states = self.states.lock();
        let Some(entry) = states.get_mut(key) else {
            return;
        };
        if entry.incarnation != planned_incarnation {
            return;
        }
        entry.canary_claimed = false;
        entry.last_outcome = Some(CanaryOutcome::Confirmed);
        entry.apply_confirmed();
        // Every in-flight guard still outstanding was opened against
        // `planned_incarnation`, and its `Drop` is scoped to that incarnation
        // (which is what stops a straggler decrementing the lifecycle that
        // replaced it). Once the carry moves the slot to `minted_incarnation`
        // those drops can never match again, so a count left standing here
        // would report phantom in-flight traffic for the rest of the process.
        // The carry is the only place that knows the old lifecycle is over.
        entry.outstanding = 0;
        entry.incarnation = minted_incarnation;
    }

    /// Settle a canary claim taken at `incarnation`. Releases the claim and
    /// routes the outcome's own state changes only when `incarnation` still
    /// matches the resident state -- a settlement arriving after the identity
    /// moved to a new incarnation describes a verdict that no longer exists, so
    /// it touches nothing at all, exactly like a dropped, unsettled guard.
    /// Releasing unconditionally would free the slot a LIVE canary of the new
    /// incarnation holds, admitting a second concurrent canary for one identity.
    ///
    /// The three outcomes move three DIFFERENT sets of state, which is why the
    /// routing lives here rather than at the call site: a settlement that
    /// picked the wrong arm would either suspend a verdict nothing disproved,
    /// or charge the wrong-repair alarm for requests no repair affected.
    fn settle_canary(&self, key: &FieldVerdictKey, incarnation: u64, outcome: CanaryOutcome) {
        let mut states = self.states.lock();
        let Some(entry) = states.get_mut(key) else {
            return;
        };
        if entry.incarnation != incarnation {
            return;
        }
        entry.canary_claimed = false;
        entry.last_outcome = Some(outcome);
        match outcome {
            // Re-confirmed: restart the full interval and VOUCH for every
            // request repaired since the previous confirmation, so a later
            // disproof cannot charge them a second time.
            CanaryOutcome::Confirmed => entry.apply_confirmed(),
            // Disproved: suspend pre-flight for the identity IMMEDIATELY (the
            // durable clear that follows can be refused, and a verdict known
            // to be wrong must stop moving traffic while that is negotiated),
            // then TRANSFER the affected-request tally into the monotonic
            // lifetime alarm. Transferred rather than copied: leaving it
            // resident would let a second disproof of the same lifecycle
            // charge the same requests twice.
            CanaryOutcome::Regressed => {
                let affected = entry.apply_regressed();
                self.disproved_requests_total
                    .fetch_add(affected, Ordering::Relaxed);
            }
            // Proved nothing: schedule another bounded interval and leave the
            // verdict, the tally, and the alarm exactly as they were.
            CanaryOutcome::Inconclusive => {
                entry.cadence = CANARY_INTERVAL;
            }
        }
    }

    /// Settle a claim taken at `incarnation` as DISPROVED, reporting whether it
    /// actually applied -- `false` for a claim whose lifecycle has been
    /// superseded, which touches NOTHING AT ALL.
    ///
    /// Ownership is a precondition for every effect here, the slot release
    /// included. The slot belongs to whichever lifecycle is resident, so freeing
    /// it on behalf of a superseded claim would free the claim a LIVE canary of
    /// the current lifecycle holds -- admitting the second concurrent canary the
    /// slot exists to prevent. A superseded claim's release is a no-op for the
    /// same reason [`Self::release_canary_claim`] scopes its own: the state it
    /// described is gone, and the slot now belongs to a canary it must not
    /// disturb.
    ///
    /// The ownership test and the effects it authorizes (the release, the
    /// suspension, the tally transfer) happen under ONE acquisition of the state
    /// lock, and the return value is what a caller gates its own DURABLE effect
    /// on. A caller that asked `owns_current_incarnation` separately and then
    /// removed a row would be deciding on an answer a concurrent carry could
    /// already have invalidated -- and that removal is not incarnation-scoped, so
    /// it would take the CURRENT lifecycle's verdict out on the strength of a
    /// superseded canary's evidence.
    fn settle_disproved_if_current(&self, key: &FieldVerdictKey, incarnation: u64) -> bool {
        let mut states = self.states.lock();
        let Some(entry) = states.get_mut(key) else {
            return false;
        };
        if entry.incarnation != incarnation {
            return false;
        }
        entry.canary_claimed = false;
        entry.last_outcome = Some(CanaryOutcome::Regressed);
        let affected = entry.apply_regressed();
        self.disproved_requests_total
            .fetch_add(affected, Ordering::Relaxed);
        true
    }

    fn release_canary_claim(&self, key: &FieldVerdictKey, incarnation: u64) {
        if let Some(entry) = self.states.lock().get_mut(key) {
            if entry.incarnation != incarnation {
                return;
            }
            entry.canary_claimed = false;
        }
    }

    /// Seed resident canary state for `key` from a cold rebuild's resident
    /// learned-capability row, replacing whatever is resident wholesale
    /// (a rebuild is itself an incarnation-defining event, so there is
    /// never a "stale" case to reseed around here).
    ///
    /// `due_immediately` forces the cadence countdown to `1` -- the next
    /// eligible request trips a canary -- for a verdict the rebuild found
    /// already acting (routing away live traffic); every other rebuilt
    /// verdict starts its cadence at the normal [`CANARY_INTERVAL`].
    pub(crate) fn seed_from_rebuild(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
        confirmations: u32,
        due_immediately: bool,
    ) {
        let mut state = CanaryState::fresh(incarnation, confirmations);
        if due_immediately {
            state.cadence = 1;
        }
        self.states.lock().insert(key.clone(), state);
    }

    /// A read-only snapshot of `key`'s resident state, or `None` when
    /// nothing is resident for it.
    #[must_use]
    pub fn snapshot(&self, key: &FieldVerdictKey) -> Option<CanaryStateSnapshot> {
        self.states.lock().get(key).map(|s| CanaryStateSnapshot {
            incarnation: s.incarnation,
            confirmations: s.confirmations,
            cadence: s.cadence,
            outstanding: s.outstanding,
            modified_since_confirmation: s.modified_since_confirmation,
            canary_claimed: s.canary_claimed,
            preflight_suspended: s.preflight_suspended,
            last_outcome: s.last_outcome,
        })
    }

    /// The CURRENT outstanding unconfirmed-pre-flight exposure: requests
    /// modified since their verdict's last confirmation, summed over every
    /// resident identity.
    ///
    /// The paired half of [`Self::disproved_requests_total`], and reported
    /// beside it deliberately: this one is exposure that MIGHT later be
    /// disproved, that one is exposure that WAS. Reporting either alone reads
    /// as the other.
    #[must_use]
    pub fn outstanding_unconfirmed_total(&self) -> u64 {
        self.states.lock().values().fold(0u64, |sum, s| {
            sum.saturating_add(s.modified_since_confirmation)
        })
    }

    /// The monotonic lifetime count of requests that applied a repair a canary
    /// later disproved. Never falls, including across a clear or a reload.
    #[must_use]
    pub fn disproved_requests_total(&self) -> u64 {
        self.disproved_requests_total.load(Ordering::Relaxed)
    }
}

/// RAII outstanding-modified-request marker. `Drop` decrements the IN-FLIGHT
/// count for the incarnation the accounting was opened at, on every exit path --
/// there is no separate settlement, because an in-flight count has no outcome to
/// record.
///
/// The decrement is incarnation-SCOPED rather than unconditional: a guard that
/// outlives its lifecycle must not decrement the count belonging to the lifecycle
/// that replaced it. Its own lifecycle's count is not stranded by that scoping,
/// because the carry that supersedes it zeroes what it abandons (see
/// [`FieldCanaryRegistry::settle_confirmed_and_carry`]).
///
/// The wrong-repair tally the same `begin` raised is deliberately NOT touched
/// here: see [`FieldCanaryRegistry::begin_modified_request`].
#[derive(Debug)]
pub struct ModifiedRequestGuard<'a> {
    registry: &'a FieldCanaryRegistry,
    key: FieldVerdictKey,
    /// The incarnation the accounting was opened at. The in-flight decrement is
    /// scoped to it, so a guard outliving its lifecycle cannot decrement a
    /// counter belonging to the lifecycle that replaced it.
    incarnation: u64,
}

impl Drop for ModifiedRequestGuard<'_> {
    fn drop(&mut self) {
        self.registry
            .end_modified_request(&self.key, self.incarnation);
    }
}

/// The single-flight canary claim for one identity.
///
/// [`settle`](Self::settle) is the only way to record an outcome. Dropping
/// the guard without settling -- an early return, a `?`, a client
/// disconnect -- releases the claim while leaving the last recorded outcome and
/// confirmation count untouched.
///
/// Every effect is scoped to the incarnation the claim was taken at, the release
/// included. Once the identity has moved on, this guard's release and its
/// settlements alike touch nothing: the state it described is gone, and the slot
/// now belongs to a canary it must not disturb.
#[derive(Debug)]
pub struct CanaryClaimGuard<'a> {
    registry: &'a FieldCanaryRegistry,
    key: FieldVerdictKey,
    incarnation: u64,
    settled: bool,
}

impl CanaryClaimGuard<'_> {
    /// Settle the claim with `outcome`. Releases the slot and routes the
    /// outcome's state changes only if this guard's incarnation still matches
    /// the resident state (see [`FieldCanaryRegistry::settle_canary`]).
    pub fn settle(mut self, outcome: CanaryOutcome) {
        self.settled = true;
        self.registry
            .settle_canary(&self.key, self.incarnation, outcome);
    }

    /// Whether this claim still names the identity's CURRENT canary
    /// incarnation.
    ///
    /// Checked before a settlement does anything observable -- a learned-registry
    /// observation refreshes decay and increments the row's `observations`, so a
    /// stale claim reaching that call would corroborate a lifecycle it never
    /// tested. A `false` here means the only correct action is to release.
    #[must_use]
    pub(crate) fn owns_current_incarnation(&self) -> bool {
        self.registry
            .states
            .lock()
            .get(&self.key)
            .is_some_and(|entry| entry.incarnation == self.incarnation)
    }

    /// Settle CONFIRMED and move the identity onto `minted_incarnation` in one
    /// critical section (see
    /// [`FieldCanaryRegistry::settle_confirmed_and_carry`]).
    ///
    /// Not reachable through [`Self::settle`]: the confirmed arm needs the
    /// minted incarnation, and splitting it into a settle plus a carry is the
    /// race this method exists to close.
    pub(crate) fn settle_confirmed_and_carry(mut self, minted_incarnation: u64) {
        self.settled = true;
        self.registry
            .settle_confirmed_and_carry(&self.key, self.incarnation, minted_incarnation);
    }

    /// Settle DISPROVED, reporting whether the claim owned the current
    /// lifecycle (see
    /// [`FieldCanaryRegistry::settle_disproved_if_current`]).
    ///
    /// Not reachable through [`Self::settle`]: the disproved arm's caller owes a
    /// DURABLE clear, and that clear names only the identity, not the lifecycle.
    /// Returning the ownership verdict from the same critical section that
    /// performs the suspension is what lets the caller refuse to remove a row a
    /// carry moved out from under this canary.
    #[must_use]
    pub(crate) fn settle_disproved_if_current(mut self) -> bool {
        self.settled = true;
        self.registry
            .settle_disproved_if_current(&self.key, self.incarnation)
    }

    /// Release the claim without recording any outcome. Test-only: the
    /// dispatch path reaches this same effect by dropping an unsettled
    /// guard (see [`Drop`]), which is what makes an early return settle
    /// correctly with no call site to forget.
    #[cfg(test)]
    pub fn release(mut self) {
        self.settled = true;
        self.registry
            .release_canary_claim(&self.key, self.incarnation);
    }
}

impl Drop for CanaryClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.registry
                .release_canary_claim(&self.key, self.incarnation);
        }
    }
}

#[cfg(test)]
#[path = "field_canary_tests.rs"]
mod tests;
