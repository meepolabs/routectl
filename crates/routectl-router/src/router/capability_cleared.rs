//! Probe-settled clear event: a resident learned negative that a successful
//! re-probe cleared, riding out on [`super::DispatchMeta`] to the
//! usage-capture layer.
//!
//! The router does not depend on the ledger writer, so a cleared event
//! travels on the dispatch meta rather than being written here -- the same
//! in-memory persistence hook [`super::CapabilityLearnEvent`] and
//! [`super::CapabilityObserveEvent`] use. The cleared arm is required because
//! the live re-probe path settles a negative in memory
//! ([`crate::learned_capability::LearnedCapabilityRegistry::record_probe_outcome`]
//! on success), which a replay-through-admission boot cannot reproduce: without
//! a persisted clear, every restart resurrects a probe-settled negative.

/// A single probe-settled clear captured at
/// [`super::LearnedProbeGuard::settle_success`] -- the ONLY settlement arm that
/// clears a resident negative (a same-capability rejection refreshes the entry
/// with backoff; a drop records a transient `OtherError`; neither clears).
///
/// Carries the registry key of the cleared entry so the warm-rebuild replayer
/// removes the same resident negative on boot. No request body, prompt, or
/// upstream text ever enters this struct.
#[derive(Debug, Clone)]
pub struct CapabilityClearedEvent {
    /// Routing state key (nickname-or-provider) of the re-probed target.
    pub state_key: String,
    /// Normalized capability key the cleared negative named.
    pub capability_key: String,
    /// Stable provider-kind token of the re-probed target.
    pub provider_kind: String,
}
