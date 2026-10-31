//! Ownership of runtime-state slots: which identity seeded each key of the
//! state map, whether a model being installed may take the keys it
//! composes, and the tuning resolved for each key.

use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::ResolvedTuning;
use crate::resolved::ResolvedModel;
use crate::runtime_state::ProviderState;
use crate::seat_pool::check_state_key_name;

use super::{DispatchTarget, Router};

/// The identity whose runtime policy seeded a state slot. A provider and a
/// pool of the same name are different owners: a pool slot is a pooled
/// model's own nickname slot, never a provider's gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlotOwner {
    Provider(String),
    Pool(String),
}

impl SlotOwner {
    /// The owner of a model's own nickname slot: its pool when it has seats,
    /// otherwise the provider it dispatches through.
    pub(super) fn of_model(m: &ResolvedModel) -> Self {
        match m.seats {
            Some(_) => Self::Pool(m.provider_name.clone()),
            None => Self::Provider(m.provider_name.clone()),
        }
    }
}

impl fmt::Display for SlotOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(name) => write!(f, "provider `{name}`"),
            Self::Pool(name) => write!(f, "pool `{name}`"),
        }
    }
}

/// Every runtime-state key `m` occupies when installed under `nickname`,
/// paired with the owner that slot must have: its own nickname slot plus one
/// per pooled seat.
fn slot_seeds(nickname: &str, m: &ResolvedModel) -> Vec<(String, SlotOwner)> {
    std::iter::once((nickname.to_string(), SlotOwner::of_model(m)))
        .chain(m.seats.iter().flat_map(|seats| seats.iter()).map(|seat| {
            (
                seat.state_key_for(nickname),
                SlotOwner::Provider(seat.provider_name.clone()),
            )
        }))
        .collect()
}

impl Router {
    /// Create the slot for `key` seeded from the resolved tuning of `entry`
    /// (a provider or pool name; `None` takes the shipped defaults) and
    /// record `owner`, or leave an existing slot and its recorded owner
    /// untouched.
    pub(super) fn claim_state_slot(&mut self, key: String, owner: SlotOwner, entry: Option<&str>) {
        if self.state.contains_key(&key) {
            return;
        }
        let gate = crate::config::resolve_tuning(&self.config, entry, None).gate();
        self.state.insert(
            key.clone(),
            Arc::new(Mutex::new(ProviderState::from_gate(gate))),
        );
        self.slot_owners.insert(key, owner);
    }

    /// Record the tuning a dispatch through `key` runs under, replacing any
    /// earlier stamp. Unlike the state slot, the stamp is not first-claim:
    /// a model reusing a slot its provider seeded still carries its own
    /// first-content override.
    pub(super) fn stamp_tuning(
        &mut self,
        key: String,
        entry: Option<&str>,
        model_first_byte_ms: Option<u64>,
    ) {
        let tuning = crate::config::resolve_tuning(&self.config, entry, model_first_byte_ms);
        self.tuning.insert(key, Arc::new(tuning));
    }

    /// The tuning `target` dispatches under: the stamp for its state key, or,
    /// for a target no install stamped, the resolver's answer for its
    /// provider and its model's first-content override.
    pub(super) fn tuning_for(&self, target: &DispatchTarget) -> Arc<ResolvedTuning> {
        self.tuning
            .get(target.state_key.as_str())
            .cloned()
            .unwrap_or_else(|| {
                Arc::new(crate::config::resolve_tuning(
                    &self.config,
                    Some(&target.provider_name),
                    target.model.stream_first_byte_timeout_ms,
                ))
            })
    }

    /// Why `m`, filed under `nickname`, may not take runtime-state slots, or
    /// `None` when every key it composes is free or already owned by the
    /// identity that would seed it.
    pub(super) fn state_slot_refusal(&self, nickname: &str, m: &ResolvedModel) -> Option<String> {
        if m.nickname != nickname {
            return Some(format!(
                "table key `{nickname}` differs from the model's own nickname `{}`",
                m.nickname
            ));
        }
        if let Err(reason) = check_state_key_name("model nickname", nickname) {
            return Some(reason);
        }
        for seat in m.seats.iter().flat_map(|seats| seats.iter()) {
            if let Err(reason) = check_state_key_name("pool member", &seat.provider_name) {
                return Some(reason);
            }
        }
        slot_seeds(nickname, m)
            .into_iter()
            .find_map(|(key, owner)| {
                if !self.state.contains_key(&key) {
                    return None;
                }
                // INVARIANT: letting a direct model named after its own provider
                // reuse that provider's slot is sound only while provider-name
                // slots are seed placeholders that no production path looks up --
                // every dispatch and status key is a model nickname or a seat
                // key. A production read keyed by a bare provider name would make
                // this reuse a shared gate between the provider and the model.
                match self.slot_owners.get(&key) {
                    Some(holder) if *holder == owner => None,
                    Some(holder) => Some(format!(
                        "state key `{key}` is already held by a slot of {holder}"
                    )),
                    None => Some(format!(
                        "state key `{key}` is already held by a slot of unknown origin"
                    )),
                }
            })
    }
}
