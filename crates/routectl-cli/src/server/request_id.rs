//! Request ID + tracing span middleware.
//!
//! For every incoming HTTP request we read `x-request-id` if the
//! caller supplied one (idempotency / cross-system correlation), or
//! mint a fresh `Uuid::now_v7()` (sortable by time, so log readers
//! scanning chronologically see IDs in roughly the same order events
//! happened). The id is then:
//!
//!   1. Set as the `request_id` field on a per-request span, so every
//!      log emitted while processing this request inherits it via
//!      tracing's parent-child propagation. Operators can grep
//!      `request_id=<id>` to follow one request across fallback hops,
//!      retries, and provider calls. The span is INFO, except for the
//!      read-only polling paths (see `POLLING_PATHS`), whose span is
//!      DEBUG so the span-close access line stays out of the default
//!      INFO log; events inside such a request then carry no
//!      `request_id` span field at INFO (rejection WARNs add it
//!      explicitly).
//!   2. Stashed on `req.extensions` as a `RequestId` so handlers /
//!      provider impls that need to thread it into upstream-bound
//!      headers can pull it back out.
//!   3. Echoed in the response `x-request-id` header so the client
//!      can correlate its logs with ours.
//!
//! Replaces `tower_http::trace::TraceLayer` -- the span we create
//! here serves the same purpose with our chosen field shape.

use axum::{
    extract::Request,
    http::{HeaderValue, header::HeaderName},
    middleware::Next,
    response::Response,
};
use routectl_core::sanitize_for_log;
use tracing::Instrument;
use uuid::Uuid;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Sanity cap on caller-supplied `x-request-id` length. Anything beyond
/// this is replaced with a generated id, partly to keep log lines
/// scannable and partly to defang abuse (a 1MB request id would bloat
/// every log line and tracing event).
const MAX_REQUEST_ID_LEN: usize = 128;

/// Stashed on `req.extensions` so downstream code can echo the id into
/// upstream-bound headers without re-reading it from the HTTP headers.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Returns true if every byte is in the allowlist for request-id
/// characters: ASCII alnum, plus `-`, `_`, `.`, `:`. This rules out
/// newlines, CR, ANSI escape `\x1b`, whitespace, and any other byte
/// that could let a malicious caller forge a fake log line by setting
/// `x-request-id: <chars>\nFAKE_LOG entry`. The allowed set covers
/// every common request-id flavor we care about: UUIDv4/v7, ULID,
/// snowflake, OpenTelemetry trace-ids, and nested `req-1:retry-2`
/// shapes.
fn is_safe_request_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REQUEST_ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b':')
}

/// The read-only polling routes, matched exactly: an undeclared path under
/// `/status/` is a 404 probe, not a poll, and keeps its INFO access line.
/// `polling_paths_match_the_declared_status_routes` welds this list to the
/// router's declared route inventory.
const POLLING_PATHS: &[&str] = &[
    "/health",
    "/status",
    "/status/usage",
    "/status/health",
    "/status/config",
    "/status/doctor",
    "/status/query",
];

fn is_polling_path(path: &str) -> bool {
    POLLING_PATHS.contains(&path)
}

pub async fn middleware(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_request_id(s))
        .map_or_else(
            || Uuid::now_v7().to_string(),
            std::string::ToString::to_string,
        );

    let method = req.method().clone();
    // The URI path component is RFC-3986-encoded so newlines and ANSI
    // escapes would already be percent-encoded by a conformant client,
    // but a non-conformant tool could smuggle raw bytes via a
    // hand-rolled HTTP request. `sanitize_for_log` closes that gap
    // matching the treatment of every other client-controlled string
    // that flows into a tracing field.
    let path = sanitize_for_log(req.uri().path());

    // Tracing callsites are static, so each level needs its own macro site.
    let span = if is_polling_path(req.uri().path()) {
        tracing::debug_span!(
            "request",
            method = %method,
            path = %path,
            request_id = %request_id,
        )
    } else {
        tracing::info_span!(
            "request",
            method = %method,
            path = %path,
            request_id = %request_id,
        )
    };

    req.extensions_mut().insert(RequestId(request_id.clone()));

    // Wrap the entire async body in the span (not just `next.run`) so
    // any log emitted after `.await` -- including the response-header
    // insertion below and any future log we might add here -- still
    // inherits `request_id`. Future-proofs against silent context loss
    // if a maintainer adds tracing in the post-response section.
    async move {
        let mut response = next.run(req).await;
        if let Ok(hv) = HeaderValue::from_str(&request_id) {
            response.headers_mut().insert(X_REQUEST_ID, hv);
        }
        response
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::{POLLING_PATHS, is_polling_path, is_safe_request_id};
    use crate::server::serve::{AUTH_GATED_ROUTES, PUBLIC_ROUTES};

    #[test]
    fn classifies_read_only_polling_paths() {
        let cases = [
            ("/status", true),
            ("/status/doctor", true),
            ("/health", true),
            ("/status/nonexistent", false),
            ("/status/../x", false),
            ("/status/", false),
            ("/statusx", false),
            ("/v1/messages", false),
            ("/", false),
        ];
        for (path, expected) in cases {
            assert_eq!(is_polling_path(path), expected, "path {path:?}");
        }
    }

    /// The polling list is exactly `/health` plus every declared `/status*`
    /// route, so a status route added to the router without being named here
    /// (or a stale entry left behind) fails.
    #[test]
    fn polling_paths_match_the_declared_status_routes() {
        let mut declared: Vec<&str> = PUBLIC_ROUTES
            .iter()
            .chain(AUTH_GATED_ROUTES.iter())
            .copied()
            .filter(|p| *p == "/health" || *p == "/status" || p.starts_with("/status/"))
            .collect();
        declared.sort_unstable();
        let mut polling = POLLING_PATHS.to_vec();
        polling.sort_unstable();

        assert_eq!(polling, declared);
    }

    #[test]
    fn accepts_uuid_v7_shape() {
        assert!(is_safe_request_id("019e0908-c6d1-7b51-b140-af977721affc"));
    }

    #[test]
    fn accepts_alnum_plus_allowed_punct() {
        assert!(is_safe_request_id("req-abc.123:retry_2"));
    }

    #[test]
    fn rejects_newlines_and_cr() {
        assert!(!is_safe_request_id("abc\ninjection"));
        assert!(!is_safe_request_id("abc\rinjection"));
    }

    #[test]
    fn rejects_ansi_escape() {
        assert!(!is_safe_request_id("abc\x1b[31mred"));
    }

    #[test]
    fn rejects_spaces_and_tabs() {
        assert!(!is_safe_request_id("foo bar"));
        assert!(!is_safe_request_id("foo\tbar"));
    }

    #[test]
    fn rejects_empty_or_oversize() {
        assert!(!is_safe_request_id(""));
        assert!(!is_safe_request_id(&"a".repeat(129)));
        assert!(is_safe_request_id(&"a".repeat(128)));
    }
}
