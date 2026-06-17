//! End-to-end coverage for the redaction step inside
//! [`trace_outgoing_headers`]: a live access-token JWT in the
//! `authorization` header and a literal api key in `x-api-key` MUST
//! collapse to `Bearer [REDACTED]` / `[REDACTED]` BEFORE the
//! `tracing::trace!` line is emitted, so journald / log archives never
//! carry a replayable token.
//!
//! Why a dedicated integration-test binary: `header_trace_enabled()`
//! reads `ROUTECTL_TRACE_HEADERS` once through a process-frozen
//! `OnceLock`. A separate test binary gets its own process, so setting
//! the env var at the start of the single test below freezes the
//! toggle to `true` here without disturbing any other test.

mod common;

use common::capture_events;
use routectl_core::{
    header_trace_enabled, headers_to_json, trace_outgoing_headers, HDR_MSG_OUTGOING,
};

#[test]
fn outgoing_emit_redacts_authorization_and_x_api_key() {
    // Arrange: freeze the toggle to ON before the OnceLock is first
    // read in this process. Build a header set that includes BOTH a
    // bearer JWT (chatgpt-oauth / codex shape) and an x-api-key
    // (anthropic-api shape) plus a non-secret header that must
    // round-trip verbatim.
    std::env::set_var("ROUTECTL_TRACE_HEADERS", "1");
    assert!(
        header_trace_enabled(),
        "OnceLock must freeze to true after the env var is set first"
    );

    let live_jwt = b"Bearer test-bearer-token-not-real";
    let api_key = b"test-api-key-not-real";
    let beta = b"context-management-2026-05-29";
    let cookie = b"session=secret-session-not-real";
    let outgoing_headers = headers_to_json([
        ("authorization", live_jwt.as_slice()),
        ("x-api-key", api_key.as_slice()),
        ("cookie", cookie.as_slice()),
        ("anthropic-beta", beta.as_slice()),
    ]);

    // Act: drive the outgoing emitter under a TRACE-capturing subscriber.
    let events = capture_events(|| {
        trace_outgoing_headers("anthropic-api", "prov-1", &outgoing_headers);
    });

    // Assert: exactly one TRACE event, message HDR_MSG_OUTGOING, with
    // a `headers` field whose JSON body has been redacted.
    let event = events
        .iter()
        .find(|e| e.message == HDR_MSG_OUTGOING)
        .unwrap_or_else(|| panic!("no outgoing-headers event captured: {events:#?}"));
    assert_eq!(
        event.level,
        tracing::Level::TRACE,
        "header trace lines must be TRACE level"
    );
    let headers = event
        .field("headers")
        .unwrap_or_else(|| panic!("event missing `headers` field; got {event:#?}"));

    // Bearer JWT must NOT appear; the redacted shell must.
    assert!(
        !headers.contains("test-bearer-token-not-real"),
        "bearer token leaked into headers field: {headers}"
    );
    assert!(
        headers.contains("Bearer [REDACTED]"),
        "expected redacted bearer marker, got: {headers}"
    );

    // The literal api-key value must NOT appear; bare [REDACTED] must.
    assert!(
        !headers.contains("test-api-key-not-real"),
        "x-api-key leaked into headers field: {headers}"
    );

    // The session cookie value must NOT appear -- set-cookie / cookie
    // carry session credentials and redact on dir-2 too.
    assert!(
        !headers.contains("secret-session-not-real"),
        "cookie value leaked into headers field: {headers}"
    );

    // Non-secret headers must round-trip verbatim so anthropic-version /
    // anthropic-beta / originator stay observable in the trace.
    assert!(
        headers.contains("context-management-2026-05-29"),
        "non-secret header was redacted: {headers}"
    );
}
