//! The crate-wide [`enum@Error`] type and its [`Result`] alias.
//!
//! One error enum spans caller errors, configuration failures,
//! upstream/provider failures, and unexpected runtime faults. The HTTP
//! boundary maps each variant to a status and a client-safe body while
//! keeping operator detail in logs.

use std::fmt;

use thiserror::Error;

use crate::{ParsedErrorBody, RefusalReason, UpstreamBody, UpstreamReason};

/// Convenience alias defaulting the error type to [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

// `Error` is returned pervasively as `Result<_, Error>`, so its size is the
// success slot's size too. The two upstream classifier strings are
// `Box<str>` (16 bytes) rather than `String` (24), and the request id,
// parsed body and typed reason share one boxed `UpstreamExtras`, to keep the enum below
// clippy's `result_large_err` threshold (it fires AT 128 bytes) without a
// lint suppression.
const _: () = assert!(std::mem::size_of::<Error>() <= 128);

/// The crate-wide error type spanning caller errors, configuration
/// failures, upstream/provider failures, and unexpected runtime faults.
/// The HTTP boundary maps each variant to a status and a client-safe
/// body; operator detail stays in logs.
///
/// `Debug` is hand-written (not derived) so the [`Error::Upstream`] `body`
/// -- which can now carry a request-fault envelope up to
/// [`crate::MAX_ERROR_BODY_BYTES`] -- renders as a bounded excerpt with a
/// length marker rather than in full. Every `?e` log sink across all lanes
/// is bounded from one place; see the [`fmt::Debug`] impl below.
#[derive(Error)]
pub enum Error {
    /// An upstream provider returned an HTTP error. Carries the operator
    /// detail plus structured classifiers (`retry_after`, `upstream_type`,
    /// `upstream_code`) consumed by the router and surfaced to callers.
    #[error("provider `{provider}`: upstream HTTP {status}: {body}")]
    Upstream {
        /// Provider that produced the error.
        provider: String,
        /// Upstream HTTP status code.
        status: u16,
        /// Operator-facing detail from the upstream response.
        body: String,
        /// Optional reset hint parsed from the upstream response (e.g.
        /// a `Retry-After` header). Consumed structurally by the router
        /// and circuit breaker to park the provider for the indicated
        /// duration; intentionally NOT surfaced in the Display string.
        /// `None` when the upstream sent no hint or it was unparseable.
        retry_after: Option<std::time::Duration>,
        /// The upstream error classifier (`error.type` on the OpenAI /
        /// Anthropic error envelope), e.g. `rate_limit_exceeded`,
        /// `context_length_exceeded`, `permission_error`. Captured by
        /// the provider error readers and surfaced by the ingress so an
        /// SDK that branches on `error.type` keeps the upstream signal
        /// instead of a generic collapse. `None` when the upstream sent
        /// no parseable type. NOT surfaced in the Display string.
        upstream_type: Option<Box<str>>,
        /// The upstream error code (`error.code` on the OpenAI error
        /// envelope; numeric codes are stringified). `None` when absent.
        /// NOT surfaced in the Display string.
        upstream_code: Option<Box<str>>,
        /// The upstream correlation id, the parsed body and the typed
        /// [`UpstreamReason`]; read them through
        /// [`Error::upstream_request_id`], [`Error::parsed_body`] and
        /// [`Error::upstream_reason`]. NOT surfaced in the Display string.
        extras: UpstreamExtras,
    },

    /// Request could not be normalized into the canonical shape for the
    /// named provider. The [`RefusalReason`] names the structural reason;
    /// it is NOT surfaced in the Display string.
    #[error("provider `{0}`: request normalization failed: {1}")]
    NormalizeRequest(String, String, RefusalReason),

    /// Upstream response could not be normalized back to canonical shape
    /// for the named provider.
    #[error("provider `{0}`: response normalization failed: {1}")]
    NormalizeResponse(String, String),

    /// The named provider is not configured.
    #[error("provider `{0}` not configured")]
    UnknownProvider(String),

    /// The named alias is not configured.
    #[error("alias `{0}` not configured")]
    UnknownAlias(String),

    /// Authentication failed or credentials were missing.
    #[error("auth: {0}")]
    Auth(String),

    /// A credential is known to be unusable with exact evidence of why, and
    /// no upstream error is left to carry the reason: a terminal OAuth
    /// refresh, or every reachable seat already parked on a credential
    /// rejection. Displays like [`Error::Auth`] and classifies the same; read
    /// the reason through [`Error::upstream_reason`].
    #[error("auth: {detail}")]
    Credential {
        /// Operator detail; never surfaced to a client.
        detail: String,
        /// Why the credential is unusable.
        reason: UpstreamReason,
        /// Epoch-ms deadline of the earliest seat park when every reachable
        /// seat was parked; `None` for a terminal refresh failure, which
        /// parks nothing until the seat's next attempt settles it.
        parked_until_ms: Option<u64>,
    },

