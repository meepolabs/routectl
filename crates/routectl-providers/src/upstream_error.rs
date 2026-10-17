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
use routectl_core::{Error, ParsedErrorBody, UpstreamBody, UpstreamReason};
use serde_json::Value;

#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
use crate::aws_error::{AccessDeniedDetail, ErrorTypeHeaderLift};

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

/// The error shape a credential row is recorded in: which status, which
/// classifier tokens, and where the message sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CredentialVocabulary {
    /// Bedrock runtime: 403, AWS type `AccessDeniedException` (body
    /// `__type` or the `x-amzn-errortype` header), no code, the message at
    /// the flat body's top-level `Message` / `message`.
    BedrockNative,
    /// Mantle OpenAI Chat Completions and Responses: 401, code
    /// `invalid_api_key`, type `permission_denied_error` or absent, the
    /// message at `error.message`.
    MantleOpenAi,
    /// Mantle Anthropic Messages: 401, type `authentication_error`, no
    /// code, the message at `error.message`.
    MantleAnthropic,
}

/// How a row's phrase must match the message.
#[derive(Clone, Copy)]
enum Phrase {
    Exact(&'static str),
    /// The fixed head of a message whose tail varies per response.
    Prefix(&'static str),
}

/// One recognized credential rejection and the committed capture that
/// records it: a fixture under `tests/fixtures/` and its response key.
struct ReasonRow {
    vocabulary: CredentialVocabulary,
    phrase: Phrase,
    reason: UpstreamReason,
    #[cfg_attr(
        not(all(
            test,
            any(
                feature = "anthropic-api",
                feature = "bedrock",
                feature = "openai-compat",
                feature = "openai-responses"
            )
        )),
        allow(dead_code)
    )]
    evidence: (&'static str, &'static str),
}

const NATIVE_CAPTURE: &str = "bedrock_native_credential_capture.json";
const MANTLE_CAPTURE: &str = "bedrock_mantle_credential_capture.json";

/// The closed table. A row is exact evidence: its vocabulary's status and
/// tokens AND its phrase. "The security token included in the request is
/// invalid" is deliberately absent: Bedrock also answers it for a region
/// the account has not opted into.
const REASON_TABLE: [ReasonRow; 8] = {
    use CredentialVocabulary::{BedrockNative, MantleAnthropic, MantleOpenAi};
    use UpstreamReason::{CredentialExpired, CredentialInvalid};
    const SECURITY_TOKEN_EXPIRED: Phrase =
        Phrase::Exact("The security token included in the request is expired");
    // The tail carries the key's signing time and the server clock.
    const SIGNATURE_EXPIRED: Phrase = Phrase::Prefix("Signature expired: ");
    const INVALID_BEARER: Phrase = Phrase::Exact("Invalid bearer token");
    [
        ReasonRow {
            vocabulary: BedrockNative,
            phrase: Phrase::Exact("Bearer Token has expired"),
            reason: CredentialExpired,
            evidence: (NATIVE_CAPTURE, "expired-invoke"),
        },
        ReasonRow {
            vocabulary: BedrockNative,
            phrase: Phrase::Exact("Authentication failed: Please make sure your API Key is valid."),
            reason: CredentialInvalid,
            evidence: (NATIVE_CAPTURE, "invalid-invoke"),
        },
        ReasonRow {
            vocabulary: MantleOpenAi,
            phrase: SECURITY_TOKEN_EXPIRED,
            reason: CredentialExpired,
            evidence: (MANTLE_CAPTURE, "mantle-chat-expired-security-token"),
        },
        ReasonRow {
            vocabulary: MantleOpenAi,
            phrase: SIGNATURE_EXPIRED,
            reason: CredentialExpired,
            evidence: (MANTLE_CAPTURE, "mantle-chat-expired"),
        },
        ReasonRow {
            vocabulary: MantleOpenAi,
            phrase: INVALID_BEARER,
            reason: CredentialInvalid,
            evidence: (MANTLE_CAPTURE, "mantle-chat-invalid"),
        },
        ReasonRow {
            vocabulary: MantleAnthropic,
            phrase: SECURITY_TOKEN_EXPIRED,
            reason: CredentialExpired,
            evidence: (MANTLE_CAPTURE, "mantle-anthropic-expired-security-token"),
        },
        ReasonRow {
            vocabulary: MantleAnthropic,
            phrase: SIGNATURE_EXPIRED,
            reason: CredentialExpired,
            evidence: (MANTLE_CAPTURE, "mantle-anthropic-expired"),
        },
        ReasonRow {
            vocabulary: MantleAnthropic,
            phrase: INVALID_BEARER,
            reason: CredentialInvalid,
            evidence: (MANTLE_CAPTURE, "mantle-anthropic-invalid"),
        },
    ]
};

