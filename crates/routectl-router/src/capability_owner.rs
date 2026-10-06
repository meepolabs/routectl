//! Whether a learned capability fact still belongs to the provider entry its
//! lane names.
//!
//! A lane (`provider_entry#upstream`) names a `[providers]` entry by its
//! operator-chosen name, and a name is not an identity: an operator can delete
//! the entry, or keep the name and change its `kind`. Either way the facts
//! learned under the old entry describe an endpoint the current config no
//! longer egresses to, so they must not act on whatever the name now points
//! at. The owner of a fact is therefore the pair (entry name, entry kind),
//! and this module is the one place that compares a fact's recorded owner
//! against the current config. Boot replay and hot-reload carry-over both
//! call it, so a fact a reload drops is also a fact the next restart skips.
//!
//! A same-kind repoint (a changed `base_url` under one name) keeps its
//! history: the predicate cannot see an endpoint, only a name and a kind.

use std::collections::BTreeMap;

use crate::config::ProviderEntry;
use crate::state_key::StateKey;

/// The owner check's verdict for one learned fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDecision {
    /// The lane's provider entry exists and still has the recorded kind.
    Owned,
    /// The lane's provider entry is absent from the current config.
    EntryRemoved,
    /// The lane's provider entry exists under a different kind.
    KindChanged,
    /// The fact carries no provider kind, so ownership cannot be shown.
    /// Treated as unowned: replaying it would attach a fact of unknown origin
    /// to whatever the entry is now.
    KindUnrecorded,
}

impl OwnerDecision {
    /// Stable log token for a skip; `None` for [`Self::Owned`].
    pub const fn skip_reason(self) -> Option<&'static str> {
        match self {
            Self::Owned => None,
            Self::EntryRemoved => Some("owner_entry_removed"),
            Self::KindChanged => Some("owner_kind_changed"),
            Self::KindUnrecorded => Some("owner_kind_unrecorded"),
        }
    }
}

/// Decide whether a fact on `lane`, recorded under `recorded_kind`, belongs
/// to the entry `providers` configures under that name today.
pub fn owner_decision(
    lane: &StateKey,
    recorded_kind: &str,
    providers: &BTreeMap<String, ProviderEntry>,
) -> OwnerDecision {
    let Some(entry) = providers.get(lane.provider_entry()) else {
        return OwnerDecision::EntryRemoved;
    };
    if recorded_kind.is_empty() {
        return OwnerDecision::KindUnrecorded;
    }
    if entry.kind_str() != recorded_kind {
        return OwnerDecision::KindChanged;
    }
    OwnerDecision::Owned
}

#[cfg(test)]
#[path = "capability_owner_tests.rs"]
mod tests;
