//! Daemon-scoped observation and failure-injection seams for the `/status` family.
//!
//! # Why these hooks exist, and why they are not a per-call argument
//!
//! Two properties of the fidelity surface are invisible from a `/status` response
//! and from a router snapshot alike: WHICH builder emitted the shared INFO line for
//! a request, and HOW MANY lines that request produced. The response carries neither
//! (the line is deliberately log-only -- a routectl key in the response body would
//! itself be a fidelity leak), and the router's state is identical whether the line
//! was emitted once, twice, or not at all. So a test over a running daemon needs an
//! observation point inside the emitter.
//!
//! A per-call argument cannot reach it. The daemon's own handlers construct their
//! `FidelityEmission` values, so nothing a test passes at the request boundary is in
//! scope where the emission is built. These hooks ride on [`super::StatusState`]
//! instead -- built once per serve process, which is exactly the scope an
//! assembled-daemon test needs.
//!
//! # Why DAEMON-scoped rather than process-global
//!
//! A test binary runs its cases concurrently in one process. A process-global slot
//! is claimed by whichever daemon boots first, and every other test then observes a
//! channel it does not own -- measured on the sibling router-swap seam, where the
//! global shape failed every case at once. One hooks value per daemon, handed in at
//! spawn, gives each test its own.
//!
//! `cfg(test)` ONLY -- not behind a feature. A release build cannot construct these,
//! so no deployment can install an event tap on its own status surface or force a
//! panel to degrade.

use std::sync::Arc;

use super::field_verdict_log::FidelityEvent;

/// A daemon-scoped tap on the fidelity emitter: every event it emits, in order.
///
/// UNBOUNDED, and multi-event rather than one-shot, because the assertion these
/// tests make is a COUNT. "Exactly one event per request" cannot be checked against
/// a one-shot channel: a second send onto a consumed sender is silently dropped, so
/// a daemon emitting twice per request would read as one. An unbounded queue records
/// every emission and lets the test drain and count.
#[derive(Debug)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "the module is private, but this is a \
    test-only seam named from a sibling module tree, so the crate visibility is \
    load-bearing rather than cosmetic -- `pub` would widen it past the crate"
)]
pub(crate) struct FidelityObserver {
    tx: tokio::sync::mpsc::UnboundedSender<FidelityEvent>,
}

/// The receiving half a test drains.
#[expect(
    clippy::redundant_pub_crate,
    reason = "the module is private, but this is a \
    test-only seam named from a sibling module tree, so the crate visibility is \
    load-bearing rather than cosmetic -- `pub` would widen it past the crate"
)]
pub(crate) type FidelityEvents = tokio::sync::mpsc::UnboundedReceiver<FidelityEvent>;

impl FidelityObserver {
    /// A fresh observer and the receiver paired with it.
    pub(crate) fn new() -> (Arc<Self>, FidelityEvents) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Arc::new(Self { tx }), rx)
    }

    /// Record one emitted event.
    ///
    /// A dropped receiver is not an error: the test may have finished while a
    /// builder was still in flight, and a panicking emitter would turn that race
    /// into a spurious daemon failure.
    pub(super) fn record(&self, event: &FidelityEvent) {
        let _ = self.tx.send(event.clone());
    }
}

/// The test-only seams one daemon's status surface carries.
///
/// One value rather than a field per seam, so adding a seam does not re-thread the
/// serve signature -- the plumbing from the spawn call down to `StatusState` is
/// already four hops long, and each hop is a place a new parameter can be dropped
/// silently in one of the two cfg arms.
#[derive(Debug, Default)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "the module is private, but this is a \
    test-only seam named from a sibling module tree, so the crate visibility is \
    load-bearing rather than cosmetic -- `pub` would widen it past the crate"
)]
pub(crate) struct StatusTestHooks {
    /// Where emitted fidelity events go, when a test is watching.
    pub(crate) fidelity_observer: Option<Arc<FidelityObserver>>,
    /// Make the health panel's blocking builder FAIL before it reaches the
    /// fidelity emitter.
    ///
    /// A panic rather than an early unavailable return, because a panic is what
    /// actually reaches `guard_panel`'s degradation path -- the real production
    /// shape of "this panel's data source broke". The point of injecting it here
    /// instead of modelling it is the aggregate's claim: health must leave the
    /// shared claim untouched so doctor still satisfies the observability floor for
    /// that request, and a builder that returned early through some other path
    /// would not prove the same thing about the one that degrades.
    pub(crate) fail_health_builder: bool,
}

impl StatusTestHooks {
    /// Hooks that observe fidelity events and leave every panel healthy.
    pub(crate) fn observing() -> (Self, FidelityEvents) {
        let (observer, events) = FidelityObserver::new();
        (
            Self {
                fidelity_observer: Some(observer),
                fail_health_builder: false,
            },
            events,
        )
    }

    /// The same, with the health panel's builder made to fail.
    pub(crate) fn observing_with_failing_health() -> (Self, FidelityEvents) {
        let (hooks, events) = Self::observing();
        (
            Self {
                fail_health_builder: true,
                ..hooks
            },
            events,
        )
    }
}
