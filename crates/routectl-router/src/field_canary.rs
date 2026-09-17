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

use parking_lot::Mutex;

use crate::config::CANARY_INTERVAL;
use crate::field_verdict::FieldVerdictKey;

/// The result of one settled canary probe for an identity.
///
/// Not yet constructed outside tests: the dispatch path that settles a
/// live canary lands in a follow-up change; this module is state-only.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryOutcome {
    /// The tested field rejected as expected: the resident verdict still
    /// holds.
    Confirmed,
    /// The tested field was accepted: the resident verdict no longer
    /// reflects live upstream behavior.
    Regressed,
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
    /// Whether the single canary slot for this identity is claimed.
    canary_claimed: bool,
    /// The most recently settled canary's outcome, if any.
    last_outcome: Option<CanaryOutcome>,
}

impl CanaryState {
    const fn fresh(incarnation: u64, confirmations: u32) -> Self {
        Self {
            incarnation,
            confirmations,
            cadence: CANARY_INTERVAL,
            outstanding: 0,
            canary_claimed: false,
            last_outcome: None,
        }
    }
}

/// A read-only view of one identity's resident canary state, for callers
/// that need more than one field at once without holding the lock across
/// several calls.
///
/// Not yet read outside tests: the eligibility rule and dispatch decision
/// that consume it land in a follow-up change; this module is state-only.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanaryStateSnapshot {
    pub incarnation: u64,
    pub confirmations: u32,
    pub cadence: u32,
    pub outstanding: u64,
    pub canary_claimed: bool,
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
        }
    }

    const fn reseed_if_stale(entry: &mut CanaryState, incarnation: u64, confirmations: u32) {
        if entry.incarnation != incarnation {
            *entry = CanaryState::fresh(incarnation, confirmations);
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
        Self::reseed_if_stale(entry, incarnation, observations);
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tick_cadence(&self, key: &FieldVerdictKey, incarnation: u64) -> bool {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        Self::reseed_if_stale(entry, incarnation, 0);
        if entry.cadence <= 1 {
            entry.cadence = CANARY_INTERVAL;
            true
        } else {
            entry.cadence -= 1;
            false
        }
    }

    /// Mark one modified (repaired) request outstanding for `key` at
    /// `incarnation`, RAII: the count is decremented on the returned
    /// guard's `Drop` regardless of how the request ends, so an early
    /// return, a `?`, or a client disconnect can never strand the count
    /// above zero.
    ///
    /// Reseeds the resident state when `incarnation` does not match --
    /// see module docs -- and saturates rather than wraps, since the
    /// outstanding count is diagnostic bookkeeping, not a value any
    /// eligibility check divides by.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn begin_modified_request(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
    ) -> ModifiedRequestGuard<'_> {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        Self::reseed_if_stale(entry, incarnation, 0);
        entry.outstanding = entry.outstanding.saturating_add(1);
        ModifiedRequestGuard {
            registry: self,
            key: key.clone(),
        }
    }

    fn end_modified_request(&self, key: &FieldVerdictKey) {
        if let Some(entry) = self.states.lock().get_mut(key) {
            entry.outstanding = entry.outstanding.saturating_sub(1);
        }
    }

    /// Claim the single in-flight canary slot for `key` at `incarnation`.
    /// `None` when a canary is already claimed for this identity (at any
    /// incarnation -- a claim on a since-superseded incarnation still
    /// occupies the slot until its guard settles or drops).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn claim_canary(
        &self,
        key: &FieldVerdictKey,
        incarnation: u64,
    ) -> Option<CanaryClaimGuard<'_>> {
        let mut states = self.states.lock();
        let entry = states
            .entry(key.clone())
            .or_insert_with(|| CanaryState::fresh(incarnation, 0));
        Self::reseed_if_stale(entry, incarnation, 0);
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

    /// Settle a canary claim taken at `incarnation`. Always releases the
    /// claim. Records `outcome` only when `incarnation` still matches the
    /// resident state -- a settlement arriving after the identity moved to
    /// a new incarnation is stale and releases the claim without mutating
    /// any verdict-facing state, exactly like a dropped, unsettled guard.
    fn settle_canary(&self, key: &FieldVerdictKey, incarnation: u64, outcome: CanaryOutcome) {
        if let Some(entry) = self.states.lock().get_mut(key) {
            entry.canary_claimed = false;
            if entry.incarnation == incarnation {
                entry.last_outcome = Some(outcome);
            }
        }
    }

    fn release_canary_claim(&self, key: &FieldVerdictKey) {
        if let Some(entry) = self.states.lock().get_mut(key) {
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn snapshot(&self, key: &FieldVerdictKey) -> Option<CanaryStateSnapshot> {
        self.states.lock().get(key).map(|s| CanaryStateSnapshot {
            incarnation: s.incarnation,
            confirmations: s.confirmations,
            cadence: s.cadence,
            outstanding: s.outstanding,
            canary_claimed: s.canary_claimed,
            last_outcome: s.last_outcome,
        })
    }
}

/// RAII outstanding-modified-request marker. Decrements the count on
/// `Drop`, unconditionally -- there is no separate settlement, because an
/// outstanding count has no outcome to record.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub struct ModifiedRequestGuard<'a> {
    registry: &'a FieldCanaryRegistry,
    key: FieldVerdictKey,
}

impl Drop for ModifiedRequestGuard<'_> {
    fn drop(&mut self) {
        self.registry.end_modified_request(&self.key);
    }
}

/// The single-flight canary claim for one identity.
///
/// [`settle`](Self::settle) is the only way to record an outcome. Dropping
/// the guard without settling -- an early return, a `?`, a client
/// disconnect -- releases the claim exactly as a stale settlement would,
/// leaving the last recorded outcome and confirmation count untouched.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub struct CanaryClaimGuard<'a> {
    registry: &'a FieldCanaryRegistry,
    key: FieldVerdictKey,
    incarnation: u64,
    settled: bool,
}

impl CanaryClaimGuard<'_> {
    /// Settle the claim with `outcome`. Releases the slot; records the
    /// outcome only if this guard's incarnation still matches the
    /// resident state (see [`FieldCanaryRegistry::settle_canary`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn settle(mut self, outcome: CanaryOutcome) {
        self.settled = true;
        self.registry
            .settle_canary(&self.key, self.incarnation, outcome);
    }

    /// Release the claim without recording any outcome. Test-only: the
    /// dispatch path reaches this same effect by dropping an unsettled
    /// guard (see [`Drop`]), which is what makes an early return settle
    /// correctly with no call site to forget.
    #[cfg(test)]
    pub fn release(mut self) {
        self.settled = true;
        self.registry.release_canary_claim(&self.key);
    }
}

impl Drop for CanaryClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.registry.release_canary_claim(&self.key);
        }
    }
}

#[cfg(test)]
#[path = "field_canary_tests.rs"]
mod tests;
