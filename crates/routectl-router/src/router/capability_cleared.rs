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
/// `super::LearnedProbeGuard::settle_success` -- the ONLY settlement arm that
/// clears a resident negative (a same-capability rejection refreshes the entry
/// with backoff; a drop records a transient `OtherError`; neither clears).
///
/// Carries the registry key of the cleared entry so the warm-rebuild replayer
/// removes the same resident negative on boot. No request body, prompt, or
/// upstream text ever enters this struct.
#[derive(Debug, Clone)]
pub struct CapabilityClearedEvent {
    /// The EFFECTIVE persistence generation this event must be stamped with.
    ///
    /// Taken from the registry operation that produced the event, atomically
    /// under the same guard as its read or mutation -- never sampled before or
    /// after. A separate read could be taken across a boundary and stamp the
    /// event with a generation that does not describe the state it reports. A
    /// single request legitimately spans a boundary, so events on one request
    /// may carry DIFFERENT generations.
    pub persistence_generation: u64,
    /// The INCARNATION of the key's state this event describes, from the same
    /// guarded mutation.
    ///
    /// What the generation cannot express: a purge and a later relearn of ONE key
    /// both happen inside one generation, so a stale event queued before the
    /// purge and a genuine post-purge relearn are indistinguishable by generation
    /// alone. The writer compares this against the key's purge floor and drops
    /// only the superseded one.
    pub incarnation: u64,

    /// Routing state key (nickname-or-provider) of the re-probed target.
    pub state_key: String,
    /// Normalized capability key the cleared negative named.
    pub capability_key: String,
    /// Stable provider-kind token of the re-probed target.
    pub provider_kind: String,
}
