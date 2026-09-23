//! The router side of the capability-persistence health gate: one trait an
//! accounting owner implements, and the optional installation the Router holds.
//!
//! # What this gates, and what it deliberately does not
//!
//! The wire-fidelity pre-flight design says a learned verdict may steer
//! traffic pre-flight only while its DURABLE lifecycle is intact. Pre-flight's
//! three escape hatches -- the durable clear a disproving canary performs, the
//! operator purge, and the confirmation acknowledgment that made the verdict
//! eligible in the first place -- are all capability-event WRITES. When those
//! writes cannot be guaranteed, a verdict that turns out to be wrong cannot be
//! taken out of service durably: the in-memory suspension holds only until the
//! process restarts, and the next boot's replay restores the very verdict the
//! canary disproved. So learned pre-flight suspends while persistence is
//! unhealthy.
//!
//! REACTIVE FORWARD-AND-REPAIR IS UNAFFECTED, and that asymmetry is the whole
//! point rather than an omission. The reactive arm acts only AFTER an upstream
//! has rejected a live request, so its evidence is in hand on the request it
//! serves and it needs no durable record to be correct -- a dropped event costs
//! it the chance to act proactively next time, nothing more. Pre-flight acts
//! BEFORE any rejection, purely on a stored verdict, which is what makes the
//! ability to durably retract that verdict a precondition for it.
//!
//! # Why a trait rather than a dependency
//!
//! The health of the capability-event writer is knowable only to the crate that
//! owns the writer, and that crate is one this one must not depend on -- the
//! dependency runs the other way, exactly as it does for
//! [`super::paid_probe_ledger`]. So the contract is declared here, in the crate
//! that CONSUMES it, and the writer-owning crate implements it. Nothing here
//! names a database, a channel, a counter, or a schema: an implementation maps
//! its own saturation, transaction, and degraded-state diagnostics onto this one
//! question, keeping the mechanism on its own side of the boundary.
//!
//! # Fail safe by construction, and explicit in tests
//!
//! The installation is an `Option`, absent in everything `Router::new` builds,
//! and an absent installation reads as NOT GUARANTEED. A build path that never
//! installs a health read therefore suspends learned pre-flight rather than
//! permitting it, so the permissive state is unreachable by forgetting a wiring
//! step -- it takes an explicit installation.
//!
//! That default would make every library test that exercises pre-flight silently
//! exercise the suspended path instead, which is why the test-side opt-in
//! ([`Router::with_capability_writes_assumed_durable_for_tests`]) is a named,
//! `cfg`-gated builder rather than a `cfg(test)` flip of the default. A test that
//! wants pre-flight to fire SAYS so, so the production default and the test
//! default are the same value and the gate is never accidentally absent from the
//! thing under test.

use std::sync::Arc;

use super::Router;

/// Whether durable capability-event persistence can currently be guaranteed.
///
/// `Send + Sync` so the Router can hold one behind an `Arc` and share it across
/// dispatch tasks, mirroring [`super::paid_probe_ledger::PaidProbeLedger`].
pub trait CapabilityPersistenceHealth: Send + Sync {
    /// Whether a capability-event write, clear, or purge submitted right now can
    /// be expected to land durably.
    ///
    /// `false` is the SAFE answer and an implementation that cannot establish
    /// the state it needs returns it rather than guessing: the consequence of a
    /// false `false` is that learned pre-flight stays dormant while reactive
    /// repair serves every request, and the consequence of a false `true` is a
    /// disproved verdict that cannot be durably retracted.
    ///
    /// SYNCHRONOUS and cheap by contract. It is consulted on the dispatch path,
    /// per considered row, so an implementation reads already-maintained state
    /// rather than probing storage. An implementation that needed to await
    /// belongs behind its own cached read on its own side of the boundary.
    fn capability_writes_durable(&self) -> bool;
}

impl Router {
    /// Install the capability-persistence health read, consuming and returning
    /// the Router.
    ///
    /// A CONSUMING BUILDER rather than a `&mut self` setter, for the same reason
    /// [`Router::with_paid_probe_ledger`] is one: the installation belongs to
    /// boot, before the Router is published behind its `ArcSwap`, and a setter
    /// would advertise a post-publication mutation that cannot be performed
    /// through a shared `Arc` anyway.
    ///
    /// A second call REPLACES the installation, which is what a rebuild needs:
    /// `carry_over_learned_from` attaches the outgoing one, and a boot path
    /// installing its own afterwards must win over it.
    #[must_use]
    pub fn with_capability_persistence_health(
        mut self,
        health: Arc<dyn CapabilityPersistenceHealth>,
    ) -> Self {
        self.capability_health = Some(health);
        self
    }

    /// The installed health read, or `None` on a Router nobody installed one on.
    ///
    /// Crate-internal: the decision itself goes through
    /// [`Self::capability_writes_durable`], so no call site has to remember what
    /// the absent case means. This exists for the carry-over to attach.
    pub(crate) const fn capability_persistence_health(
        &self,
    ) -> Option<&Arc<dyn CapabilityPersistenceHealth>> {
        self.capability_health.as_ref()
    }

    /// Whether durable capability persistence can be guaranteed right now.
    ///
    /// THE one predicate every persistence-dependent gate asks, so "durable
    /// writes are assured" cannot be restated as a different condition at a
    /// second site. With no health read installed it answers `false` -- the
    /// fail-safe default this whole module exists to make unavoidable.
    pub(crate) fn capability_writes_durable(&self) -> bool {
        self.capability_health
            .as_ref()
            .is_some_and(|health| health.capability_writes_durable())
    }

    /// [`Self::capability_writes_durable`], for the cross-crate tests that install
    /// the real adapter and need to read the verdict it produces.
    ///
    /// Gated to test builds and named `_for_tests`, because the predicate itself is
    /// crate-internal on purpose: publishing it would invite an out-of-crate caller
    /// to ask the question and act on the answer, which is the second
    /// persistence-gate site this seam exists to prevent. A test reading the
    /// verdict is not a second gate.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    #[must_use]
    pub fn capability_writes_durable_for_tests(&self) -> bool {
        self.capability_writes_durable()
    }

    /// Install a health read that always reports durable writes, for tests that
    /// need pre-flight to fire.
    ///
    /// GATED to test builds, and NAMED for what it asserts rather than spelled as
    /// a flipped default: a test exercising pre-flight has to say that it assumes
    /// durable persistence, so the production default and the library-test
    /// default are one value and no test can exercise the gate's absence by
    /// accident.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    #[must_use]
    pub fn with_capability_writes_assumed_durable_for_tests(self) -> Self {
        /// Always durable. Carries no state: a test that needs the UNHEALTHY
        /// answer installs nothing at all, which is the production default.
        struct AssumedDurable;
        impl CapabilityPersistenceHealth for AssumedDurable {
            fn capability_writes_durable(&self) -> bool {
                true
            }
        }
        self.with_capability_persistence_health(Arc::new(AssumedDurable))
    }
}

#[cfg(test)]
#[path = "capability_health_tests.rs"]
mod capability_health_tests;
