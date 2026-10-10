//! The field-verdict operations a re-check probe settles through: the stamp a
//! probe captures before it dials, the removal that retires exactly that
//! version once its `cleared` row is durable, and the in-place refresh a
//! re-confirming rejection applies.
//!
//! All three are keyed by the minted [`FieldVerdictKey`], never a free-form
//! string, and all three run under the guarded READ primitive: none of them
//! produces a new version of the key's state. The removal retires the version
//! the durable clear names, as `finalize_purge` does, and the refresh extends
//! the decay of the version already resident. Minting an incarnation for
//! either would restamp a surviving row and orphan the canary state its
//! pre-flight eligibility is keyed on.

use std::time::Instant;

use routectl_core::capability::SignalTier;

use super::{GenerationOutcome, LearnedCapabilityRegistry, NegativeState};
use crate::field_verdict::FieldVerdictKey;

/// One resident field negative's version, read under one guarded acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldVerdictStamp {
    /// The resident entry's own incarnation.
    pub incarnation: u64,
    /// The effective persistence generation at the read, which an event
    /// describing this version is stamped with.
    pub persistence_generation: u64,
}

/// What a re-check finds resident for its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldVerdictPresence {
    /// An acting or lapsed negative is resident at this stamp.
    Resident(FieldVerdictStamp),
    /// No negative is resident.
    Absent,
    /// The key cannot be read right now: a purge holds its lease, or the
    /// generation may not read it. Neither resident nor absent is known.
    Unreadable,
}

impl FieldVerdictPresence {
    /// The resident stamp, if any.
    #[must_use]
    pub const fn stamp(self) -> Option<FieldVerdictStamp> {
        match self {
            Self::Resident(stamp) => Some(stamp),
            Self::Absent | Self::Unreadable => None,
        }
    }
}

impl LearnedCapabilityRegistry {
    /// What negative is resident for `key`, read as `generation`.
    pub fn field_verdict_presence_in_generation(
        &self,
        generation: u64,
        key: &FieldVerdictKey,
        now: Instant,
    ) -> FieldVerdictPresence {
        let outcome = self.guarded_read(
            generation,
            key.registry_key(),
            key.capability_key(),
            key.provider_kind(),
            |entries, _leased| {
                let registry_key = Self::make_key(
                    key.registry_key(),
                    key.capability_key(),
                    key.provider_kind(),
                );
                Self::negative_state_in(entries, &registry_key, now) != NegativeState::Absent
            },
        );
        match outcome {
            GenerationOutcome::Applied {
                value: true,
                incarnation,
                generation,
            } => FieldVerdictPresence::Resident(FieldVerdictStamp {
                incarnation,
                persistence_generation: generation,
            }),
            GenerationOutcome::Applied { value: false, .. } => FieldVerdictPresence::Absent,
            GenerationOutcome::Stale
            | GenerationOutcome::Reserved
            | GenerationOutcome::Exhausted
            | GenerationOutcome::FieldNamespace => FieldVerdictPresence::Unreadable,
        }
    }

    /// Remove `key`'s resident entry only while it is still at `incarnation`.
    /// Returns whether a row was removed.
    ///
    /// For a caller whose durable `cleared` row names `incarnation`: a row at
    /// any other incarnation is a lifecycle that clear has no evidence about.
    pub fn remove_field_verdict_at_incarnation(
        &self,
        generation: u64,
        key: &FieldVerdictKey,
        incarnation: u64,
    ) -> bool {
        let outcome = self.guarded_read(
            generation,
            key.registry_key(),
            key.capability_key(),
            key.provider_kind(),
            |entries, _leased| {
                let registry_key = Self::make_key(
                    key.registry_key(),
                    key.capability_key(),
                    key.provider_kind(),
                );
                if entries
                    .get(&registry_key)
                    .is_some_and(|entry| entry.incarnation == incarnation)
                {
                    entries.remove(&registry_key).is_some()
                } else {
                    false
                }
            },
        );
        matches!(outcome, GenerationOutcome::Applied { value: true, .. })
    }

    /// Re-observe `key`'s resident negative at `tier`, only while it is still
    /// at `incarnation` and negative. Returns whether it was refreshed.
    ///
    /// Never creates a row: a refresh re-confirms the version a probe asked
    /// about, so an absent or replaced entry is left exactly as it is.
    pub fn refresh_field_verdict_at_incarnation(
        &self,
        generation: u64,
        key: &FieldVerdictKey,
        incarnation: u64,
        tier: SignalTier,
        now: Instant,
    ) -> bool {
        let outcome = self.guarded_read(
            generation,
            key.registry_key(),
            key.capability_key(),
            key.provider_kind(),
            |entries, _leased| {
                let registry_key = Self::make_key(
                    key.registry_key(),
                    key.capability_key(),
                    key.provider_kind(),
                );
                if Self::negative_state_in(entries, &registry_key, now) == NegativeState::Absent {
                    return false;
                }
                match entries.get_mut(&registry_key) {
                    Some(entry) if entry.incarnation == incarnation => {
                        self.observe_existing(entry, tier, now);
                        true
                    }
                    _ => false,
                }
            },
        );
        matches!(outcome, GenerationOutcome::Applied { value: true, .. })
    }
}
