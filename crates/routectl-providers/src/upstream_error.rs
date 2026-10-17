//! Upstream error classification: the one read every provider lane runs on
//! a non-success HTTP response.
//!
//! [`UpstreamErrorRead`] captures, in a single pass, everything a lane's
//! error mapper derives from the response itself: the error body read under
//! the shared response-body cap (with its cap and transport-failure WARNs),
//! the capped parse of that body, the rate-limit-gated `Retry-After` hint,
//! the upstream correlation id, and the `x-amzn-errortype` header lift. The
//! classifier tokens (`upstream_type` / `upstream_code`) are derived here
//! from one precedence rule, so every lane lifts the same tokens from the
//! same wire.
//!
//! What stays at the provider boundary: the per-lane client message, the
//! 403 scrub decision for the carried body, the upstream-failure WARN, and
//! any lane-specific reset hint that outranks the header (the Codex
//! usage-limit body hint).

use std::time::Duration;

use reqwest::header::HeaderMap;
use routectl_core::{Error, ParsedErrorBody, UpstreamBody, upstream_message_or_excerpt};
use serde_json::Value;

use crate::aws_error::ErrorTypeHeaderLift;

/// Lift the nested first-party classifier (`/error/type`, `/error/code`)
/// from an already-parsed value. `code` is stringified because the wire
/// admits both a string code (`"context_length_exceeded"`) and a numeric
/// one. Shared by the non-2xx read, inline 2xx error detection, and
/// mid-stream error-frame detection so all three surface the same tokens.
pub fn lift_nested_error_tokens(v: &Value) -> (Option<String>, Option<String>) {
    let upstream_type = v
        .pointer("/error/type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let upstream_code = v.pointer("/error/code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    (upstream_type, upstream_code)
}

/// Which text the built error carries as its body.
pub enum CarriedBody {
    /// The upstream body as read, with its parse.
    Original,
    /// A lane-built replacement (a scrubbed message, the fixed cap message).
    Replacement(String),
}

/// A non-success upstream response, read once.
pub struct UpstreamErrorRead {
    status: u16,
    body: UpstreamBody,
    hit_cap: bool,
    retry_after: Option<Duration>,
    request_id: Option<String>,
    header_type: ErrorTypeHeaderLift,
}

impl UpstreamErrorRead {
    /// Read `resp`'s error body under the shared response-body cap. Headers
    /// are lifted before the body read consumes `resp`. A cap trip emits one
    /// WARN (`path="error_body"`); a transport failure while reading is
    /// logged and degrades to an empty body.
    pub async fn read(provider_id: &str, resp: reqwest::Response) -> Self {
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let content_length = resp.content_length();
        let (bytes, hit_cap) = match crate::http_client::read_body_capped(
            resp,
            crate::http_client::MAX_RESPONSE_BODY_BYTES,
        )
        .await
        {
            Ok(read) => read,
            Err(e) => {
                tracing::warn!(
                    provider = %provider_id,
                    status,
                    error = %e,
                    "failed to read upstream error body",
                );
                (Vec::new(), false)
            }
        };
        if hit_cap {
            crate::http_client::warn_body_cap(provider_id, status, content_length, "error_body");
        }
        Self::from_parts(
            status,
            &headers,
            String::from_utf8_lossy(&bytes).into_owned(),
            hit_cap,
        )
    }

    /// Build the read from a body already in hand (`hit_cap` true when it
    /// was truncated at the response-body cap). The body is parsed here,
    /// once; every later consumer reads that parse.
    pub fn from_parts(
        status: u16,
        headers: &HeaderMap,
        body_text: impl Into<String>,
        hit_cap: bool,
    ) -> Self {
        // Gated on rate-limit statuses so a stray Retry-After on a 400 does
        // not park the provider.
        let retry_after = if crate::retry_after::is_rate_limit_status(status) {
            crate::retry_after::parse_retry_after(headers)
        } else {
            None
        };
        Self {
            status,
            body: UpstreamBody::new(body_text),
            hit_cap,
            retry_after,
            request_id: crate::upstream_request_id::parse_upstream_request_id(headers),
            header_type: crate::aws_error::classify_aws_error_type_header(headers),
        }
    }

    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn text(&self) -> &str {
        self.body.text()
    }

    pub const fn parsed(&self) -> Option<&ParsedErrorBody> {
        self.body.parsed()
    }

    pub const fn hit_cap(&self) -> bool {
        self.hit_cap
    }

    /// The header reset hint, `None` unless the status is a rate limit.
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// True when the body is the first-party nested `{"error":{...}}`
    /// envelope; anything else is treated as a potential AWS (mantle) body.
    pub fn is_native_envelope(&self) -> bool {
        self.parsed()
            .is_some_and(ParsedErrorBody::is_error_envelope)
    }

    /// The classifier tokens. A native envelope lifts its nested
    /// type/code; any other body lifts the flat AWS `__type` / `code`. The
    /// `x-amzn-errortype` header then fills the TYPE only when the body
    /// supplied none: it never overrides a body token and never supplies a
    /// code.
    pub fn tokens(&self) -> (Option<String>, Option<String>) {
        let parsed = self.parsed();
        let (body_type, code) = if self.is_native_envelope() {
            parsed.map_or((None, None), |p| lift_nested_error_tokens(p.value()))
        } else {
            crate::aws_error::lift_aws_error_tokens(parsed.map(ParsedErrorBody::value))
        };
        let upstream_type = body_type.or_else(|| match &self.header_type {
            ErrorTypeHeaderLift::Lifted(token) => Some(token.clone()),
            _ => None,
        });
        (upstream_type, code)
    }

    /// Client message for a body without a native envelope, treated as a
    /// potential mantle (AWS) body: a 403 AccessDenied collapses to the IAM
    /// action only; anything else gets the redacted, sanitized excerpt.
    pub fn non_envelope_client_message(&self) -> String {
        let text = self.text();
        match crate::aws_error::classify_bedrock_error(self.status, text) {
            crate::aws_error::BedrockErrorClass::AccessDenied { .. } => {
                crate::aws_error::classify_client_error_message(self.status, text)
            }
            crate::aws_error::BedrockErrorClass::Other => {
                upstream_message_or_excerpt(text, self.parsed())
            }
        }
    }

    /// The upstream body for the DEBUG log: a non-envelope 403 collapses to
    /// the IAM action so no ARN or account id reaches the log.
    pub fn debug_body(&self) -> String {
        if self.is_native_envelope() {
            self.text().to_string()
        } else {
            crate::aws_error::sanitized_debug_body(self.status, self.text())
        }
    }

    /// Build the canonical `Error::Upstream`, carrying the lifted tokens
    /// and the correlation id. `retry_after` is passed in so a lane hint
    /// that outranks the header can win; pass [`Self::retry_after`]
    /// otherwise.
    pub fn into_error(
        self,
        provider_id: &str,
        carried: CarriedBody,
        retry_after: Option<Duration>,
    ) -> Error {
        let (upstream_type, upstream_code) = self.tokens();
        let body = match carried {
            CarriedBody::Original => self.body,
            CarriedBody::Replacement(text) => UpstreamBody::new(text),
        };
        Error::upstream_with_body(
            provider_id,
            self.status,
            body,
            retry_after,
            upstream_type,
            upstream_code,
        )
        .with_upstream_request_id(self.request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{UpstreamErrorRead, lift_nested_error_tokens};
    use reqwest::header::HeaderMap;
    use serde_json::json;

    const HEADER: &str = "x-amzn-errortype";

    type TokenRow<'a> = (
        &'a str,
        &'a HeaderMap,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    );

    fn headers_with_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, value.parse().unwrap());
        headers
    }

    fn tokens(headers: &HeaderMap, body: &str) -> (Option<String>, Option<String>) {
        UpstreamErrorRead::from_parts(400, headers, body, false).tokens()
    }

    #[test]
    fn nested_lift_stringifies_numeric_code() {
        let v = json!({"error": {"type": "invalid_request_error", "code": 400}});

        let got = lift_nested_error_tokens(&v);

        assert_eq!(
            got,
            (Some("invalid_request_error".into()), Some("400".into()))
        );
    }

    #[test]
    fn token_precedence_rows() {
        let header = headers_with_type("ValidationException:http://internal.amazon.com/coral/");
        let no_header = HeaderMap::new();
        let cases: [TokenRow<'_>; 5] = [
            (
                "native envelope lifts nested type and code",
                &no_header,
                r#"{"error":{"type":"invalid_request_error","code":"bad"}}"#,
                Some("invalid_request_error"),
                Some("bad"),
            ),
            (
                "flat body lifts namespaced __type and code",
                &no_header,
                r#"{"__type":"com.amazonaws.bedrock#ThrottlingException","code":"T"}"#,
                Some("ThrottlingException"),
                Some("T"),
            ),
            (
                "header fills type when the flat body has none",
                &header,
                r#"{"message":"bad"}"#,
                Some("ValidationException"),
                None,
            ),
            (
                "body __type wins over the header",
                &header,
                r#"{"__type":"AccessDeniedException","message":"no"}"#,
                Some("AccessDeniedException"),
                None,
            ),
            (
                "header fills type on a native envelope without one",
                &header,
                r#"{"error":{"code":"bad"}}"#,
                Some("ValidationException"),
                Some("bad"),
            ),
        ];
        for (name, headers, body, want_type, want_code) in cases {
            let (got_type, got_code) = tokens(headers, body);

            assert_eq!(got_type.as_deref(), want_type, "{name}");
            assert_eq!(got_code.as_deref(), want_code, "{name}");
        }
    }

    #[test]
    fn unusable_header_lifts_no_type() {
        let mut headers = HeaderMap::new();
        headers.append(HEADER, "ValidationException".parse().unwrap());
        headers.append(HEADER, "ThrottlingException".parse().unwrap());

        let (got_type, _) = tokens(&headers, r#"{"message":"bad"}"#);

        assert_eq!(got_type, None);
    }

    #[test]
    fn retry_after_is_gated_on_rate_limit_status() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        let cases = [(429, true), (503, true), (400, false), (500, false)];
        for (status, want) in cases {
            let read = UpstreamErrorRead::from_parts(status, &headers, "", false);

            assert_eq!(read.retry_after().is_some(), want, "status {status}");
        }
    }
}
