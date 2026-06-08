//! Parser for the standard HTTP `Retry-After` response header.
//!
//! Per RFC 9110 the header carries either a delta-seconds integer or an
//! HTTP-date. This module extracts the indicated wait as a
//! [`std::time::Duration`]; the router and circuit breaker consume the
//! parsed value (carried structurally on `Error::Upstream`) to park a
//! provider after a rate-limit / overload response. The shared
//! extraction primitive lives here so each provider's egress can lift
//! the hint with one call.

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Parse the `Retry-After` header into a wait [`Duration`].
///
/// Handles both RFC 9110 forms:
///   - delta-seconds: an integer `N` -> `Duration::from_secs(N)`.
///   - HTTP-date: a date string -> `date - now`, clamped to
///     `Duration::ZERO` when the date is already in the past.
///
/// Returns `None` when the header is absent or neither form parses.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    // delta-seconds form takes precedence: a bare integer is the common
    // shape and is unambiguous against the HTTP-date form.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    parse_http_date_delta(raw)
}

/// Parse an HTTP-date `Retry-After` value into the delay from now,
/// clamped to `Duration::ZERO` for a past date. RFC 9110's preferred
/// IMF-fixdate form (e.g. `Wed, 21 Oct 2026 07:28:00 GMT`) parses via
/// chrono's RFC 2822 reader, which accepts the `GMT` zone token.
fn parse_http_date_delta(raw: &str) -> Option<Duration> {
    let when: DateTime<FixedOffset> = DateTime::parse_from_rfc2822(raw).ok()?;
    (when.with_timezone(&Utc) - Utc::now())
        .to_std()
        .ok()
        .or(Some(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::parse_retry_after;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_str(value).expect("header value"),
        );
        h
    }

    #[test]
    fn parses_integer_delta_seconds() {
        // Arrange
        let headers = headers_with("120");

        // Act
        let got = parse_retry_after(&headers);

        // Assert
        assert_eq!(got, Some(Duration::from_secs(120)));
    }

    #[test]
    fn parses_http_date_form() {
        // Arrange: a date safely in the future.
        let headers = headers_with("Wed, 21 Oct 2099 07:28:00 GMT");

        // Act
        let got = parse_retry_after(&headers).expect("future date must parse");

        // Assert: a positive, non-zero delay.
        assert!(
            got > Duration::ZERO,
            "future HTTP-date must yield a positive delay; got {got:?}"
        );
    }

    #[test]
    fn returns_none_when_absent() {
        // Arrange
        let headers = HeaderMap::new();

        // Act + Assert
        assert!(parse_retry_after(&headers).is_none());
    }

    #[test]
    fn returns_none_on_garbage() {
        // Arrange
        let headers = headers_with("not-a-date-or-int");

        // Act + Assert
        assert!(parse_retry_after(&headers).is_none());
    }

    #[test]
    fn clamps_past_http_date_to_zero() {
        // Arrange: a date well in the past.
        let headers = headers_with("Wed, 21 Oct 2015 07:28:00 GMT");

        // Act
        let got = parse_retry_after(&headers);

        // Assert: a past date clamps to ZERO rather than returning None.
        assert_eq!(got, Some(Duration::ZERO));
    }
}
