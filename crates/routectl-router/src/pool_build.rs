//! Typed outcomes of compiling a `[pools.<name>]` block into dispatch seats.
//!
//! A pool compiles down at build time into the same `ResolvedModel.seats`
//! shape a seat-pinned provider set has always used: one seat per member
//! provider entry, built from that member's OWN `api_key_ref`. A member that
//! cannot be built is OMITTED rather than sinking the pool, so a pool with
//! one dead account keeps serving through its survivors.
//!
//! The omission set is data, not a log line: the same sanitized report drives
//! the degraded-pool WARN, the router's per-reason counters, and the
//! operator-facing pools report. Nothing here carries token material, a
//! credential path, an account id, or a store error string -- a member is
//! named by its `[providers]` table key, its kind by the fixed kind token,
//! and its failure by one of four allowlisted reasons.

use std::sync::Arc;

use crate::seat_pool::SeatTarget;

/// Why one pool member was left out of the compiled seat set.
///
/// An ALLOWLIST, deliberately: the reason reaches a structured log field and
/// an operator-facing report, and a store or provider error string routed
/// there would publish credential paths, account ids, and upstream response
/// bytes. Each variant is derived from the SHAPE of the failure (an absent
/// ref, an unparseable ref, the error variant a build returned), never from
/// the text of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolOmissionReason {
    /// The member's provider entry declares no credential reference at all,
    /// so there is nothing to authenticate the account with.
    CredentialMissing,
    /// The member's credential reference is present but the store could not
    /// produce a credential for it (not logged in, refresh refused, backing
    /// file unreadable).
    CredentialUnreadable,
    /// The member's credential reference is present but not a well-formed
    /// secret reference.
    CredentialInvalid,
    /// The credential was usable but the provider instance itself failed to
    /// construct (a rejected base URL, an incoherent provider block).
    ProviderInitFailed,
}

impl PoolOmissionReason {
    /// The stable token for this reason: the `reason` field of the
    /// degraded-pool WARN and the operator-facing report's vocabulary.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::CredentialMissing => "credential_missing",
            Self::CredentialUnreadable => "credential_unreadable",
            Self::CredentialInvalid => "credential_invalid",
            Self::ProviderInitFailed => "provider_init_failed",
        }
    }
}

/// One omitted pool member, in the only three facts that may leave the build:
/// the member's `[providers]` table key, its fixed provider-kind token, and
/// an allowlisted reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMemberOmission {
    /// The `[providers]` table key of the omitted member.
    pub member: String,
    /// The member's stable provider-kind token, or `unknown` when the member
    /// names no provider entry (validation rejects that; the build stays
    /// defensive).
    pub provider_kind: &'static str,
    /// Why the member was omitted.
    pub reason: PoolOmissionReason,
}

/// The sanitized build report for one pool: what it was configured with,
/// what it can actually serve, and every member it lost.
///
/// Retained on the built `Router` (see `Router::pool_reports`) so the read
/// side reports the same facts the build observed rather than re-deriving
/// them from config, which cannot see a credential failure at all.
///
/// `usable_members == 0` is the Unavailable outcome: the pool serves nothing,
/// and every model naming it is unroutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReport {
    /// The `[pools]` table key.
    pub pool: String,
    /// Every selectable `[models]` nickname that resolved against this pool.
    pub models: Vec<String>,
    /// How many members the block declared.
    pub configured_members: usize,
    /// How many members compiled into a dispatch seat.
    pub usable_members: usize,
    /// Every member left out, in declaration order.
    pub omissions: Vec<PoolMemberOmission>,
}

impl PoolReport {
    /// Whether this pool lost at least one member but still serves. A
    /// degraded pool is a live operational state, not a failure: dispatch
    /// runs on the survivors.
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        self.usable_members > 0 && !self.omissions.is_empty()
    }

    /// Whether this pool serves nothing.
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        self.usable_members == 0
    }
}

/// The compiled outcome of one pool: a seat set to dispatch across, or
/// nothing.
///
/// Two variants rather than an `Option<Arc<[SeatTarget]>>` plus a side
/// channel, so a caller cannot reach the seats without having handled the
/// zero-usable case -- which is a boot refusal, not an empty walk.
#[derive(Debug, Clone)]
pub enum PoolOutcome {
    /// At least one member compiled. The seat set is shared by every model
    /// naming this pool.
    Ready {
        /// One seat per usable member, in declaration order.
        seats: Arc<[SeatTarget]>,
        /// The omission set (empty for a fully healthy pool).
        omissions: Vec<PoolMemberOmission>,
    },
    /// No member compiled. The pool serves nothing.
    Unavailable {
        /// Why each member was lost.
        omissions: Vec<PoolMemberOmission>,
    },
}

impl PoolOutcome {
    /// The seat set, or `None` for an unavailable pool.
    #[must_use]
    pub const fn seats(&self) -> Option<&Arc<[SeatTarget]>> {
        match self {
            Self::Ready { seats, .. } => Some(seats),
            Self::Unavailable { .. } => None,
        }
    }

    /// The omission set, whichever outcome this is.
    #[must_use]
    pub fn omissions(&self) -> &[PoolMemberOmission] {
        match self {
            Self::Ready { omissions, .. } | Self::Unavailable { omissions } => omissions,
        }
    }
}

/// The boot / reload refusal for every pool that serves nothing, naming BOTH
/// the pool and the models routed at it. `None` when every pool that any
/// model names has at least one usable member.
///
/// A zero-usable pool behind a selectable model is not a degraded state that
/// dispatch can walk past: every request for that model would find an empty
/// seat set. Refusing at build time gives the operator the pool and the
/// model, instead of a server that starts healthy and fails at first
/// traffic.
#[must_use]
pub fn unavailable_pool_error(reports: &[PoolReport]) -> Option<String> {
    let lines: Vec<String> = reports
        .iter()
        .filter(|report| report.is_unavailable() && !report.models.is_empty())
        .map(|report| {
            format!(
                "pool `{}` has no usable member of {} configured (omitted: {}); \
                 models routed at it: {}",
                report.pool,
                report.configured_members,
                omission_summary(&report.omissions),
                report.models.join(", "),
            )
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// `member=reason` pairs for an operator-facing message. Only the member key
/// and the allowlisted reason token, never an error string.
fn omission_summary(omissions: &[PoolMemberOmission]) -> String {
    omissions
        .iter()
        .map(|o| format!("{}={}", o.member, o.reason.token()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "pool_build_tests.rs"]
mod pool_build_tests;
