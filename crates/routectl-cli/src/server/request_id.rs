//! Request ID + tracing span middleware.
//!
//! For every incoming HTTP request we read `x-request-id` if the
//! caller supplied one (idempotency / cross-system correlation), or
//! mint a fresh `Uuid::now_v7()` (sortable by time, so log readers
//! scanning chronologically see IDs in roughly the same order events
//! happened). The id is then:
//!
//!   1. Set as the `request_id` field on a per-request `info_span!`,
//!      so every log emitted while processing this request inherits
//!      it via tracing's parent-child propagation. Operators can grep
//!      `request_id=<id>` to follow one request across fallback hops,
//!      retries, and provider calls.
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
    http::{header::HeaderName, HeaderValue},
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

pub async fn middleware(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_request_id(s))
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    let method = req.method().clone();
    // The URI path component is RFC-3986-encoded so newlines and ANSI
    // escapes would already be percent-encoded by a conformant client,
    // but a non-conformant tool could smuggle raw bytes via a
    // hand-rolled HTTP request. `sanitize_for_log` closes that gap
    // matching the treatment of every other client-controlled string
    // that flows into a tracing field.
    let path = sanitize_for_log(req.uri().path());

    let span = tracing::info_span!(
        "request",
        method = %method,
        path = %path,
        request_id = %request_id,
    );

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
    use super::is_safe_request_id;

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
