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

/// A reserved purge: the key is leased, the entry is still resident, and the
/// caller owes exactly one settlement.
///
/// Carries the keys the reservation was made on so the caller persists and logs
/// what the registry actually keyed on rather than the raw request values -- the
/// `capability_key` here is already normalized. No request body, prompt, or
/// upstream text ever enters it.
#[must_use = "a reserved purge must be finalized or abandoned"]
pub struct ReservedPurge {
    /// Routing state key the purge is keyed on.
    pub state_key: String,
    /// Normalized capability key the purge is keyed on.
    pub capability_key: String,
    /// Stable provider-kind token that normalized the capability key.
    pub provider_kind: String,
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

/// What a purge request resolved to. Every variant needs its own answer on the
/// wire, and collapsing any two loses something the operator needs: "already
/// gone" is not "ask the current router", and neither is "someone else is
/// purging this right now". Telling an operator an entry is gone is the answer a
/// refusal must never give.
#[must_use]
pub enum PurgeOutcome {
    /// Reserved: the caller commits the settlement and then finalizes.
    ///
    /// BOXED because the other three variants carry nothing: the reservation
    /// holds a captured entry plus a lease, and leaving it inline would make
    /// every refusal pay its size. The refusals are also the common answers on a
    /// healthy daemon.
    Reserved(Box<ReservedPurge>),
    /// No resident entry under this key -- a clean no-op.
    Absent,
    /// Another purge holds this key's lease.
    Busy,
    /// The request arrived through a superseded Router, whose registry the
    /// published Router no longer reads.
    Stale,
}

impl Router {
    /// Reserve `(state_key, capability_key)` for an operator purge.
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
    /// Returns holding NO registry lock, which is what lets the caller await
    /// SQLite next.
    pub fn reserve_learned_capability_purge(
        &self,
        state_key: &str,
        capability_key: &str,
    ) -> PurgeOutcome {
        let provider_kind = self.provider_kind_for_state_key(state_key).to_string();
        let prepared = self.learned_capabilities.prepare_purge(
            self.registry_generation(),
            state_key,
            capability_key,
            &provider_kind,
        );
        match prepared {
            PurgePreparation::Reserved(lease) => PurgeOutcome::Reserved(Box::new(ReservedPurge {
                state_key: state_key.to_string(),
                capability_key: routectl_core::capability::normalize_capability_key(
                    capability_key,
                    &provider_kind,
                ),
                provider_kind,
                lease,
            })),
            PurgePreparation::Absent => PurgeOutcome::Absent,
            PurgePreparation::Busy => PurgeOutcome::Busy,
            PurgePreparation::Stale => PurgeOutcome::Stale,
        }
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
    pub fn finalize_learned_capability_purge(&self, reserved: Box<ReservedPurge>) -> bool {
        let removed = self.learned_capabilities.finalize_purge(reserved.lease);
        tracing::info!(
            event = "purge",
            state_key = %routectl_core::sanitize_for_log(&reserved.state_key),
            capability_key = %routectl_core::sanitize_for_log(&reserved.capability_key),
            removed,
            "operator purged a learned-capability entry",
        );
        removed
    }

    /// Emit the audit record for a purge that found nothing resident.
    ///
    /// A no-op is still a completed operator action, and it gets the SAME record
    /// shape with `removed = false`: an operator reading the log has to be able to
    /// tell "I purged something" from "there was nothing to purge", and only a
    /// record present in both cases makes that readable. The capability key is
    /// normalized here exactly as the reservation would have normalized it, so
    /// the two cases key identically in the log.
    pub fn audit_absent_purge(&self, state_key: &str, capability_key: &str) {
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
