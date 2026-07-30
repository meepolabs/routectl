//! Extraction of an upstream provider correlation id from response headers.
//!
//! Vendors stamp a request/trace id on their HTTP responses that vendor
//! support uses to correlate a specific failed request. This module lifts
//! the FIRST present of the common id headers so each provider's error
//! mapper can attach it onto `Error::Upstream` with one call, at the same
//! seam it already parses `Retry-After` (see [`crate::retry_after`]). The
//! ingress then surfaces it on a client-facing header so a caller can quote
//! it back to the vendor -- matching what codex and opencode attach.

use reqwest::header::HeaderMap;

/// Correlation-id header names in precedence order, most specific first:
///   - `x-request-id`: OpenAI / Anthropic (and many gateways) request id.
///   - `x-oai-request-id`: the older OpenAI-specific spelling.
///   - `cf-ray`: Cloudflare edge ray id (present when a request transited
///     Cloudflare but the origin stamped no request id of its own).
///
/// The first header present with a non-empty, valid-UTF-8 value wins.
const CORRELATION_HEADERS: [&str; 3] = ["x-request-id", "x-oai-request-id", "cf-ray"];

/// Lift the upstream provider's correlation id from the response headers,
/// returning the first present of [`CORRELATION_HEADERS`]. Returns `None`
/// when none are present, or when the present value is empty or not valid
/// UTF-8 (a non-UTF-8 correlation id is not something a caller can quote to
/// vendor support, so it is dropped rather than lossily rendered).
pub fn parse_upstream_request_id(headers: &HeaderMap) -> Option<String> {
    for name in CORRELATION_HEADERS {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_upstream_request_id;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("header name");
            map.insert(name, HeaderValue::from_str(value).expect("header value"));
        }
        map
    }

    #[test]
    fn lifts_x_request_id_when_present() {
        // Arrange
        let headers = headers_with(&[("x-request-id", "req-abc")]);

        // Act
        let got = parse_upstream_request_id(&headers);

        // Assert
        assert_eq!(got.as_deref(), Some("req-abc"));
    }

    #[test]
    fn prefers_x_request_id_over_later_headers() {
        // Arrange: all three present; the most specific wins.
        let headers = headers_with(&[
            ("x-request-id", "primary"),
            ("x-oai-request-id", "oai"),
            ("cf-ray", "ray"),
        ]);

        // Act
        let got = parse_upstream_request_id(&headers);

        // Assert
        assert_eq!(got.as_deref(), Some("primary"));
    }

    #[test]
    fn falls_back_to_x_oai_request_id() {
        // Arrange: no x-request-id; the OpenAI-specific spelling is next.
        let headers = headers_with(&[("x-oai-request-id", "oai-42"), ("cf-ray", "ray")]);

        // Act
        let got = parse_upstream_request_id(&headers);

        // Assert
        assert_eq!(got.as_deref(), Some("oai-42"));
    }

    #[test]
    fn falls_back_to_cf_ray_last() {
        // Arrange: only the Cloudflare edge ray is present.
        let headers = headers_with(&[("cf-ray", "8a1b2c3d-EWR")]);

        // Act
        let got = parse_upstream_request_id(&headers);

        // Assert
        assert_eq!(got.as_deref(), Some("8a1b2c3d-EWR"));
    }

    #[test]
    fn returns_none_when_absent() {
        // Arrange + Act + Assert
        assert!(parse_upstream_request_id(&HeaderMap::new()).is_none());
    }

    #[test]
    fn skips_empty_value_and_falls_through() {
        // Arrange: an empty x-request-id must not win over a real cf-ray.
        let headers = headers_with(&[("x-request-id", "   "), ("cf-ray", "ray-9")]);

        // Act
        let got = parse_upstream_request_id(&headers);

        // Assert
        assert_eq!(got.as_deref(), Some("ray-9"));
    }
}
