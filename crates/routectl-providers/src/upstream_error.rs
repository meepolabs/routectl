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
//!
//! The AWS half (the `x-amzn-errortype` lift and the flat-envelope token
//! fallback) is compiled only with the lanes that can front a Bedrock
//! upstream; a gemini-only build reads its own `/error/status` classifier
//! and passes the tokens in.

use std::time::Duration;

use reqwest::header::HeaderMap;
#[cfg(any(feature = "openai-compat", feature = "openai-responses"))]
use routectl_core::upstream_message_or_excerpt;
use routectl_core::{Error, ParsedErrorBody, UpstreamBody};
use serde_json::Value;

#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
use crate::aws_error::ErrorTypeHeaderLift;

/// The nested `/error/code`, stringified because the wire admits both a
/// string code (`"context_length_exceeded"`) and a numeric one.
fn nested_error_code(v: &Value) -> Option<String> {
    v.pointer("/error/code").and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// Lift the nested first-party classifier (`/error/type`, `/error/code`)
/// from an already-parsed value. Shared by the non-2xx read, inline 2xx
/// error detection, and mid-stream error-frame detection so all three
/// surface the same tokens.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
pub fn lift_nested_error_tokens(v: &Value) -> (Option<String>, Option<String>) {
    let upstream_type = v
        .pointer("/error/type")
        .and_then(Value::as_str)
        .map(str::to_string);
    (upstream_type, nested_error_code(v))
}

/// Lift the Gemini classifier from an already-parsed value. The Gemini
/// envelope is `{"error":{"code":<int>,"message":...,"status":"<UPPER_SNAKE>"}}`:
/// `error.status` (`RESOURCE_EXHAUSTED`, `INVALID_ARGUMENT`, ...) is the
/// string an SDK branches on, so it is the type; the numeric `error.code`
/// is the code.
#[cfg(feature = "gemini")]
pub fn lift_nested_status_tokens(v: &Value) -> (Option<String>, Option<String>) {
    let upstream_type = v
        .pointer("/error/status")
        .and_then(Value::as_str)
        .map(str::to_string);
    (upstream_type, nested_error_code(v))
}

/// Map a non-streaming success body `(bytes, hit_cap)` to the parsed JSON.
///
/// A cap trip on a 2xx means the upstream returned an unreadable success
/// response, an invalid upstream protocol result: it maps to a 502
/// `Error::upstream`, which classifies as a ServerError (debits the
/// breaker, retries/fallbacks) like any other upstream protocol failure.
/// Otherwise the buffered bytes are parsed once.
#[cfg(feature = "anthropic-api")]
pub fn map_success_body(
    provider_id: &str,
    status: u16,
    bytes: &[u8],
    hit_cap: bool,
) -> routectl_core::Result<Value> {
    if hit_cap {
        return Err(Error::upstream(
            provider_id,
            502,
            crate::http_client::body_cap_exceeded_message(),
        ));
    }
    serde_json::from_slice(bytes).map_err(|e| Error::upstream(provider_id, status, e.to_string()))
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
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
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
            #[cfg(any(
                feature = "anthropic-api",
                feature = "bedrock",
                feature = "openai-compat",
                feature = "openai-responses"
            ))]
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

    /// The body's parse, for a lane that builds its message from it.
    #[cfg(feature = "anthropic-api")]
    pub const fn body(&self) -> &UpstreamBody {
        &self.body
    }

    /// The tokens the body alone supplies: a native envelope lifts its
    /// nested type/code; any other body lifts the flat AWS `__type` /
    /// `code`.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    fn body_tokens(&self) -> (Option<String>, Option<String>) {
        let parsed = self.parsed();
        if self.is_native_envelope() {
            parsed.map_or((None, None), |p| lift_nested_error_tokens(p.value()))
        } else {
            crate::aws_error::lift_aws_error_tokens(parsed.map(ParsedErrorBody::value))
        }
    }

    /// The classifier tokens: the body's own (see `body_tokens`), then the
    /// `x-amzn-errortype` header fills the TYPE only when the body supplied
    /// none: it never overrides a body token and never supplies a code.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn tokens(&self) -> (Option<String>, Option<String>) {
        let (body_type, code) = self.body_tokens();
        let upstream_type = body_type.or_else(|| match &self.header_type {
            ErrorTypeHeaderLift::Lifted(token) => Some(token.clone()),
            _ => None,
        });
        (upstream_type, code)
    }

    /// The bounded reason label for an unusable `x-amzn-errortype` header
    /// (`missing` included), reported only when the body supplied no type,
    /// i.e. only when the header was the type's last source. `None` when
    /// the body typed itself or the header lifted a token.
    #[cfg(feature = "bedrock")]
    pub fn unusable_error_type_header_reason(&self) -> Option<&'static str> {
        if self.body_tokens().0.is_some() {
            return None;
        }
        self.header_type.unusable_reason()
    }

    /// Client message for a body without a native envelope, treated as a
    /// potential mantle (AWS) body: a 403 AccessDenied collapses to the IAM
    /// action only; anything else gets the redacted, sanitized excerpt.
    #[cfg(any(feature = "openai-compat", feature = "openai-responses"))]
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
    #[cfg(any(feature = "openai-compat", feature = "openai-responses"))]
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
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn into_error(
        self,
        provider_id: &str,
        carried: CarriedBody,
        retry_after: Option<Duration>,
    ) -> Error {
        let tokens = self.tokens();
        self.into_error_with_tokens(provider_id, carried, retry_after, tokens)
    }

    /// [`Self::into_error`] for a lane whose classifier lives in a
    /// different field than the shared lift reads (Gemini's
    /// `error.status`): the lane passes its own `(type, code)`.
    pub fn into_error_with_tokens(
        self,
        provider_id: &str,
        carried: CarriedBody,
        retry_after: Option<Duration>,
        (upstream_type, upstream_code): (Option<String>, Option<String>),
    ) -> Error {
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
#[path = "upstream_error_tests.rs"]
mod tests;
