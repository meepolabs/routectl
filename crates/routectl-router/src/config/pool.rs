//! The `[pools.<name>]` config block: a set of same-kind provider entries
//! (accounts) sharing one seat-selection strategy.
//!
//! A pool is the operator-facing multi-account shape: `[models.X] provider`
//! may name a provider entry OR a pool, and the pool owns the strategy that
//! picks among its members. Membership is by provider-entry NAME, so a pool
//! never carries credential material and stays credential-generic.

use serde::{Deserialize, Serialize};

use super::{Config, SeatSelection};

/// One `[pools.<name>]` entry: the member provider entries plus the policy
/// that picks among them.
///
/// Members are `[providers.X]` table keys, all of the same provider kind.
/// Validation rejects a mixed-kind pool, an unknown member, a member
/// claimed by two pools, an empty member list, and a pool name that
/// collides with a provider entry or a model nickname.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PoolEntry {
    /// Provider entry names that make up this pool. Required and
    /// non-empty: an empty pool serves nothing, which is a silent
    /// no-serve rather than a configuration an operator meant to write.
    /// Every member must name an existing `[providers.X]` entry of the
    /// same kind, and a provider entry belongs to at most one pool.
    pub members: Vec<String>,

    /// How dispatch picks among this pool's members for one request.
    /// `fill-first` (the default) drains one member before moving to the
    /// next; `round-robin` spreads load across members, advancing the
    /// start member per request; `sticky-least-loaded` pins each
    /// conversation to one member for prompt-cache affinity while
    /// balancing new conversations across members by load, and is the only
    /// strategy quota-aware placement applies to. Applied per request
    /// whenever the pool resolves to more than one usable member.
    #[serde(default)]
    pub seat_selection: SeatSelection,

    /// Whether a future `routectl login` for this pool's provider family
    /// may propose joining the new account to this pool. False (the
    /// default) pins the membership list: only an explicit config edit
    /// grows it. This is the only thing that grows a pool -- there is no
    /// wildcard member token.
    #[serde(default)]
    pub accepts_new_logins: bool,
}

impl PoolEntry {
    /// Construct a pool from its member list, taking the default
    /// strategy (`fill-first`) and a pinned membership list. Use this for
    /// explicit construction from outside the crate (the struct is
    /// `#[non_exhaustive]`, so struct-literal syntax is unavailable there).
    pub fn new(members: Vec<String>) -> Self {
        Self {
            members,
            seat_selection: SeatSelection::default(),
            accepts_new_logins: false,
        }
    }

    /// Same pool with `seat_selection` replaced.
    #[must_use]
    pub fn with_seat_selection(self, seat_selection: SeatSelection) -> Self {
        Self {
            seat_selection,
            ..self
        }
    }

    /// Same pool with `accepts_new_logins` replaced.
    #[must_use]
    pub fn with_accepts_new_logins(self, accepts_new_logins: bool) -> Self {
        Self {
            accepts_new_logins,
            ..self
        }
    }
}

impl Config {
    /// The seat-selection strategy in force for a dispatch target named by
    /// a `[models.X] provider` value.
    ///
    /// The strategy is a property of a SET of accounts, so it lives on the
    /// pool block. `name` may be a pool (its own strategy applies) or a
    /// provider entry: a provider claimed by a pool inherits that pool's
    /// strategy, and a standalone provider takes the `fill-first` default
    /// -- it has one credential, so there is nothing to select between.
    ///
    /// Validation rejects both a name held by a provider and a pool and a
    /// provider claimed by two pools, so the walk below has at most one
    /// answer for any config that passed the gate.
    pub fn seat_selection_for(&self, name: &str) -> SeatSelection {
        if let Some(pool) = self.pools.get(name) {
            return pool.seat_selection;
        }
        self.pools
            .values()
            .find(|pool| pool.members.iter().any(|member| member == name))
            .map_or_else(SeatSelection::default, |pool| pool.seat_selection)
    }
}
