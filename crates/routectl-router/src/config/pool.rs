//! The `[pools.<name>]` config block: a set of same-kind provider entries
//! (accounts) sharing one seat-selection strategy.
//!
//! A pool is the operator-facing multi-account shape: `[models.X] provider`
//! may name a provider entry OR a pool, and the pool owns the strategy that
//! picks among its members. Membership is by provider-entry NAME, so a pool
//! never carries credential material and stays credential-generic.

use serde::{Deserialize, Serialize};

use super::{Config, ProviderEntry, SeatSelection};

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

    /// Consecutive failed attempts that open the circuit breaker on each
    /// member of this pool. A member's own `circuit_failures` wins over
    /// this value. None = inherit (the breaker stays off unless a member
    /// sets it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_failures: Option<u32>,

    /// How long an opened breaker on a member stays open, in
    /// milliseconds. A member's own `circuit_cooldown_ms` wins over this
    /// value. None = inherit the 30 s default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_cooldown_ms: Option<u64>,

    /// Requests per minute allowed through each member of this pool. A
    /// member's own `rpm_limit` wins over this value. None = inherit
    /// (unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,

    /// Per-attempt request timeout for complete and count-tokens requests
    /// through this pool's members. Precedence: member provider > this
    /// pool > `[retry] request_timeout_ms` > no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,

    /// Per-attempt first-content timeout for streams through this pool's
    /// members. Precedence: `[models.X]` > member provider > this pool >
    /// `[retry] stream_first_byte_timeout_ms` > the 10 min default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,
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
            circuit_failures: None,
            circuit_cooldown_ms: None,
            rpm_limit: None,
            request_timeout_ms: None,
            stream_first_byte_timeout_ms: None,
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

    /// Same pool with `circuit_failures` set.
    #[must_use]
    pub fn with_circuit_failures(self, circuit_failures: u32) -> Self {
        Self {
            circuit_failures: Some(circuit_failures),
            ..self
        }
    }

    /// Same pool with `circuit_cooldown_ms` set.
    #[must_use]
    pub fn with_circuit_cooldown_ms(self, circuit_cooldown_ms: u64) -> Self {
        Self {
            circuit_cooldown_ms: Some(circuit_cooldown_ms),
            ..self
        }
    }

    /// Same pool with `rpm_limit` set.
    #[must_use]
    pub fn with_rpm_limit(self, rpm_limit: u32) -> Self {
        Self {
            rpm_limit: Some(rpm_limit),
            ..self
        }
    }

    /// Same pool with `request_timeout_ms` set.
    #[must_use]
    pub fn with_request_timeout_ms(self, request_timeout_ms: u64) -> Self {
        Self {
            request_timeout_ms: Some(request_timeout_ms),
            ..self
        }
    }

    /// Same pool with `stream_first_byte_timeout_ms` set.
    #[must_use]
    pub fn with_stream_first_byte_timeout_ms(self, stream_first_byte_timeout_ms: u64) -> Self {
        Self {
            stream_first_byte_timeout_ms: Some(stream_first_byte_timeout_ms),
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

    /// The catalog-selector provider kind for a `[models.X] provider` value,
    /// or `""` when the value names neither a provider entry nor a pool with a
    /// resolvable member.
    ///
    /// `[models.X] provider` resolves against providers and POOLS in one
    /// namespace, so a bare `config.providers.get(...)` lookup misses for every
    /// pool-backed model -- and an empty kind identifies no catalog cell at
    /// all, silently blanking that model's pricing, context window, and output
    /// ceiling. A pool therefore answers with a MEMBER's kind: `validate_pools`
    /// rejects a pool whose members disagree on `kind_str()`, so the first
    /// resolvable member is representative of the whole pool.
    ///
    /// Provider entries are consulted first, which is unambiguous because the
    /// same validator rejects a name held by both a provider entry and a pool.
    #[must_use]
    pub fn provider_kind_for_target(&self, name: &str) -> &'static str {
        if let Some(entry) = self.providers.get(name) {
            return entry.kind_str();
        }
        self.pools
            .get(name)
            .and_then(|pool| {
                pool.members
                    .iter()
                    .find_map(|member| self.providers.get(member))
            })
            .map_or("", ProviderEntry::kind_str)
    }
}
