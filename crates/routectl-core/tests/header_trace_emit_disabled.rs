//! Emit-path coverage for the four header-trace emitters with header
//! tracing OFF (the default). Pairs with `header_trace_emit_enabled.rs`:
//! a dedicated integration-test binary so `header_trace_enabled()`
//! freezes to `false` in its own process. See that file for the full
//! OnceLock rationale.
//!
//! This is the toggle-off arm: even with a TRACE subscriber active (so
//! the only thing gating emission is the header-trace toggle), every
//! emitter must no-op when `ROUTECTL_TRACE_HEADERS` is unset.

mod common;

use common::capture_events;
use routectl_core::{
    header_trace_enabled, headers_to_json, trace_egress_headers, trace_ingress_headers,
    trace_outgoing_headers, trace_upstream_response_headers,
};

#[test]
fn header_emitters_emit_nothing_when_tracing_disabled() {
    // Arrange: force the toggle OFF in this process independent of the
    // ambient environment, before the OnceLock freezes.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("ROUTECTL_TRACE_HEADERS") };
    assert!(
        !header_trace_enabled(),
        "OnceLock must freeze to false when the env var is unset"
    );
    let headers = headers_to_json([("x-secret", b"value".as_slice())]);

    // Act: a TRACE subscriber is installed, so the header-trace toggle is
    // the only remaining gate. With it OFF every emitter must short-circuit.
    let events = capture_events(|| {
        trace_ingress_headers("anthropic", &headers);
        trace_outgoing_headers("openai-compat", "prov-1", &headers);
        trace_upstream_response_headers("openai-compat", "prov-1", &headers);
        trace_egress_headers("anthropic", &headers);
    });

    // Assert: zero events -- the emitters returned before reaching trace!.
    assert!(
        events.is_empty(),
        "header tracing OFF must suppress all emission; captured: {events:#?}"
    );
}
