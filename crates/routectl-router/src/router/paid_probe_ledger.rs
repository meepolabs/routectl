//! The router side of the crash-safe paid-probe budget: one trait the
//! usage-writer actor implements, one closed outcome vocabulary, and the
//! optional installation the Router holds.
//!
//! WHY A TRAIT RATHER THAN A DEPENDENCY: the reservation must be atomic and
//! durable, which only the usage-writer actor can promise -- and that actor
//! lives in a crate this one must not depend on (the dependency runs the other
//! way). So the contract is declared here, in the crate that CONSUMES it, and
//! the actor-owning crate implements it. Nothing in this module names a
//! database, a channel, or a schema, and no outcome is spelled after one
//! implementation's mechanism: an adapter maps its own saturation, transaction,
//! or rollover diagnostics onto this vocabulary, keeping its detail on its own
//! side of the boundary.
//!
//! WHY THERE IS NO RELEASE, REFUND, OR TIMEOUT METHOD, stated because its
//! absence is the design and not an omission: a committed unit is spent. A
//! probe that reserved and then timed out, was cancelled, or died with the
//! process leaves its unit consumed, because the alternative -- releasing a
//! unit whose call may have already reached the upstream -- is how a crash
//! loop spends a day's cap several times over. The feature favors underuse
//! over overspend, so the only operation is the reservation itself.
//!
//! FAIL CLOSED BY CONSTRUCTION: the installation is an `Option`, absent in
//! everything `Router::new` builds. A build path that never installs a ledger
//! cannot make a paid call at all, so the permissive state is unreachable by
//! forgetting a wiring step -- it takes an explicit installation.

use std::sync::Arc;

use super::Router;

/// Atomic, durable reservation of one paid-probe unit against a configured
/// provider's UTC-day budget.
///
/// `Send + Sync` so the Router can hold one behind an `Arc` and share it
/// across dispatch tasks, mirroring `crate::k_estimator::KEstimator`.
#[async_trait::async_trait]
pub trait PaidProbeLedger: Send + Sync {
    /// Reserve one unit for `provider` against `daily_cap`, and answer what
    /// the accounting state permits.
    ///
    /// `provider` is the CONFIGURED provider name -- the same key
    /// `[fidelity.paid_probe_daily_caps]` is written under -- and `daily_cap`
    /// is that key's configured value. Both are passed in rather than read by
    /// the implementation, so the cap the caller gated on and the cap the
    /// reservation is checked against cannot be two different numbers.
    ///
    /// THE UTC DAY IS NOT REPORTED BACK, and its absence is the boundary
    /// working: the implementation owns the `(provider, day)` key entirely --
    /// which day a unit landed in, how the day is resolved, and how a rollover
    /// is handled are all accounting concerns. What the router needs to
    /// authorize a dial is whether a unit was committed and where that leaves
    /// the budget, so a day string here would be state the router carries,
    /// logs, and eventually reasons about without owning it.
    ///
    /// A commit means the unit is DURABLE. Every other answer means no paid
    /// call may happen, and an implementation that cannot establish the state
    /// it needs answers a refusal rather than guessing.
    async fn reserve_paid_probe_unit(&self, provider: &str, daily_cap: u32)
    -> PaidProbeReservation;
}

/// What one reservation attempt established. Closed set, and every variant has
/// a crate-internal log token (`as_str`).
///
/// Exactly one variant permits a paid call (`permits_paid_call`). Both of those
/// methods are crate-internal -- written as plain code spans here rather than
/// intra-doc links, because a public item's docs may not link a private one.
/// The refusals stay DISTINCT rather than collapsing into one "no" because
/// they are operationally different: an exhausted cap is the budget working as
/// configured, while a write failure or malformed state is accounting the
/// operator needs to know is broken -- and a single refusal token would make
/// a silently unhealthy ledger look like a lane that simply spent its day.
///
/// MECHANISM-NEUTRAL BY CONSTRUCTION: no variant names a channel, a queue, a
/// table, or a transaction. An implementation's own diagnostics stay on its
/// side of the boundary and map onto these words; the reverse -- a variant
/// spelled after one implementation's mechanism -- would make this enum
/// unusable by any other and leak the mechanism into every router log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaidProbeReservation {
    /// One unit is durably reserved. The ONLY outcome that permits a dial.
    Committed {
        /// Units committed for this provider's current accounting day,
        /// INCLUDING this one, as the implementation counted them.
        used: u32,
        /// The cap this reservation was checked against.
        cap: u32,
    },
    /// The provider's units for the current accounting day are already spent,
    /// or the configured cap is zero. Nothing was written.
    CapExhausted,
    /// The stored accounting state could not be read as a count (absent where
    /// it must exist, or not a number). Refused rather than treated as zero:
    /// reading unparseable state as "nothing spent yet" is how a corrupt
    /// counter authorizes a full cap's worth of calls every time it is read.
    MalformedState,
    /// The durable write was attempted and did not land.
    WriteFailed,
    /// The accounting layer is saturated, so the reservation could not even be
    /// submitted. Distinct from [`Self::WriteFailed`]: nothing was attempted,
    /// and the condition is LOAD rather than a fault, so the lane may reach a
    /// different answer later without anything being repaired.
    Overloaded,
    /// No accounting is reachable: no ledger is installed, or the accounting
    /// layer is shutting down. THE DEFAULT ANSWER of a Router nobody installed
    /// a ledger on, which is what makes the absent state fail closed.
    Unavailable,
}