    /// A configuration value failed validation: malformed TOML, a
    /// missing required field, an invalid alias/model/provider entry,
    /// or a startup policy check (e.g. refusing a public bind without
    /// listener tokens). Detected when parsing or building from
    /// config, NOT during request processing. Surfaces as HTTP 500
    /// `config_error` with the detail suppressed from the client body
    /// (operators read the full message in logs). For unexpected
    /// runtime failures (serialization bugs, IO, impossible states)
    /// use `Internal` instead -- a client seeing `config_error` for a
    /// serialization bug is misleading and unactionable.
    #[error("config: {0}")]
    Config(String),

    /// An unexpected runtime failure that is neither caller error nor
    /// an upstream/provider problem: a response/chunk serialization
    /// bug, a socket bind / serve-loop / local_addr IO failure, or an
    /// "impossible state" path that no better variant covers. Surfaces
    /// as HTTP 500 `internal_error` with a generic client body -- the
    /// detail is for operators (logs), never exposed to callers.
    /// Distinct from `Config` (configuration validation) and `Io`
    /// (a bare `std::io::Error` carried verbatim via `#[from]`).
    #[error("internal: {0}")]
    Internal(String),

    /// Request rejected because a body failed a static invariant.
    /// Produced both by ingress adapters (e.g. cache_control
    /// 4-breakpoint cap, TTL ordering) and by egress translation in
    /// `routectl-providers` (e.g. an openai-compat wire-lift that hits
    /// an untranslatable canonical-only shape under
    /// `strict_translation`). HTTP handlers surface this as 400 Bad
    /// Request. The [`RefusalReason`] is operator vocabulary and never
    /// reaches the client text.
    #[error("validation: {0}")]
    Validation(String, RefusalReason),

    /// A provider refused to send a request because this egress has no legal
    /// representation for it, decided locally before any network I/O.
    ///
    /// Routes as a bad request for the fallback decision, and nothing else: it
    /// carries no upstream status, so no per-provider status remap can
    /// reclassify it, and the router never retries it on the same seat, never
    /// debits the seat's health for it, and never learns a capability from it.
    /// The next target in the chain builds from the untouched canonical
    /// request. `detail` names the shape only and never carries request
    /// content. Surfaces as HTTP 400 `invalid_request_error`.
    #[error("provider `{provider}`: refused before egress: {detail}")]
    LocalRefusal {
        /// Provider that refused the request.
        provider: String,
        /// Why the request has no legal representation on this egress.
        detail: String,
        /// The typed reason for the refusal; never part of the client text.
        reason: RefusalReason,
    },

    /// A streaming (SSE) transport or framing failure.
    #[error("streaming: {0}")]
    Streaming(String),

    /// The provider does not implement an optional `Provider` trait
    /// method (e.g. `count_tokens`). Producers: the default trait
    /// impl on `Provider` for any provider that hasn't overridden the
    /// method. Surfaces as 501 Not Implemented at the HTTP boundary
    /// and is NEVER retried (the next retry would land on the same
    /// provider with the same default impl). Distinct from `Validation`
    /// (caller error) and `Upstream` (request reached a provider but
    /// the upstream rejected it) -- this signals "routectl can't run
    /// this operation against this provider at all".
    #[error("not implemented for provider `{0}`: {1}")]
    NotImplemented(String, String),

