//! Removal of learned entries whose provider entry no longer owns their lane.
//!
//! A reload attaches the replacement Router to the outgoing registry, so a
//! lane whose `[providers]` entry the new config dropped, or now configures
//! under a different kind, would otherwise keep acting on whatever that name
//! points at. The owner rule is [`crate::capability_owner::owner_decision`],
//! the same one boot replay applies, so an entry this sweep drops is also an
//! entry the next restart skips.
//!
//! The owner of an entry is the kind that WROTE it, recorded on the entry
//! itself. Neither the outgoing nor the replacement config can stand in for
//! it: a request still holding an older Router can write a lane after a
//! sweep, so the outgoing config's kind is not necessarily the writer's.

use super::Router;
use crate::capability_owner::{OwnerDecision, owner_decision};
use crate::learned_capability::{GenerationOutcome, RecordedLearnedEntry};
use crate::state_key::StateKey;

impl Router {
    /// Whether this Router's config still owns `recorded`: its lane's provider
    /// entry exists under the kind the entry was written under.
    ///
    /// A key that is not a lane names no provider entry, so there is nothing to
    /// disown it by; boot replay refuses such rows on its own.
    #[must_use]
    pub fn owns_learned_entry(&self, recorded: &RecordedLearnedEntry) -> bool {
        self.owner_of(recorded)
            .is_none_or(|(_, decision)| decision == OwnerDecision::Owned)
    }

    fn owner_of(&self, recorded: &RecordedLearnedEntry) -> Option<(StateKey, OwnerDecision)> {
        let lane = StateKey::parse(&recorded.entry.state_key)?;
        let decision = owner_decision(&lane, &recorded.provider_kind, &self.config.providers);
        Some((lane, decision))
    }

    /// Remove every resident lane-keyed entry and seed-clear marker this
    /// Router's config does not own, and return how many were removed.
    ///
    /// Destructive, so it runs only where this Router is the one being
    /// published: directly in a config-only carry-over, and after a
    /// revision-changing reload's boundary has committed. A boundary that fails
    /// leaves the outgoing Router live, and the entries it owns must still be
    /// there.
    pub fn sweep_unowned_learned_entries(&self) -> usize {
        let generation = self.registry_generation();
        let removed = self
            .learned_capabilities
            .recorded_snapshot()
            .iter()
            .filter(|recorded| self.remove_if_unowned(generation, recorded))
            .count()
            + self.sweep_unowned_seed_clears();
        if removed > 0 {
            tracing::info!(
                event = "owner_sweep",
                dropped_owner = removed,
                "dropped learned capabilities whose provider entry was removed or changed kind",
            );
        }
        removed
    }

    /// Remove every seed-clear marker whose lane this Router's config does not
    /// own under the kind the marker was recorded under -- the same rule boot
    /// replay applies to the `cleared` row behind it.
    fn sweep_unowned_seed_clears(&self) -> usize {
        self.learned_capabilities
            .seed_clear_snapshot()
            .iter()
            .filter(|marker| {
                StateKey::parse(&marker.state_key).is_some_and(|lane| {
                    owner_decision(&lane, &marker.provider_kind, &self.config.providers)
                        != OwnerDecision::Owned
                })
            })
            .filter(|marker| {
                self.learned_capabilities.remove_seed_clear(
                    &marker.state_key,
                    &marker.feature_key,
                    &marker.provider_kind,
                )
            })
            .count()
    }

    fn remove_if_unowned(&self, generation: u64, recorded: &RecordedLearnedEntry) -> bool {
        let Some((lane, decision)) = self.owner_of(recorded) else {
            return false;
        };
        if decision == OwnerDecision::Owned {
            return false;
        }
        let entry = &recorded.entry;
        let outcome = self.learned_capabilities.remove_recorded_in_generation(
            generation,
            &entry.state_key,
            &entry.feature_key,
            &recorded.provider_kind,
        );
        if !matches!(outcome, GenerationOutcome::Applied { value: true, .. }) {
            return false;
        }
        if !crate::field_capability::capability_key_is_catalog_scoped(&entry.feature_key) {
            let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
                lane.clone(),
                entry.feature_key.clone(),
                recorded.provider_kind.clone(),
            );
            self.field_verdicts.canaries().reset(&key);
        }
        tracing::debug!(
            event = "owner_sweep",
            reason = decision.skip_reason().unwrap_or_default(),
            state_key = %lane.for_log(),
            capability_key = %routectl_core::sanitize_for_log(&entry.feature_key),
            "dropped a learned entry whose provider entry no longer owns its lane",
        );
        true
    }
}

#[cfg(test)]
#[path = "capability_owner_sweep_tests.rs"]
mod tests;
