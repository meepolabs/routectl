//! Lazily-gated header-trace helpers shared by every egress provider.
//!
//! Each provider's `complete()` and `stream()` paths emit dir-2
//! (routectl -> upstream) and dir-3 (upstream -> routectl) header
//! traces. These two helpers centralize the gate plus the
//! `reqwest::HeaderMap -> core JSON` conversion so:
//!
//!   1. ZERO header JSON is built when `ROUTECTL_TRACE_HEADERS` is off
//!      (or the subscriber is above TRACE). The gate is checked FIRST,
//!      before [`routectl_core::headers_to_json`] runs, so the default
//!      path pays nothing.
//!   2. The provider dir-2 / dir-3 call sites collapse to one line
//!      each, so the `headers.iter().map(...)` adapter that bridges
//!      `reqwest::header::HeaderMap` to the `&str` / `&[u8]` pairs
//!      `routectl_core` consumes lives in exactly one place.
//!
//! The core `trace_*_headers` fns keep their own internal gate as
//! belt-and-suspenders; this layer is what makes the JSON build lazy.

use reqwest::header::HeaderMap;

/// True only when a header trace should be BUILT here: the operator
/// opted in via `ROUTECTL_TRACE_HEADERS`. Cheap env-toggle check whose
/// sole job is to skip the `headers_to_json` allocation on the default
/// (toggle-off) path.
///
/// The TRACE-LEVEL gate is intentionally NOT checked here.
/// `tracing::event_enabled!` resolves against the CALLER's module target
/// -- this crate runs at `info` under the usual
/// `routectl=info,routectl_core::log_safe=trace` filter, so a level
/// check here would always be false and silently suppress every header
/// trace. The core `trace_*_headers` emitters re-check the level against
/// their own `routectl_core::log_safe` target, which is where TRACE is
/// actually enabled.
fn should_trace_headers() -> bool {
    routectl_core::header_trace_enabled()
}

/// Emit dir-2 (routectl -> upstream) outgoing request headers --
/// INCLUDING auth -- for the given provider. No-op, and no allocation,
/// unless header tracing is on. Call AFTER the request is built so the
/// resolved auth header is present on the `HeaderMap`.
pub fn outgoing(provider_kind: &str, id: &str, headers: &HeaderMap) {
    if !should_trace_headers() {
        return;
    }
    routectl_core::trace_outgoing_headers(
        provider_kind,
        id,
        &routectl_core::headers_to_json(headers.iter().map(|(k, v)| (k.as_str(), v.as_bytes()))),
    );
}

/// Emit dir-3 (upstream -> routectl) response headers for the given
/// provider. No-op, and no allocation, unless header tracing is on.
/// Call BEFORE the response body is consumed -- `resp.json()` /
/// `resp.bytes_stream()` take ownership, after which `resp.headers()`
/// is gone.
pub fn upstream(provider_kind: &str, id: &str, headers: &HeaderMap) {
    if !should_trace_headers() {
        return;
    }
    routectl_core::trace_upstream_response_headers(
        provider_kind,
        id,
        &routectl_core::headers_to_json(headers.iter().map(|(k, v)| (k.as_str(), v.as_bytes()))),
    );
}