    /// A bare `std::io::Error` carried verbatim.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization or deserialization failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Hand-written `Debug` for [`enum@Error`]. Identical to the derived form
/// for every variant and every field EXCEPT [`Error::LocalRefusal`]'s
/// `detail`, which renders through [`crate::sanitize_detail_for_log`], and
/// [`Error::Upstream`]'s `body`:
/// that renders as a `body_excerpt` (capped at [`crate::MAX_LOG_BODY_EXCERPT`]
/// chars) plus a `body_len` total-byte marker, so a request-fault envelope
/// carried up to [`crate::MAX_ERROR_BODY_BYTES`] cannot flood a `?e` log
/// sink (retry / fallback / stream-fallback WARN lines) at full size. Only
/// the Debug rendering is bounded; consumers that read the `body` FIELD
/// directly (the capability matcher) still see it in full. From `extras` it
/// renders the request id and the typed reason (as its stable token), never
/// the parsed body. A carried [`RefusalReason`] also renders as its stable
/// token.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream {
                provider,
                status,
                body,
                retry_after,
                upstream_type,
                upstream_code,
                extras,
            } => f
                .debug_struct("Upstream")
                .field("provider", provider)
                .field("status", status)
                .field("body_excerpt", &body_debug_excerpt(body))
                .field("body_len", &body.len())
                .field("retry_after", retry_after)
                .field("upstream_type", upstream_type)
                .field("upstream_code", upstream_code)
                .field("upstream_request_id", &extras.request_id())
                .field(
                    "upstream_reason",
                    &extras.reason().map(UpstreamReason::as_str),
                )
                .finish(),
            Self::NormalizeRequest(a, b, reason) => f
                .debug_tuple("NormalizeRequest")
                .field(a)
                .field(b)
                .field(&reason.as_str())
                .finish(),
            Self::NormalizeResponse(a, b) => f
                .debug_tuple("NormalizeResponse")
                .field(a)
                .field(b)
                .finish(),
            Self::UnknownProvider(s) => f.debug_tuple("UnknownProvider").field(s).finish(),
            Self::UnknownAlias(s) => f.debug_tuple("UnknownAlias").field(s).finish(),
            Self::Auth(s) => f.debug_tuple("Auth").field(s).finish(),
            Self::Credential {
                detail,
                reason,
                parked_until_ms,
            } => f
                .debug_struct("Credential")
                .field("detail", detail)
                .field("reason", &reason.as_str())
                .field("parked_until_ms", parked_until_ms)
                .finish(),
            Self::Config(s) => f.debug_tuple("Config").field(s).finish(),
            Self::Internal(s) => f.debug_tuple("Internal").field(s).finish(),
            Self::Validation(s, reason) => f
                .debug_tuple("Validation")
                .field(s)
                .field(&reason.as_str())
                .finish(),
            Self::LocalRefusal {
                provider,
                detail,
                reason,
            } => f
                .debug_struct("LocalRefusal")
                .field("provider", provider)
                .field("detail", &crate::sanitize_detail_for_log(detail))
                .field("reason", &reason.as_str())
                .finish(),
            Self::Streaming(s) => f.debug_tuple("Streaming").field(s).finish(),
            Self::NotImplemented(a, b) => {
                f.debug_tuple("NotImplemented").field(a).field(b).finish()
            }
            Self::Io(e) => f.debug_tuple("Io").field(e).finish(),
            Self::Json(e) => f.debug_tuple("Json").field(e).finish(),
        }
    }
}

/// Boxed tail of [`Error::Upstream`]: the upstream correlation id, the
/// parsed body and the typed reason, one pointer wide, allocated only when
/// any of them is present.
/// Built only by the `Error::upstream*` constructors -- it has no public
/// constructor, so no struct literal outside this crate can build the variant
/// -- and a carried parse therefore always describes the variant's `body`.
/// Its `Debug` prints the request id only, never the parse.
pub struct UpstreamExtras(Option<Box<UpstreamExtrasInner>>);

struct UpstreamExtrasInner {
    request_id: Option<Box<str>>,
    parsed: Option<ParsedErrorBody>,
    reason: Option<UpstreamReason>,
}

type ExtrasParts = (
    Option<Box<str>>,
    Option<ParsedErrorBody>,
    Option<UpstreamReason>,
);

impl UpstreamExtras {
    fn new(
        request_id: Option<Box<str>>,
        parsed: Option<ParsedErrorBody>,
        reason: Option<UpstreamReason>,
    ) -> Self {
        if request_id.is_none() && parsed.is_none() && reason.is_none() {
            return Self(None);
        }
        Self(Some(Box::new(UpstreamExtrasInner {
            request_id,
            parsed,
            reason,
        })))
    }

    fn take_parts(&mut self) -> ExtrasParts {
        match self.0.take() {
            Some(inner) => (inner.request_id, inner.parsed, inner.reason),
            None => (None, None, None),
        }
    }

    /// The upstream correlation id; see [`Error::upstream_request_id`].
    pub fn request_id(&self) -> Option<&str> {
        self.0.as_ref()?.request_id.as_deref()
    }

    /// The parsed body; see [`Error::parsed_body`].
    pub fn parsed(&self) -> Option<&ParsedErrorBody> {
        self.0.as_ref()?.parsed.as_ref()
    }

    /// The typed reason; see [`Error::upstream_reason`].
    pub fn reason(&self) -> Option<UpstreamReason> {
        self.0.as_ref()?.reason
    }
}

