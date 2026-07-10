//! End-to-end coverage for the redaction step inside
//! [`trace_upstream_response_headers`] (direction 3: upstream ->
//! routectl). A `set-cookie` session credential and the SigV4
//! `x-amz-security-token` STS credential MUST collapse to `[REDACTED]`
//! BEFORE the `tracing::trace!` line is emitted, while a non-secret
//! response header (a rate-limit metadata value) round-trips verbatim.
//!
//! Why a dedicated integration-test binary: `header_trace_enabled()`
//! reads `ROUTECTL_TRACE_HEADERS` once through a process-frozen
//! `OnceLock`. A separate test binary gets its own process, so setting
//! the env var at the start of the single test below freezes the
//! toggle to `true` here without disturbing any other test. Mirrors
//! tests/header_trace_outgoing_redacts.rs (dir-2).

use routectl_core::{
    HDR_MSG_UPSTREAM, header_trace_enabled, headers_to_json, trace_upstream_response_headers,
};
use routectl_testkit::capture_events;

#[test]
fn upstream_emit_redacts_set_cookie_and_security_token() {
    // Arrange: freeze the toggle to ON before the OnceLock is first read
    // in this process. Build an upstream response header set with a
    // session cookie + an STS security token (both secret) plus a
    // non-secret rate-limit header that must round-trip verbatim.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("ROUTECTL_TRACE_HEADERS", "1") };
    assert!(
        header_trace_enabled(),
        "OnceLock must freeze to true after the env var is set first"
    );

    let cookie = b"session=secret-session-not-real; HttpOnly";
    let sts_token = b"FwoGZXIvYXdzEXAMPLE-not-real";
    let ratelimit = b"42";
    let upstream_headers = headers_to_json([
        ("set-cookie", cookie.as_slice()),
        ("x-amz-security-token", sts_token.as_slice()),
        (
            "anthropic-ratelimit-unified-remaining",
            ratelimit.as_slice(),
        ),
    ]);

    // Act: drive the upstream-response emitter under a TRACE-capturing
    // subscriber.
    let events = capture_events(|| {
        trace_upstream_response_headers("anthropic-api", "prov-1", &upstream_headers);
    });

    // Assert: exactly one TRACE event, message HDR_MSG_UPSTREAM, with a
    // `headers` field whose JSON body has been redacted.
    let event = events
        .iter()
        .find(|e| e.message == HDR_MSG_UPSTREAM)
        .unwrap_or_else(|| panic!("no upstream-headers event captured: {events:#?}"));
    assert_eq!(
        event.level,
        tracing::Level::TRACE,
        "header trace lines must be TRACE level"
    );
    let headers = event
        .field("headers")
        .unwrap_or_else(|| panic!("event missing `headers` field; got {event:#?}"));

    // The session cookie value must NOT appear in the trace.
    assert!(
        !headers.contains("secret-session-not-real"),
        "set-cookie leaked into upstream headers field: {headers}"
    );

    // The STS session token must NOT appear in the trace.
    assert!(
        !headers.contains("FwoGZXIvYXdzEXAMPLE-not-real"),
        "x-amz-security-token leaked into upstream headers field: {headers}"
    );
    assert!(
        headers.contains("[REDACTED]"),
        "expected redacted marker in upstream headers, got: {headers}"
    );

    // A non-secret response header must round-trip verbatim so operator
    // triage of rate-limit / quota still works.
    assert!(
        headers.contains("42"),
        "non-secret upstream header was redacted: {headers}"
    );
}
