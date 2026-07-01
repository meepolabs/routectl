//! Emit-path coverage for the four header-trace emitters
//! (`trace_ingress_headers`, `trace_outgoing_headers`,
//! `trace_upstream_response_headers`, `trace_egress_headers`) with header
//! tracing ENABLED. Closes the gap where the pure gate predicate
//! (`header_trace_should_emit`) and the JSON builder (`headers_to_json`)
//! were tested but no test ever drove an emitter through to a real
//! `tracing::trace!` call -- so a regression that wired an emitter but
//! never fired `trace!` could ship green.
//!
//! Why a dedicated integration-test binary: `header_trace_enabled()`
//! reads `ROUTECTL_TRACE_HEADERS` once through a process-frozen
//! `OnceLock`. A separate test binary gets its own process, so setting
//! the env var at the start of the single test below freezes the toggle
//! to `true` here without disturbing any other test's process. The
//! companion `header_trace_emit_disabled.rs` binary covers the OFF case
//! in its own process.

mod common;

use common::capture_events;
use routectl_core::{
    HDR_MSG_EGRESS, HDR_MSG_INGRESS, HDR_MSG_OUTGOING, HDR_MSG_UPSTREAM, header_trace_enabled,
    headers_to_json, trace_egress_headers, trace_ingress_headers, trace_outgoing_headers,
    trace_upstream_response_headers,
};

#[test]
fn header_emitters_fire_trace_events_when_tracing_enabled() {
    // Arrange: freeze the toggle to ON *before* the OnceLock is first
    // read (nothing else in this process reads it), then a distinct
    // header marker per direction proves each emitter produced its OWN
    // line rather than one emitter firing four times.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("ROUTECTL_TRACE_HEADERS", "1") };
    assert!(
        header_trace_enabled(),
        "OnceLock must freeze to true after the env var is set first"
    );
    let ingress_headers = headers_to_json([("x-ingress", b"in".as_slice())]);
    let outgoing_headers = headers_to_json([("x-outgoing", b"out".as_slice())]);
    let upstream_headers = headers_to_json([("x-upstream", b"up".as_slice())]);
    let egress_headers = headers_to_json([("x-egress", b"eg".as_slice())]);

    // Act: drive all four emitters under a TRACE-capturing subscriber.
    let events = capture_events(|| {
        trace_ingress_headers("anthropic", &ingress_headers);
        trace_outgoing_headers("openai-compat", "prov-1", &outgoing_headers);
        trace_upstream_response_headers("openai-compat", "prov-1", &upstream_headers);
        trace_egress_headers("anthropic", &egress_headers);
    });

    // Assert: exactly one event per emitter (no over- or under-emission),
    // each at TRACE level on the log_safe target, carrying the canonical
    // HDR_MSG_* message and a `headers` field with that direction's marker.
    assert_eq!(
        events.len(),
        4,
        "expected one event per emitter; captured: {events:#?}"
    );
    for (message, marker) in [
        (HDR_MSG_INGRESS, "x-ingress"),
        (HDR_MSG_OUTGOING, "x-outgoing"),
        (HDR_MSG_UPSTREAM, "x-upstream"),
        (HDR_MSG_EGRESS, "x-egress"),
    ] {
        let event = events
            .iter()
            .find(|e| e.message == message)
            .unwrap_or_else(|| panic!("no event with message {message:?}; captured: {events:#?}"));
        assert_eq!(
            event.level,
            tracing::Level::TRACE,
            "header trace lines must be TRACE level"
        );
        assert_eq!(
            event.target, "routectl_core::log_safe",
            "header trace lines must originate from the log_safe module"
        );
        let headers = event
            .field("headers")
            .unwrap_or_else(|| panic!("event {message:?} missing `headers` field; got {event:#?}"));
        assert!(
            headers.contains(marker),
            "event {message:?} `headers` field {headers:?} should contain {marker:?}"
        );
    }
}
