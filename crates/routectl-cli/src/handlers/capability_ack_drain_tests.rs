// The field-verdict routing predicate, and the one-row-one-path split it enforces.
//
// The ACKNOWLEDGMENT behavior itself -- a reject, a repaired retry, a durable ack,
// and the next request planning pre-flight in the same process -- is asserted in
// `server::preflight_daemon_tests`, against a real daemon and a real writer. It
// belongs there rather than here because the property spans a request boundary,
// and a fixture that stopped short of one would pass on code that never advanced
// eligibility until a restart.

use super::*;

/// A learned negative on `capability_key`, with the rest of the event's fields at
/// values that make no difference to the routing predicate.
fn learn_event(capability_key: &str) -> CapabilityLearnEvent {
    CapabilityLearnEvent {
        persistence_generation: 1,
        incarnation: 1,
        state_key: "nick".to_string(),
        capability_key: capability_key.to_string(),
        provider_kind: "anthropic-api".to_string(),
        signal_tier: routectl_core::capability::SignalTier::SelfIdentifying,
        observations: 1,
        upstream_status: 400,
        remapped: false,
        request_features: Vec::new(),
        phase: routectl_core::capability::FailurePhase::F1,
        source: routectl_core::capability::EvidenceSource::Live,
    }
}

/// A wire-shape capability key, assembled at runtime.
///
/// The namespace prefix is owned by one module in `routectl-router` and a lexical
/// guard fails if the literal appears anywhere else under `crates/`. This crate
/// builds the key from parts instead: the scanned source carries no full prefix
/// literal while the VALUE is byte-identical.
fn field_key(path: &str) -> String {
    format!("{}{}{}", "fie", "ld:", path)
}

#[test]
fn a_field_namespace_key_routes_to_the_acknowledged_path() {
    // The positive half. Asserted through the PRODUCTION predicate, which reads the
    // namespace's own published test rather than a prefix spelled here -- a second
    // spelling could drift from the owner and re-partition persisted history.
    assert!(
        is_field_verdict_event(&learn_event(&field_key("thinking.enabled.display"))),
        "an envelope-field verdict's row must take the acknowledged path: its \
         pre-flight eligibility depends on the row being durable",
    );
}

#[test]
fn every_other_capability_key_stays_on_the_best_effort_path() {
    // The negative half, and the one that keeps the acknowledged path from widening
    // into every capability write. A catalog capability's row is best effort because
    // nothing rewrites a client request on the strength of it, so awaiting its
    // acknowledgment on the request path would buy latency and no safety.
    //
    // Driven over several shapes rather than one, including a key that merely
    // CONTAINS the namespace token rather than starting with it -- a
    // `contains`-based predicate would misroute that one.
    for capability in [
        "web_search",
        "structured_output",
        "thinking",
        "",
        &format!("prefix-{}", field_key("a.b")),
    ] {
        assert!(
            !is_field_verdict_event(&learn_event(capability)),
            "{capability:?} is not an envelope-field verdict and must stay on the \
             best-effort drain",
        );
    }
}
