//! Hot-reload removal of learned entries whose provider entry no longer owns
//! their lane.
//!
//! The reload attaches the replacement Router to the outgoing registry, so a
//! lane whose `[providers]` entry the new config dropped, or now configures
//! under a different kind, would otherwise keep acting on whatever that name
//! points at. The owner rule is [`crate::capability_owner::owner_decision`],
//! the same one boot replay applies, so an entry this sweep drops is also an
//! entry the next restart skips.

use super::Router;
use crate::capability_owner::{OwnerDecision, owner_decision};
use crate::learned_capability::GenerationOutcome;
use crate::state_key::StateKey;

impl Router {
    /// Remove every resident lane-keyed entry that `previous` learned under a
    /// provider entry this Router no longer configures under the same kind.
    /// Returns how many entries were removed.
    ///
    /// The recorded kind is `previous`'s: an entry resident at swap time was
    /// learned under the outgoing config. Must run after the registry and the
    /// field-verdict facade are attached, and before any boundary cut, which
    /// restates catalog-independent survivors stamped with THIS Router's kind.
    pub(super) fn drop_unowned_learned_entries(&self, previous: &Self) -> usize {
        let generation = self.registry_generation();
        let mut removed = 0;
        for entry in self.learned_capabilities.snapshot() {
            let Some(lane) = StateKey::parse(&entry.state_key) else {
                continue;
            };
            let recorded_kind = previous
                .config
                .providers
                .get(lane.provider_entry())
                .map_or("", |p| p.kind_str());
            let decision = owner_decision(&lane, recorded_kind, &self.config.providers);
            if decision == OwnerDecision::Owned {
                continue;
            }
            // The snapshot key is already normalized; an empty kind normalizes
            // as the identity, so the removal meets the resident key exactly.
            let outcome = self.learned_capabilities.remove_keyed_in_generation(
                generation,
                &entry.state_key,
                &entry.feature_key,
                "",
            );
            if !matches!(outcome, GenerationOutcome::Applied { value: true, .. }) {
                continue;
            }
            removed += 1;
            if !crate::field_capability::capability_key_is_catalog_scoped(&entry.feature_key) {
                let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
                    lane.clone(),
                    entry.feature_key.clone(),
                    recorded_kind.to_string(),
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
        }
        removed
    }
}

#[cfg(test)]
#[path = "capability_owner_sweep_tests.rs"]
mod tests;
