//! Operator-initiated purge of one keyed learned-capability entry.
//!
//! The counterpart to the probe-settled clear in [`super::capability_cleared`],
//! reached from an operator control path rather than from live traffic. Scope is
//! LEARNED entries only: a purge never edits, shadows, or invents an operator
//! override or a catalog prior, because a learned negative is an observation the
//! daemon made and dropping it says nothing about operator intent. An operator
//! who wants the decision to persist across relearning writes it in the
//! capability override configuration, which this path never touches.
//!
//! One case reaches past learned entries: a seeded beta flag with nothing
//! resident. The seed withholds it until an operator purge lifts it, so that
//! purge commits a `cleared` row and records the seed-clear marker
//! ([`PurgeOutcome::SeedLift`]) under the same reserve / commit / finalize
//! order below.
//!
//! # Why this is two calls and not one
//!
//! A purge is a memory mutation plus a SQLite transaction, and they cannot be
//! one atomic step: the transaction must not be awaited under a registry lock.
//! So the operation is split, and the ORDER is the contract:
//!
//! 1. [`Router::reserve_learned_capability_purge`] validates the generation,
//!    captures the resident entry and LEASES the key -- leaving the entry
//!    resident and acting.
//! 2. The caller commits the `cleared` settlement durably and waits for the
//!    acknowledgement. No registry lock is held across that await.
//! 3. Only on a committed acknowledgement does
//!    [`Router::finalize_learned_capability_purge`] remove the entry and release
//!    the lease. Any other outcome goes to
//!    [`Router::abandon_learned_capability_purge`], which releases the lease and
//!    leaves the entry exactly as it was.
//!
//! Reporting success before that commit is the failure this shape exists to
//! prevent: the operator would be told the verdict is gone while the ledger
//! still holds the negative, so it would keep steering routing until the next
//! boot and the warm rebuild would then resurrect it. Removing first and
//! re-inserting on failure is the other wrong answer -- it has to reconstruct
//! the entry from a capture, and any drift in that reconstruction is itself a
//! silent mutation.

use super::{CapabilityClearedEvent, Router};
use crate::learned_capability::{PurgeLease, PurgePreparation};
use crate::state_key::StateKey;

/// A reserved purge: the key is leased, the entry is still resident, and the
/// caller owes exactly one settlement.
///
/// Carries the keys the reservation was made on so the caller persists and logs
/// what the registry actually keyed on rather than the raw request values -- the
/// `capability_key` here is already normalized. No request body, prompt, or
/// upstream text ever enters it.
#[must_use = "a reserved purge must be finalized or abandoned"]
pub struct ReservedPurge {
    /// Serialized learned lane (`provider_entry#upstream`) the purge is keyed
    /// on.
    pub state_key: String,
    /// Normalized capability key the purge is keyed on.
    pub capability_key: String,
    /// Stable provider-kind token that normalized the capability key.
    pub provider_kind: String,
    lane: StateKey,
    lease: PurgeLease,
}

impl ReservedPurge {
    /// The EFFECTIVE generation the removal will run under, taken from the
    /// reservation itself rather than sampled separately: a value read before or
    /// after the guarded operation could straddle a reload boundary and stamp the
    /// settlement with a generation that does not describe the removal it
    /// reports.
    pub const fn generation(&self) -> u64 {
        self.lease.generation()
    }

    /// The INCARNATION this purge clears -- the version of the key the operator
    /// approved removing, and the floor the writer records on commit.
    ///
    /// The CAPTURED value, not a fresh read: a later read could observe a version
    /// the operator never saw, and the floor would then suppress events the purge
    /// had no authority over.
    pub const fn generation_incarnation(&self) -> u64 {
        self.lease.incarnation()
    }

    /// The `cleared` settlement to commit durably before finalizing.
    pub fn settlement(&self) -> CapabilityClearedEvent {
        CapabilityClearedEvent {
            // The CAPTURED incarnation: the clear describes exactly the version
            // of the key the operator approved removing, and the writer records
            // it as that key's purge floor.
            incarnation: self.lease.incarnation(),
            state_key: self.state_key.clone(),
            capability_key: self.capability_key.clone(),
            provider_kind: self.provider_kind.clone(),
            persistence_generation: self.lease.generation(),
        }
    }
}

/// A reserved seed lift: a purge of a seeded beta key with no resident entry,
/// whose seed still withholds the flag on this lane. The key is leased and the
/// caller owes exactly one settlement, as for [`ReservedPurge`].
///
/// Nothing is resident, so nothing is removed: the committed `cleared` row is
/// the durable record that the operator lifted the seed for this cell, and
/// finalizing records the in-memory marker the withheld pass honors.
#[must_use = "a reserved seed lift must be finalized or abandoned"]
pub struct ReservedSeedLift {
    /// Serialized learned lane (`provider_entry#upstream`) the lift is keyed on.
    pub state_key: String,
    /// Normalized capability key the lift is keyed on.
    pub capability_key: String,
    /// Stable provider-kind token that normalized the capability key.
    pub provider_kind: String,
    lease: PurgeLease,
}

