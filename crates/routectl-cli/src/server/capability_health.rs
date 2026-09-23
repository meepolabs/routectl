//! The CLI-side bridge from the router's capability-persistence health gate to
//! the usage writer whose health actually answers the question.
//!
//! The router declares the contract (`routectl_router::CapabilityPersistenceHealth`)
//! because it CONSUMES it, and the usage crate owns the writer whose state answers
//! it. Neither depends on the other, so this crate -- the one that already depends
//! on both and owns the daemon's boot -- is where the two meet. Its sibling
//! `paid_probe_ledger` bridges the same two crates for the same reason.
//!
//! # What "durable" means here, and why it is read from counters
//!
//! The router asks one question: can a capability-event write, clear, or purge
//! submitted RIGHT NOW be expected to land? The writer already maintains exactly
//! the signals that answer it on its shared counters -- store faults seen by the
//! consumer thread, and capability writes the channel refused as full or as closed
//! -- and it maintains them because it has to for its own degraded-state logging.
//! Reading them is therefore free and, more importantly, reads state the writer
//! itself owns rather than a second opinion this module maintains in parallel.
//!
//! # ZERO, not a baseline
//!
//! The predicate is an absolute level: durable only while EVERY capability-persistence
//! failure counter reads zero. It does not compare against values sampled at install
//! time, and that is a correction rather than a simplification.
//!
//! An install-time baseline silently forgave every failure that happened BEFORE the
//! adapter was installed, and the daemon's boot has such failures: the startup warm
//! commits a boundary tombstone batch before this is installed, so a tombstone whose
//! channel was refused, or whose transaction failed, was baselined away and the very
//! first health read answered DURABLE. That is exactly backwards -- a boot whose
//! tombstone did not land is a boot whose replay boundary is not where the registry
//! thinks it is, which is the strongest possible reason to keep verdicts off the
//! pre-flight path. A failure needed a SECOND failure after install to be noticed at
//! all.
//!
//! Reading zero also means a single pre-install failure is enough on its own,
//! permanently. The counters are monotonic, so once any of them moves this answers
//! false for the life of the process.
//!
//! STICKINESS IS THE SAFE DIRECTION AND IT IS A REAL COST, stated rather than
//! glossed: a daemon that dropped one capability write at 3am -- or at boot --
//! serves the rest of its life with reactive forward-and-repair only. That is a
//! bounded loss (every request is still served, and correctly) against an unbounded
//! one (a verdict rewriting traffic that no boot can explain), and the recovery is a
//! restart, which a warm rebuild makes cheap. A self-clearing read would need a
//! notion of "the failing write has been superseded" that the counters cannot
//! express.
//!
//! # Fail safe by construction
//!
//! Nothing here is installed by `Router::new`. Every Router this crate builds
//! outside the daemon's boot carries no health read and therefore suspends learned
//! pre-flight entirely; the boot is the one place that installs one, after the
//! writer exists.

use std::sync::Arc;

use routectl_router::{CapabilityPersistenceHealth, Router};
use routectl_usage::{UsageCounters, UsageHandle};

/// Reports capability-persistence health from the usage writer's own counters.
///
/// Holds the shared counters and NOTHING else: no connection, no path, no cached
/// verdict, and deliberately no install-time baseline (see the module docs). The
/// counters are the writer's own state, so this adapter cannot disagree with the
/// writer about whether a write failed, and holding no state of its own means there
/// is nothing here that can drift from them.
pub(super) struct UsageCapabilityHealth {
    counters: Arc<UsageCounters>,
}

impl CapabilityPersistenceHealth for UsageCapabilityHealth {
    /// Whether NOTHING has ever failed: every capability-persistence failure
    /// counter reads zero.
    ///
    /// THREE independent signals, and ALL THREE must be zero. None subsumes another:
    ///
    /// - `write_errors` is a durable-STORE fault -- the insert reached the writer and
    ///   did not land (a failed open, a failed transaction, a thread that would not
    ///   spawn);
    /// - `capability_events_dropped_full` is BACK-PRESSURE -- the writer is alive but
    ///   behind, so the row never reached it;
    /// - `capability_writer_unavailable` is TEARDOWN -- the channel is closed, so
    ///   the row never reached it and never will.
    ///
    /// All three mean the same thing for the router's question and none of the three
    /// moves the others, so reading fewer than three would leave a whole class of
    /// failure invisible to the gate.
    ///
    /// Compared against ZERO rather than an install-time baseline: a failure during
    /// boot -- a refused or failed boundary tombstone, before this adapter exists --
    /// must keep pre-flight suspended, and a baseline forgave exactly those. See the
    /// module docs.
    ///
    /// Deliberately NOT keyed on the events this process happens to care about: the
    /// counters are process-wide, so an unrelated capability row's failure also
    /// suspends learned pre-flight. That is the conservative reading and it is the
    /// right one -- a writer that just failed to persist one row is not a writer
    /// whose next clear or purge can be guaranteed.
    ///
    /// Ordinary usage-RECORD drops are excluded, and that exclusion is deliberate:
    /// `dropped_full` on a usage row is accounting telemetry falling behind, which
    /// says nothing about whether a capability write would land, and folding it in
    /// would suspend pre-flight on a busy daemon for no safety gain.
    fn capability_writes_durable(&self) -> bool {
        self.counters.write_errors() == 0
            && self.counters.capability_events_dropped_full() == 0
            && self.counters.capability_writer_unavailable() == 0
    }
}

/// A health read over `usage`'s counters.
///
/// Carries no baseline: the predicate reads zero (see the module docs), so there is
/// nothing to sample at install time and no way for this to be installed "too late"
/// to notice a failure.
fn capability_health(usage: &UsageHandle) -> Arc<UsageCapabilityHealth> {
    Arc::new(UsageCapabilityHealth {
        counters: Arc::clone(usage.counters()),
    })
}

/// Install the capability-persistence health read on the owned Router, before
/// publication.
///
/// Consuming, like the builder it calls, so the installation can only happen while
/// the Router is still owned -- which is to say at boot, before the `ArcSwap` makes
/// it shared. A reload needs no call here at all:
/// `Router::carry_over_learned_from` attaches the outgoing installation to the
/// replacement, so both rebuild paths keep gating on the one writer they both
/// submit to. That matters because a replacement that lost the read would suspend
/// learned pre-flight on every reload until a boot path reinstalled it.
pub(super) fn install_capability_health(router: Router, usage: &UsageHandle) -> Router {
    router.with_capability_persistence_health(capability_health(usage))
}

#[cfg(test)]
#[path = "capability_health_wiring_tests.rs"]
mod capability_health_wiring_tests;
