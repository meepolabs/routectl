//! Ownership of runtime-state slots: which identity seeded each key of the
//! state map, and whether a model being installed may take the keys it
//! composes.

use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::ProviderRuntimePolicy;
use crate::resolved::ResolvedModel;
use crate::runtime_state::ProviderState;
use crate::seat_pool::check_state_key_name;

use super::Router;

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
    /// Create the slot for `key` seeded from `policy` and record `owner`,
    /// or leave an existing slot and its recorded owner untouched.
    pub(super) fn claim_state_slot(
        &mut self,
        key: String,
        owner: SlotOwner,
        policy: &ProviderRuntimePolicy,
    ) {
        if self.state.contains_key(&key) {
            return;
        }
        self.state.insert(
            key.clone(),
            Arc::new(Mutex::new(ProviderState::new(policy))),
        );
        self.slot_owners.insert(key, owner);
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