impl ReservedSeedLift {
    /// The effective generation captured under the reservation's own guard.
    /// See [`ReservedPurge::generation`].
    pub const fn generation(&self) -> u64 {
        self.lease.generation()
    }

    /// The incarnation the lift's clear is submitted at: zero, because no
    /// version of the key is resident to supersede.
    pub const fn generation_incarnation(&self) -> u64 {
        self.lease.incarnation()
    }

    /// The `cleared` settlement to commit durably before finalizing.
    pub fn settlement(&self) -> CapabilityClearedEvent {
        CapabilityClearedEvent {
            incarnation: self.lease.incarnation(),
            state_key: self.state_key.clone(),
            capability_key: self.capability_key.clone(),
            provider_kind: self.provider_kind.clone(),
            persistence_generation: self.lease.generation(),
        }
    }
}

/// What a purge request resolved to. Every variant needs its own answer on the
/// wire, and collapsing any two loses something the operator needs: "already
/// gone" is not "ask the current router", and neither is "someone else is
/// purging this right now". Telling an operator an entry is gone is the answer a
/// refusal must never give.
#[must_use]
pub enum PurgeOutcome {
    /// Reserved: the caller commits the settlement and then finalizes.
    ///
    /// BOXED because the refusals carry nothing: the reservation
    /// holds a captured entry plus a lease, and leaving it inline would make
    /// every refusal pay its size. The refusals are also the common answers on a
    /// healthy daemon.
    Reserved(Box<ReservedPurge>),
    /// No resident entry, but the key is a seeded beta flag the seed still
    /// withholds on this lane: the caller commits the settlement and then
    /// finalizes, which lifts the seed for this cell.
    SeedLift(Box<ReservedSeedLift>),
    /// No resident entry under this key and no seed to lift -- a clean no-op.
    Absent,
    /// Another purge holds this key's lease.
    Busy,
    /// The request arrived through a superseded Router, whose registry the
    /// published Router no longer reads.
    Stale,
}

impl Router {
    /// Reserve `(lane, capability_key)` for an operator purge.
    ///
    /// Typed on [`StateKey`] so an operator string reaches the registry only
    /// through the lane's single parser: a target spelled any other way cannot
    /// be addressed, rather than being answered as a clean no-op.
    ///
    /// The provider kind that normalizes the capability key is DERIVED here from
    /// the router's own config, never accepted from the caller: it is one half of
    /// the registry key, so a caller-supplied value would be a second source of
    /// truth for it. A wrong one addresses a key the learn path never minted, and
    /// the purge then reports a clean no-op while the entry stays resident and
    /// keeps steering routing. An unrecognized target derives the empty kind,
    /// whose normalization is the identity -- the right answer for a registry
    /// entry whose config entry the operator has since removed.
    ///
    /// The derived kind is also the third part of the registry identity, so the
    /// purge clears the version the current config owns and only that one. The
    /// durable clear it commits names that one kind, and removing another
    /// kind's version from memory without a clear of its own would let a later
    /// flip back to that kind plus a restart replay a version memory had
    /// dropped. A version another kind wrote acts for no one, is skipped at
    /// boot replay, and is removed by the next owner sweep.
    ///
    /// Returns holding NO registry lock, which is what lets the caller await
    /// SQLite next.
    pub fn reserve_learned_capability_purge(
        &self,
        lane: &StateKey,
        capability_key: &str,
    ) -> PurgeOutcome {
        let state_key = lane.as_lane_key();
        let provider_kind = self.provider_kind_for_state_key(state_key).to_string();
        let prepared = self.learned_capabilities.prepare_purge(
            self.registry_generation(),
            state_key,
            capability_key,
            &provider_kind,
        );
        let normalized =
            routectl_core::capability::normalize_capability_key(capability_key, &provider_kind);
        match prepared {
            PurgePreparation::Reserved(lease) => PurgeOutcome::Reserved(Box::new(ReservedPurge {
                state_key: state_key.to_string(),
                capability_key: normalized,
                provider_kind,
                lane: lane.clone(),
                lease,
            })),
            // A lift is only for a lane dispatch actually reaches: the lane
            // parser accepts any upstream on a configured entry, and lifting on
            // an invented one would grow the marker set and the ledger with
            // cells no request can ever use.
            PurgePreparation::SeedLift(lease) if !self.lane_is_routed(lane) => {
                self.learned_capabilities.restore_purge(lease);
                PurgeOutcome::Absent
            }
            PurgePreparation::SeedLift(lease) => {
                PurgeOutcome::SeedLift(Box::new(ReservedSeedLift {
                    state_key: state_key.to_string(),
                    capability_key: normalized,
                    provider_kind,
                    lease,
                }))
            }
            PurgePreparation::Absent => PurgeOutcome::Absent,
            PurgePreparation::Busy => PurgeOutcome::Busy,
            PurgePreparation::Stale => PurgeOutcome::Stale,
        }
    }