impl fmt::Debug for UpstreamExtras {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamExtras")
            .field("request_id", &self.request_id())
            .finish_non_exhaustive()
    }
}

/// Render an [`Error::Upstream`] `body` for `Debug`: the first
/// [`crate::MAX_LOG_BODY_EXCERPT`] chars, with a `... [truncated]` marker
/// when the body ran longer. Char-count truncation is fine here (the
/// excerpt is bounded to `MAX_LOG_BODY_EXCERPT` chars, a few KB at most);
/// the accompanying `body_len` field carries the true byte length.
///
/// An upstream error envelope can echo request fields back, so a body that
/// opens like JSON is redacted the way a traced body is (credential-named
/// keys and reasoning artifacts always, prompt content under
/// `ROUTECTL_LOG_REDACT_PROMPTS`) before excerpting. One that cannot be
/// redacted -- over [`crate::MAX_ERROR_BODY_BYTES`] (never parsed) or not
/// parseable (e.g. truncated) -- renders as a fixed marker, never a raw
/// prefix. Any other body is excerpted as-is. The shape test and marker
/// are shared with [`crate::redact_error_body_text`].
fn body_debug_excerpt(body: &str) -> String {
    body_debug_excerpt_with_flag(body, crate::log_safe::redact_enabled())
}

fn body_debug_excerpt_with_flag(body: &str, redact_prompts: bool) -> String {
    use crate::log_safe::ErrorJsonBody;
    match crate::log_safe::read_error_json_body(body) {
        ErrorJsonBody::NotJson => excerpt_chars(body),
        ErrorJsonBody::Unreadable(marker) => marker,
        ErrorJsonBody::Parsed(mut value) => {
            crate::log_safe::redact_in_place(&mut value, redact_prompts);
            excerpt_chars(&value.to_string())
        }
    }
}

fn excerpt_chars(body: &str) -> String {
    if body.chars().count() <= crate::MAX_LOG_BODY_EXCERPT {
        return body.to_string();
    }
    let mut excerpt = body
        .chars()
        .take(crate::MAX_LOG_BODY_EXCERPT)
        .collect::<String>();
    excerpt.push_str("... [truncated]");
    excerpt
}

// Convenience constructors used widely by provider impls.
impl Error {
    /// Construct an `Upstream` error with no reset hint or classifiers.
    pub fn upstream(provider: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        Self::upstream_full(provider, status, body, None, None, None)
    }

    /// Construct an `Upstream` error carrying a reset hint (e.g. a
    /// parsed `Retry-After` value). The router and circuit breaker read
    /// `retry_after` to decide how long to park the provider. Pass
    /// `None` to behave exactly like [`Error::upstream`].
    pub fn upstream_with_retry_after(
        provider: impl Into<String>,
        status: u16,
        body: impl Into<String>,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        Self::upstream_full(provider, status, body, retry_after, None, None)
    }

    /// Construct an `Upstream` error carrying the full classifier set:
    /// the reset hint plus the upstream `error.type` / `error.code`
    /// parsed from the response body. The ingress surfaces
    /// `upstream_type` / `upstream_code` so an SDK that branches on
    /// `error.type` (rate limit, context length, auth, ...) keeps the
    /// upstream signal rather than a generic collapse. Pass `None` for
    /// any field the upstream did not supply.
    pub fn upstream_full(
        provider: impl Into<String>,
        status: u16,
        body: impl Into<String>,
        retry_after: Option<std::time::Duration>,
        upstream_type: Option<String>,
        upstream_code: Option<String>,
    ) -> Self {
        let body = body.into();
        let parsed_body = ParsedErrorBody::parse(&body);
        Self::upstream_parts(
            provider.into(),
            status,
            body,
            parsed_body,
            retry_after,
            upstream_type,
            upstream_code,
        )
    }

    /// [`Error::upstream_full`] for a caller that already parsed the body:
    /// `body` carries its own parse ([`UpstreamBody::new`]), which is
    /// attached as-is, so the text is not parsed again and the carried parse
    /// is always the parse of the carried text.
    pub fn upstream_with_body(
        provider: impl Into<String>,
        status: u16,
        body: UpstreamBody,
        retry_after: Option<std::time::Duration>,
        upstream_type: Option<String>,
        upstream_code: Option<String>,
    ) -> Self {
        let (body, parsed_body) = body.into_parts();
        Self::upstream_parts(
            provider.into(),
            status,
            body,
            parsed_body,
            retry_after,
            upstream_type,
            upstream_code,
        )
    }