/// The reason an upstream rejection carries exact evidence of: the one
/// reason every matching [`REASON_TABLE`] row agrees on. No row, rows that
/// disagree, or a message the vocabulary cannot read unambiguously is
/// `None`, never a guess.
pub fn recognize_upstream_reason(
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
    parsed: Option<&ParsedErrorBody>,
) -> Option<UpstreamReason> {
    let parsed = parsed?;
    let mut matched = REASON_TABLE.iter().filter(|row| {
        vocabulary_matches(row.vocabulary, status, upstream_type, upstream_code)
            && vocabulary_message(row.vocabulary, parsed)
                .is_some_and(|message| phrase_matches(row.phrase, message))
    });
    let reason = matched.next()?.reason;
    matched.all(|row| row.reason == reason).then_some(reason)
}

fn vocabulary_matches(
    vocabulary: CredentialVocabulary,
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
) -> bool {
    match vocabulary {
        CredentialVocabulary::BedrockNative => {
            status == 403
                && upstream_type == Some("AccessDeniedException")
                && upstream_code.is_none()
        }
        CredentialVocabulary::MantleOpenAi => {
            status == 401
                && upstream_code == Some("invalid_api_key")
                && matches!(upstream_type, None | Some("permission_denied_error"))
        }
        CredentialVocabulary::MantleAnthropic => {
            status == 401
                && upstream_type == Some("authentication_error")
                && upstream_code.is_none()
        }
    }
}

/// The message where `vocabulary` records it. The flat AWS body is read at
/// its top-level `Message` / `message`; both present and different is
/// ambiguous.
fn vocabulary_message(vocabulary: CredentialVocabulary, parsed: &ParsedErrorBody) -> Option<&str> {
    match vocabulary {
        CredentialVocabulary::BedrockNative => {
            if parsed.is_error_envelope() {
                return None;
            }
            let value = parsed.value();
            let upper = value.get("Message").and_then(Value::as_str);
            let lower = value.get("message").and_then(Value::as_str);
            match (upper, lower) {
                (Some(a), Some(b)) if a != b => None,
                (a, b) => a.or(b),
            }
        }
        CredentialVocabulary::MantleOpenAi | CredentialVocabulary::MantleAnthropic => {
            parsed.error_message()
        }
    }
}

fn phrase_matches(phrase: Phrase, message: &str) -> bool {
    match phrase {
        Phrase::Exact(text) => message == text,
        Phrase::Prefix(head) => message.starts_with(head) && message.len() > head.len(),
    }
}