    /// Whether one of the installed models dispatches to `lane`.
    fn lane_is_routed(&self, lane: &StateKey) -> bool {
        self.learned_lane_projection()
            .lanes()
            .iter()
            .any(|resolved| resolved.routed && resolved.lane == *lane)
    }

    /// Finalize a reserved purge: remove the entry and release the lease.
    ///
    /// Called ONLY after the reservation's settlement has durably committed, so
    /// from this instant the registry and the ledger agree. Emits the single
    /// content-free audit record for the completed action -- after the commit, so
    /// the log cannot claim a removal the ledger never recorded.
    ///
    /// Returns whether an entry was removed; `false` would mean the entry
    /// vanished under an open lease, which nothing can currently do.
    ///
    /// A removed field-namespace key also drops its canary/quorum state, the
    /// same reset `FieldRepairGuard::clear` performs -- otherwise a later
    /// re-learn of the purged identity would inherit a stale cadence, claim,
    /// or confirmation count from the incarnation the operator just removed.
    pub fn finalize_learned_capability_purge(&self, reserved: Box<ReservedPurge>) -> bool {
        let removed = self.learned_capabilities.finalize_purge(reserved.lease);
        if removed
            && !crate::field_capability::capability_key_is_catalog_scoped(&reserved.capability_key)
        {
            let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
                reserved.lane.clone(),
                reserved.capability_key.clone(),
                reserved.provider_kind.clone(),
            );
            self.field_verdicts.canaries().reset(&key);
        }
        tracing::info!(
            event = "purge",
            state_key = %routectl_core::sanitize_for_log(&reserved.state_key),
            capability_key = %routectl_core::sanitize_for_log(&reserved.capability_key),
            removed,
            "operator purged a learned-capability entry",
        );
        removed
    }

    /// Finalize a reserved seed lift: record the seed-clear marker and release
    /// the lease.
    ///
    /// Called ONLY after the lift's settlement has durably committed, so the
    /// marker and the ledger agree from this instant on. Emits the same
    /// content-free `purge` audit record as a learned purge, with
    /// `removed = false` (nothing was resident) and `seed_lifted = true`.
    pub fn finalize_seed_lift(&self, reserved: Box<ReservedSeedLift>) {
        self.learned_capabilities.finalize_seed_lift(reserved.lease);
        tracing::info!(
            event = "purge",
            state_key = %routectl_core::sanitize_for_log(&reserved.state_key),
            capability_key = %routectl_core::sanitize_for_log(&reserved.capability_key),
            removed = false,
            seed_lifted = true,
            "operator purged a learned-capability entry",
        );
    }

    /// Release a reserved seed lift WITHOUT recording a marker: the durable
    /// clear did not commit, so the seed keeps withholding the flag.
    pub fn abandon_seed_lift(&self, reserved: Box<ReservedSeedLift>) {
        self.learned_capabilities.restore_purge(reserved.lease);
        tracing::warn!(
            event = "purge_abandoned",
            state_key = %routectl_core::sanitize_for_log(&reserved.state_key),
            capability_key = %routectl_core::sanitize_for_log(&reserved.capability_key),
            "operator seed lift did not persist its clear; the seed still withholds the flag",
        );
    }

    /// Emit the audit record for a purge that found nothing resident.
    ///
    /// A no-op is still a completed operator action, and it gets the SAME record
    /// shape with `removed = false`: an operator reading the log has to be able to
    /// tell "I purged something" from "there was nothing to purge", and only a
    /// record present in both cases makes that readable. The capability key is
    /// normalized here exactly as the reservation would have normalized it, so
    /// the two cases key identically in the log.
    pub fn audit_absent_purge(&self, lane: &StateKey, capability_key: &str) {
        let state_key = lane.as_lane_key();
        let provider_kind = self.provider_kind_for_state_key(state_key);
        let normalized =
            routectl_core::capability::normalize_capability_key(capability_key, provider_kind);
        tracing::info!(
            event = "purge",
            state_key = %routectl_core::sanitize_for_log(state_key),
            capability_key = %routectl_core::sanitize_for_log(&normalized),
            removed = false,
            "operator purged a learned-capability entry",
        );
    }

    /// Release a reserved purge WITHOUT removing anything: the durable clear did
    /// not commit, so the entry stays exactly as it was and keeps acting.
    ///
    /// There is nothing to put back -- the reservation never removed it -- which
    /// is why this path cannot restore the entry wrongly.
    pub fn abandon_learned_capability_purge(&self, reserved: Box<ReservedPurge>) {
        self.learned_capabilities.restore_purge(reserved.lease);
        tracing::warn!(
            event = "purge_abandoned",
            state_key = %routectl_core::sanitize_for_log(&reserved.state_key),
            capability_key = %routectl_core::sanitize_for_log(&reserved.capability_key),
            "operator purge did not persist its clear; the entry is unchanged and still acting",
        );
    }
}
