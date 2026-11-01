//! Typed outcomes of compiling a `[pools.<name>]` block into dispatch seats.
//!
//! A pool compiles down at build time into the same `ResolvedModel.seats`
//! shape a seat-pinned provider set has always used: one seat per member
//! provider entry, built from that member's OWN `api_key_ref`. A member whose
//! credential cannot be read is kept as a PLACEHOLDER seat (no provider, its
//! account marked unusable); a member that cannot be built for any other
//! reason refuses the config. The pool keeps serving through its built seats,
//! and a pool of placeholders alone still builds.
//!
//! Nothing here carries token material, a credential path, an account id
//! beyond the seat's own account key, or a store error string: a member is
//! named by its `[providers]` table key, its kind by the fixed kind token,
//! and its failure by an allowlisted cause.

use std::sync::Arc;

use crate::runtime_state::UnusableCause;
use crate::seat_pool::SeatTarget;

/// One pool member kept as a placeholder seat, in the only facts that may
/// leave the build: the member's `[providers]` table key, its fixed
/// provider-kind token, why its credential is unusable, and the credential
/// account the mark lands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolPlaceholder {
    /// The `[providers]` table key of the member.
    pub member: String,
    /// The member's stable provider-kind token.
    pub provider_kind: &'static str,
    /// Why the member's credential could not be read, chosen by the store
    /// failure's variant.
    pub cause: UnusableCause,
    /// The seat's credential account (`crate::seat_pool::credential_account`),
    /// or `None` for a forwarded member, which has no account to mark.
    pub account: Option<String>,
}

/// The sanitized build report for one pool: what it was configured with,
/// what it built, and every member it kept as a placeholder.
///
/// Retained on the built `Router` (see `Router::pool_reports`) so the read
/// side reports the same facts the build observed rather than re-deriving
/// them from config, which cannot see a credential failure at all. The live
/// per-seat eligibility is `Router::pool_eligibility`, not this report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReport {
    /// The `[pools]` table key.
    pub pool: String,
    /// Every selectable `[models]` nickname that resolved against this pool.
    pub models: Vec<String>,
    /// How many members the block declared.
    pub configured_members: usize,
    /// How many members compiled into a dispatch seat with a provider.
    pub usable_members: usize,
    /// Every member kept as a placeholder seat, in declaration order.
    pub placeholders: Vec<PoolPlaceholder>,
}

impl PoolReport {
    /// Whether the build kept at least one member as a placeholder seat.
    /// A degraded pool is a live operational state, not a failure: dispatch
    /// runs on the built seats, and a pool of placeholders alone answers a
    /// local refusal until a credential recovers.
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        !self.placeholders.is_empty()
    }

    /// Whether this pool compiled no seat at all, placeholder or built.
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        self.usable_members == 0 && self.placeholders.is_empty()
    }
}

/// The compiled seat set of one pool, shared by every model naming it.
#[derive(Debug, Clone)]
pub(crate) struct PoolOutcome {
    /// One seat per member, built or placeholder, in declaration order.
    pub(crate) seats: Arc<[SeatTarget]>,
    /// The placeholder set (empty when every member built).
    pub(crate) placeholders: Vec<PoolPlaceholder>,
}

#[cfg(test)]
#[path = "pool_build_tests.rs"]
mod pool_build_tests;