/// True when a string anywhere in `value` (object values and array items,
/// recursively; keys are not read) names an AWS principal or account. The
/// recursion is bounded by serde_json's default 128-level parse depth limit.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
fn any_string_leaf_names_principal(value: &Value) -> bool {
    match value {
        Value::String(s) => crate::aws_error::names_aws_principal(s),
        Value::Array(items) => items.iter().any(any_string_leaf_names_principal),
        Value::Object(map) => map.values().any(any_string_leaf_names_principal),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Which text the built error carries as its body.
pub enum CarriedBody {
    /// The upstream body as read, with its parse.
    Original,
    /// A lane-built replacement (a scrubbed message, the fixed cap message).
    Replacement(String),
    /// A built body that already holds its parse.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    Built(UpstreamBody),
}

/// A non-success upstream response, read once.
pub struct UpstreamErrorRead {
    status: u16,
    body: UpstreamBody,
    hit_cap: bool,
    retry_after: Option<Duration>,
    request_id: Option<String>,
    aws_backed: bool,
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
            aws_backed: false,
            #[cfg(any(
                feature = "anthropic-api",
                feature = "bedrock",
                feature = "openai-compat",
                feature = "openai-responses"
            ))]
            header_type: crate::aws_error::classify_aws_error_type_header(headers),
        }
    }

    /// Mark whether the seat that answered is AWS-backed (a Bedrock
    /// provider, or a lane configured for the Bedrock Mantle endpoint).
    /// Credential reasons are recognized only on such a seat: the recorded
    /// rows are AWS responses, and the same text from any other upstream is
    /// not evidence of them.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    #[must_use]
    pub const fn aws_backed(mut self, aws_backed: bool) -> Self {
        self.aws_backed = aws_backed;
        self
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
    /// none: it never overrides a body token and never supplies a code. On
    /// a 403 needing the access-denied scrub each token must also be
    /// client-safe ([`crate::aws_error::is_client_safe_token`]), since the
    /// nested envelope's tokens are not bounded at their lift.
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
        if self.needs_access_denied_scrub() {
            let safe =
                |token: Option<String>| token.filter(|t| crate::aws_error::is_client_safe_token(t));
            return (safe(upstream_type), safe(code));
        }
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

    /// The typed reason this rejection carries exact evidence of, from the
    /// closed table ([`recognize_upstream_reason`]) over `tokens`, the
    /// status, and the read's own parse. A body truncated at the cap, or a
    /// rejection from a seat that is not AWS-backed ([`Self::aws_backed`]),
    /// is never evidence.
    fn reason_for(
        &self,
        (upstream_type, upstream_code): (Option<&str>, Option<&str>),
    ) -> Option<UpstreamReason> {
        if self.hit_cap || !self.aws_backed {
            return None;
        }
        recognize_upstream_reason(self.status, upstream_type, upstream_code, self.parsed())
    }

    /// The AWS type and credential reason an access-denied rendering names,
    /// both derived from this read: the type is the lifted classifier token
    /// ([`Self::tokens`]), the reason the table's verdict over it.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn access_denied_detail(&self) -> AccessDeniedDetail {
        let (upstream_type, upstream_code) = self.tokens();
        let reason = self.reason_for((upstream_type.as_deref(), upstream_code.as_deref()));
        AccessDeniedDetail {
            aws_type: upstream_type,
            reason,
        }
    }

    /// True when this read must go through the access-denied scrub rather
    /// than be carried or logged as read: a 403 whose body is not a native
    /// `{"error":{...}}` envelope (a potential AWS body), every 403 on an
    /// AWS-backed seat ([`Self::aws_backed`]), and, as defense in depth on any
    /// other seat, a 403 whose envelope names an AWS principal or account.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn needs_access_denied_scrub(&self) -> bool {
        self.status == 403
            && (self.aws_backed || !self.is_native_envelope() || self.envelope_names_principal())
    }

    /// True when any decoded string leaf of the parsed body (the `error`
    /// object's fields, a string-valued `error`, and any sibling) names an
    /// AWS principal or account. The raw text is not checked: a JSON escape
    /// can spell any marker.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    fn envelope_names_principal(&self) -> bool {
        self.parsed()
            .is_some_and(|parsed| any_string_leaf_names_principal(parsed.value()))
    }

    /// The client message for a 403 that needs the access-denied scrub,
    /// rendered from routectl's own text only
    /// ([`crate::aws_error::access_denied_message`] over this read's detail
    /// and the shape-checked IAM action), never from the upstream message.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn access_denied_client_message(&self) -> String {
        crate::aws_error::sanitized_debug_body(403, self.text(), &self.access_denied_detail())
    }

    /// The carried body for a client message built from a body without a
    /// native envelope. A 403 from an AWS-backed seat ([`Self::aws_backed`])
    /// or an AWS-shaped one ([`AccessDeniedDetail::is_aws_shaped`]) is
    /// carried as a built
    /// `{"error":{"message":...}}` envelope: the ingress surfaces only a
    /// parsed `error.message`, so plain text would reach the client as the
    /// bare status. Any other 403 keeps the plain text, so a non-AWS
    /// upstream's client never reads "bedrock". The envelope's message is
    /// always [`Self::access_denied_client_message`], whatever the lane
    /// passed. Any other message is carried as the body as read when it is
    /// that text, else as the message.
    #[cfg(any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    ))]
    pub fn aws_carried(&self, client_message: String) -> CarriedBody {
        if self.status == 403
            && (self.aws_backed || self.access_denied_detail().is_aws_shaped(self.text()))
        {
            CarriedBody::Built(UpstreamBody::error_envelope(
                &self.access_denied_client_message(),
            ))
        } else if client_message == self.text() {
            CarriedBody::Original
        } else {
            CarriedBody::Replacement(client_message)
        }
    }

    /// Client message for a body without a native envelope, treated as a
    /// potential mantle (AWS) body: a 403 AccessDenied collapses to the IAM
    /// action only; anything else gets the redacted, sanitized excerpt.
    #[cfg(any(feature = "openai-compat", feature = "openai-responses"))]
    pub fn non_envelope_client_message(&self) -> String {
        if self.status == 403 {
            self.access_denied_client_message()
        } else {
            upstream_message_or_excerpt(self.text(), self.parsed())
        }
    }

    /// The upstream body for the DEBUG log: a 403 needing the access-denied
    /// scrub ([`Self::needs_access_denied_scrub`]) collapses to the rendered
    /// access-denied string so no ARN or account id reaches the log.
    #[cfg(any(feature = "openai-compat", feature = "openai-responses"))]
    pub fn debug_body(&self) -> String {
        if self.is_native_envelope() && !self.needs_access_denied_scrub() {
            self.text().to_string()
        } else {
            crate::aws_error::sanitized_debug_body(
                self.status,
                self.text(),
                &self.access_denied_detail(),
            )
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
        let reason = self.reason_for((upstream_type.as_deref(), upstream_code.as_deref()));
        let body = match carried {
            CarriedBody::Original => self.body,
            CarriedBody::Replacement(text) => UpstreamBody::new(text),
            #[cfg(any(
                feature = "anthropic-api",
                feature = "bedrock",
                feature = "openai-compat",
                feature = "openai-responses"
            ))]
            CarriedBody::Built(body) => body,
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
        .with_upstream_reason(reason)
    }
}

#[cfg(test)]
#[path = "upstream_error_tests.rs"]
mod tests;

#[cfg(all(
    test,
    any(
        feature = "anthropic-api",
        feature = "bedrock",
        feature = "openai-compat",
        feature = "openai-responses"
    )
))]
#[path = "upstream_reason_tests.rs"]
pub mod reason_tests;