    fn upstream_parts(
        provider: String,
        status: u16,
        body: String,
        parsed_body: Option<ParsedErrorBody>,
        retry_after: Option<std::time::Duration>,
        upstream_type: Option<String>,
        upstream_code: Option<String>,
    ) -> Self {
        Self::Upstream {
            provider,
            status,
            body,
            retry_after,
            upstream_type: upstream_type.map(Into::into),
            upstream_code: upstream_code.map(Into::into),
            extras: UpstreamExtras::new(None, parsed_body, None),
        }
    }

    /// Attach an upstream provider correlation id (lifted from the response
    /// headers) to an `Upstream` error, returning the updated error. A no-op
    /// on any other variant, so a mapper can chain it unconditionally after
    /// building the error. Pass `None` to leave the id unset. Kept as a
    /// post-construction step (rather than a parameter on every `Upstream`
    /// constructor) so only the mappers that read an upstream HTTP response
    /// populate it; every other construction site stays `None` untouched.
    #[must_use]
    pub fn with_upstream_request_id(mut self, id: Option<String>) -> Self {
        if let Self::Upstream { extras, .. } = &mut self {
            let (_, parsed, reason) = extras.take_parts();
            *extras = UpstreamExtras::new(id.map(Into::into), parsed, reason);
        }
        self
    }

    /// Attach the typed reason an upstream gave for the rejection, returning
    /// the updated error. A no-op on any other variant, so a mapper can chain
    /// it unconditionally. Pass `None` to leave the error unclassified.
    #[must_use]
    pub fn with_upstream_reason(mut self, reason: Option<UpstreamReason>) -> Self {
        if let Self::Upstream { extras, .. } = &mut self {
            let (request_id, parsed, _) = extras.take_parts();
            *extras = UpstreamExtras::new(request_id, parsed, reason);
        }
        self
    }

    /// The typed reason a credential was rejected for: the one attached by
    /// [`Error::with_upstream_reason`], or the one an [`Error::Credential`]
    /// carries. `None` on any other variant, or when the rejection is
    /// unclassified.
    pub fn upstream_reason(&self) -> Option<UpstreamReason> {
        match self {
            Self::Upstream { extras, .. } => extras.reason(),
            Self::Credential { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// The upstream provider's own correlation id lifted from the response
    /// headers (first present of `x-request-id`, `x-oai-request-id`,
    /// `cf-ray`). Surfaced by the ingress on a client-facing
    /// `x-upstream-request-id` header so a caller can quote it to vendor
    /// support. `None` on any other variant, when the upstream sent no such
    /// header, or when the error was built without an upstream HTTP response.
    pub fn upstream_request_id(&self) -> Option<&str> {
        match self {
            Self::Upstream { extras, .. } => extras.request_id(),
            _ => None,
        }
    }

    /// The single capped JSON parse of an `Upstream` error's `body`
    /// (the one [`UpstreamBody::new`] makes), attached by every `upstream*`
    /// constructor so consumers never parse `body` again. `None` on any other
    /// variant, or when `body` is not JSON, is over
    /// [`crate::MAX_ERROR_BODY_BYTES`], or does not parse.
    pub fn parsed_body(&self) -> Option<&ParsedErrorBody> {
        match self {
            Self::Upstream { extras, .. } => extras.parsed(),
            _ => None,
        }
    }

    /// Why routectl refused the request locally: the reason an
    /// [`Error::Validation`], [`Error::LocalRefusal`] or
    /// [`Error::NormalizeRequest`] carries, and
    /// [`RefusalReason::Unparseable`] for an [`Error::Json`]. `None` on
    /// every other variant.
    pub const fn refusal_reason(&self) -> Option<RefusalReason> {
        match self {
            Self::Validation(_, reason)
            | Self::LocalRefusal { reason, .. }
            | Self::NormalizeRequest(_, _, reason) => Some(*reason),
            Self::Json(_) => Some(RefusalReason::Unparseable),
            _ => None,
        }
    }

    /// Construct a `NormalizeRequest` error for the named provider,
    /// refused for `reason`.
    pub fn normalize_request(
        provider: impl Into<String>,
        msg: impl Into<String>,
        reason: RefusalReason,
    ) -> Self {
        Self::NormalizeRequest(provider.into(), msg.into(), reason)
    }

    /// Construct a `NormalizeResponse` error for the named provider.
    pub fn normalize_response(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeResponse(provider.into(), msg.into())
    }
}

#[cfg(test)]
#[path = "error_reason_tests.rs"]
mod reason_tests;

#[cfg(test)]
#[path = "error_refusal_tests.rs"]
mod refusal_tests;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