impl PaidProbeReservation {
    /// Whether a paid upstream call may proceed on the strength of this
    /// outcome.
    ///
    /// `pub(crate)` deliberately, though the enum is public: the enum must be
    /// public because trait implementors outside this crate RETURN it, but the
    /// authorization decision is the router's alone. Publishing it would invite
    /// an out-of-crate caller to ask the question and act on the answer.
    ///
    /// Production authorization matches `Committed` directly because it also
    /// needs the committed counts; this predicate pins the same classification
    /// in tests, so it remains test-only in normal builds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn permits_paid_call(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Closed-set log token for this outcome. Carries no count or provider
    /// identity -- the caller logs those from its own fields.
    ///
    /// `pub(crate)` for the same reason as [`Self::permits_paid_call`], plus
    /// one of its own: these tokens are what router log lines and the future
    /// accounting-health panel are keyed on, so they are an internal
    /// vocabulary this crate may retune, not a string contract an out-of-crate
    /// reader may match on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Committed { .. } => "committed",
            Self::CapExhausted => "cap_exhausted",
            Self::MalformedState => "malformed_state",
            Self::WriteFailed => "write_failed",
            Self::Overloaded => "overloaded",
            Self::Unavailable => "unavailable",
        }
    }
}

impl Router {
    /// Install the paid-probe ledger, consuming and returning the Router.
    ///
    /// A CONSUMING BUILDER rather than a `&mut self` setter, deliberately: the
    /// installation belongs to boot, before the Router is published behind its
    /// `ArcSwap`, and a setter would advertise a post-publication mutation
    /// that cannot be performed through a shared `Arc` anyway. Taking
    /// ownership puts that restriction in the type instead of in a doc comment
    /// nobody has to read.
    ///
    /// A second call REPLACES the installation, which is what a rebuild needs:
    /// `carry_over_learned_from` attaches the outgoing one, and a boot path
    /// installing its own afterwards must win over it.
    #[must_use]
    pub fn with_paid_probe_ledger(mut self, ledger: Arc<dyn PaidProbeLedger>) -> Self {
        self.paid_probe_ledger = Some(ledger);
        self
    }

    /// The installed ledger, or `None` on a Router nobody installed one on.
    ///
    /// Crate-internal: the reservation itself goes through
    /// [`Self::reserve_paid_probe_unit`], so no call site has to remember what
    /// the absent case means. This exists for the carry-over to attach and for
    /// the tests to prove installation identity.
    pub(crate) const fn paid_probe_ledger(&self) -> Option<&Arc<dyn PaidProbeLedger>> {
        self.paid_probe_ledger.as_ref()
    }

    /// Reserve one paid-probe unit for `provider` against `daily_cap`.
    ///
    /// THE seam the paid path calls before any outbound paid dial. With no
    /// ledger installed it answers [`PaidProbeReservation::Unavailable`]
    /// without inventing a permission -- the fail-closed default this whole
    /// module exists to make unavoidable.
    ///
    /// Called by paid-probe authorization before the outbound dial. A committed
    /// answer is the permission the dial consumes; every other answer prevents the
    /// call. With no ledger installed the path therefore remains fail closed.
    pub(crate) async fn reserve_paid_probe_unit(
        &self,
        provider: &str,
        daily_cap: u32,
    ) -> PaidProbeReservation {
        match self.paid_probe_ledger.as_ref() {
            Some(ledger) => ledger.reserve_paid_probe_unit(provider, daily_cap).await,
            None => PaidProbeReservation::Unavailable,
        }
    }
}

#[cfg(test)]
#[path = "paid_probe_ledger_tests.rs"]
mod paid_probe_ledger_tests;
