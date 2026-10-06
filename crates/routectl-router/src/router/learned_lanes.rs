//! The learned lanes a configuration resolves to, projected once for every
//! operator surface that names them.
//!
//! A dispatch target's learned lane is minted at chain expansion from the
//! provider entry that egresses and the upstream it sends. Doctor,
//! `probe --capabilities` and the status health panel need those same lanes
//! without dispatching; minting them again from raw config at each surface
//! lets a surface drift from the key dispatch actually learns under. This is
//! the one projection they read:
//!
//! - [`Router::learned_lane_projection`] reads each lane off the installed
//!   router's own dispatch targets, built by the same constructors chain
//!   expansion uses.
//! - [`LearnedLaneProjection::from_config`] walks a configuration the way the
//!   factory resolves it, for a surface that holds no built router. It applies
//!   the factory's static filters (an unknown provider or pool, a nickname
//!   carrying the seat separator, a pool member with no `[providers]` entry)
//!   but cannot see credentials, so a pool member whose credential would fail
//!   at build is still projected.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::chain::{dispatch_target_for_seat, into_one_dispatch_target};
use super::{DispatchTarget, Router};
use crate::config::{Config, ProviderEntry};
use crate::resolved::ResolvedModel;
use crate::state_key::StateKey;

/// One learned lane and the configured models that dispatch to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLearnedLane {
    /// The lane every learned fact for these models keys on.
    pub lane: StateKey,
    /// The model nicknames whose dispatch targets egress through this lane,
    /// sorted and deduplicated.
    pub nicknames: Vec<String>,
    /// The lane's provider kind (`kind_str`), empty when its provider entry
    /// is not configured.
    pub provider_kind: &'static str,
    /// Whether dispatch reaches the lane: at least one of its models is
    /// selectable. A lane only `selectable = false` models map to is still
    /// projected, so its learned history keeps a home, but is not routed.
    pub routed: bool,
}

impl ResolvedLearnedLane {
    /// Whether the model `nickname`, egressing through `provider_entry`,
    /// dispatches to this lane.
    #[must_use]
    pub fn maps(&self, nickname: &str, provider_entry: &str) -> bool {
        self.lane.provider_entry() == provider_entry && self.nicknames.iter().any(|n| n == nickname)
    }
}

/// Every learned lane a configuration resolves to, sorted by lane key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearnedLaneProjection {
    lanes: Vec<ResolvedLearnedLane>,
}

impl LearnedLaneProjection {
    /// Project the lanes `config` resolves to without building a router.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut lanes = LaneCollector::default();
        for (nickname, model) in &config.models {
            if crate::seat_pool::check_state_key_name("model nickname", nickname).is_err() {
                continue;
            }
            for provider_entry in egress_entries(config, &model.provider) {
                if let Some(lane) = StateKey::new(provider_entry, &model.upstream) {
                    let kind = provider_kind(config, provider_entry);
                    lanes.add(lane, nickname, kind, model.selectable);
                }
            }
        }
        lanes.finish()
    }

    /// The projected lanes, sorted by lane key.
    #[must_use]
    pub fn lanes(&self) -> &[ResolvedLearnedLane] {
        &self.lanes
    }

    /// The lane the model `nickname` dispatches to through `provider_entry`.
    #[must_use]
    pub fn lane_for(&self, nickname: &str, provider_entry: &str) -> Option<&ResolvedLearnedLane> {
        self.lanes
            .iter()
            .find(|lane| lane.maps(nickname, provider_entry))
    }
}

impl Router {
    /// The learned lanes this router's installed models dispatch to, read off
    /// the dispatch targets chain expansion would build for them. Read-only:
    /// no seat order is computed and no routing state moves.
    #[must_use]
    pub fn learned_lane_projection(&self) -> LearnedLaneProjection {
        let mut lanes = LaneCollector::default();
        for model in self.resolved_models.values() {
            for target in self.projection_targets(model) {
                if let Some(lane) = target.learned_lane {
                    let kind = provider_kind(&self.config, &target.provider_name);
                    lanes.add(lane, &model.nickname, kind, true);
                }
            }
        }
        lanes.finish()
    }

    /// One dispatch target per seat of a pooled model, or the single target
    /// of a direct one, in configured order, built by the constructors chain
    /// expansion uses.
    pub(super) fn projection_targets(&self, model: &Arc<ResolvedModel>) -> Vec<DispatchTarget> {
        model.seats.as_ref().map_or_else(
            || vec![into_one_dispatch_target(Arc::clone(model))],
            |seats| {
                seats
                    .iter()
                    .map(|seat| {
                        let kind = self
                            .config
                            .providers
                            .get(&seat.provider_name)
                            .map(ProviderEntry::kind_str);
                        dispatch_target_for_seat(model, seat, kind)
                    })
                    .collect()
            },
        )
    }
}

/// The provider entries a model naming `provider` egresses through: every
/// configured member of the pool it names, or the entry itself.
fn egress_entries<'a>(config: &'a Config, provider: &'a str) -> Vec<&'a str> {
    if let Some(pool) = config.pools.get(provider) {
        return pool
            .members
            .iter()
            .filter(|member| config.providers.contains_key(*member))
            .map(String::as_str)
            .collect();
    }
    if config.providers.contains_key(provider) {
        vec![provider]
    } else {
        Vec::new()
    }
}

fn provider_kind(config: &Config, provider_entry: &str) -> &'static str {
    config
        .providers
        .get(provider_entry)
        .map_or("", ProviderEntry::kind_str)
}

/// Groups lane bindings by serialized lane key.
#[derive(Default)]
struct LaneCollector {
    lanes: BTreeMap<String, ResolvedLearnedLane>,
}

impl LaneCollector {
    fn add(&mut self, lane: StateKey, nickname: &str, provider_kind: &'static str, routed: bool) {
        let entry = self
            .lanes
            .entry(lane.as_lane_key().to_string())
            .or_insert_with(|| ResolvedLearnedLane {
                lane,
                nicknames: Vec::new(),
                provider_kind,
                routed: false,
            });
        entry.routed |= routed;
        if !entry.nicknames.iter().any(|n| n == nickname) {
            entry.nicknames.push(nickname.to_string());
        }
    }

    fn finish(self) -> LearnedLaneProjection {
        let lanes = self
            .lanes
            .into_values()
            .map(|mut lane| {
                lane.nicknames.sort();
                lane
            })
            .collect();
        LearnedLaneProjection { lanes }
    }
}

#[cfg(test)]
#[path = "learned_lanes_tests.rs"]
mod tests;
