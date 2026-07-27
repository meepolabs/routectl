//! The crate-wide [`enum@Error`] type and its [`Result`] alias.
//!
//! One error enum spans caller errors, configuration failures,
//! upstream/provider failures, and unexpected runtime faults. The HTTP
//! boundary maps each variant to a status and a client-safe body while
//! keeping operator detail in logs.

use std::fmt;

use thiserror::Error;

/// Convenience alias defaulting the error type to [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

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
        upstream_type: Option<String>,
        /// The upstream error code (`error.code` on the OpenAI error
        /// envelope; numeric codes are stringified). `None` when absent.
        /// NOT surfaced in the Display string.
        upstream_code: Option<String>,
    },

    /// Request could not be normalized into the canonical shape for the
    /// named provider.
    #[error("provider `{0}`: request normalization failed: {1}")]
    NormalizeRequest(String, String),

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
    /// Request.
    #[error("validation: {0}")]
    Validation(String),

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
/// for every variant and every field EXCEPT [`Error::Upstream`]'s `body`:
/// that renders as a `body_excerpt` (capped at [`crate::MAX_LOG_BODY_EXCERPT`]
/// chars) plus a `body_len` total-byte marker, so a request-fault envelope
/// carried up to [`crate::MAX_ERROR_BODY_BYTES`] cannot flood a `?e` log
/// sink (retry / fallback / stream-fallback WARN lines) at full size. Only
/// the Debug rendering is bounded; consumers that read the `body` FIELD
/// directly (the capability matcher) still see it in full.
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
            } => f
                .debug_struct("Upstream")
                .field("provider", provider)
                .field("status", status)
                .field("body_excerpt", &body_debug_excerpt(body))
                .field("body_len", &body.len())
                .field("retry_after", retry_after)
                .field("upstream_type", upstream_type)
                .field("upstream_code", upstream_code)
                .finish(),
            Self::NormalizeRequest(a, b) => {
                f.debug_tuple("NormalizeRequest").field(a).field(b).finish()
            }
            Self::NormalizeResponse(a, b) => f
                .debug_tuple("NormalizeResponse")
                .field(a)
                .field(b)
                .finish(),
            Self::UnknownProvider(s) => f.debug_tuple("UnknownProvider").field(s).finish(),
            Self::UnknownAlias(s) => f.debug_tuple("UnknownAlias").field(s).finish(),
            Self::Auth(s) => f.debug_tuple("Auth").field(s).finish(),
            Self::Config(s) => f.debug_tuple("Config").field(s).finish(),
            Self::Internal(s) => f.debug_tuple("Internal").field(s).finish(),
            Self::Validation(s) => f.debug_tuple("Validation").field(s).finish(),
            Self::Streaming(s) => f.debug_tuple("Streaming").field(s).finish(),
            Self::NotImplemented(a, b) => {
                f.debug_tuple("NotImplemented").field(a).field(b).finish()
            }
            Self::Io(e) => f.debug_tuple("Io").field(e).finish(),
            Self::Json(e) => f.debug_tuple("Json").field(e).finish(),
        }
    }
}

/// Render an [`Error::Upstream`] `body` for `Debug`: the first
/// [`crate::MAX_LOG_BODY_EXCERPT`] chars, with a `... [truncated]` marker
/// when the body ran longer. Char-count truncation is fine here (the
/// excerpt is bounded to `MAX_LOG_BODY_EXCERPT` chars, a few KB at most);
/// the accompanying `body_len` field carries the true byte length.
fn body_debug_excerpt(body: &str) -> String {
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
        Self::Upstream {
            provider: provider.into(),
            status,
            body: body.into(),
            retry_after: None,
            upstream_type: None,
            upstream_code: None,
        }
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
        Self::Upstream {
            provider: provider.into(),
            status,
            body: body.into(),
            retry_after,
            upstream_type: None,
            upstream_code: None,
        }
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
        Self::Upstream {
            provider: provider.into(),
            status,
            body: body.into(),
            retry_after,
            upstream_type,
            upstream_code,
        }
    }

    /// Construct a `NormalizeRequest` error for the named provider.
    pub fn normalize_request(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeRequest(provider.into(), msg.into())
    }

    /// Construct a `NormalizeResponse` error for the named provider.
    pub fn normalize_response(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeResponse(provider.into(), msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use std::time::Duration;

    #[test]
    fn upstream_ctor_sets_retry_after_none() {
        // Arrange + Act
        let err = Error::upstream("test", 429, "rate limited");

        // Assert
        match err {
            Error::Upstream {
                provider,
                status,
                body,
                retry_after,
                ..
            } => {
                assert_eq!(provider, "test");
                assert_eq!(status, 429);
                assert_eq!(body, "rate limited");
                assert!(
                    retry_after.is_none(),
                    "plain ctor must set retry_after = None"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    #[test]
    fn upstream_with_retry_after_carries_value() {
        // Arrange
        let hint = Duration::from_secs(42);

        // Act
        let err = Error::upstream_with_retry_after("test", 429, "rate limited", Some(hint));

        // Assert
        match err {
            Error::Upstream { retry_after, .. } => {
                assert_eq!(retry_after, Some(hint), "ctor must carry the reset hint");
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    /// `Debug` for `Error::Upstream` renders the body as a bounded excerpt
    /// plus a `body_len` marker, so a request-fault envelope carried up to
    /// `MAX_ERROR_BODY_BYTES` cannot flood a `?e` WARN sink at full size.
    #[test]
    fn upstream_debug_bounds_oversized_body() {
        let body_len = crate::MAX_LOG_BODY_EXCERPT * 8;
        let body = "z".repeat(body_len);
        let err = Error::upstream("prov", 400, body);

        let rendered = format!("{err:?}");

        // The rendered Debug string is bounded: it never carries the full
        // oversized body (the excerpt + marker + field names are far under it).
        assert!(
            rendered.len() < body_len,
            "Debug output must be bounded, got {} for a {body_len}-byte body",
            rendered.len()
        );
        assert!(
            rendered.contains("body_excerpt"),
            "Debug must render a bounded body_excerpt field, got: {rendered}"
        );
        assert!(
            rendered.contains("... [truncated]"),
            "an oversized body must carry the truncation marker, got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("body_len: {body_len}")),
            "Debug must carry the true total body length marker, got: {rendered}"
        );
    }

    /// A short body renders in full (no marker) and other fields keep their
    /// derived Debug shape.
    #[test]
    fn upstream_debug_short_body_renders_in_full() {
        let err = Error::upstream_full(
            "prov",
            429,
            "rate limited",
            Some(Duration::from_secs(5)),
            Some("rate_limit_exceeded".to_string()),
            None,
        );

        let rendered = format!("{err:?}");

        assert!(rendered.contains("body_excerpt: \"rate limited\""));
        assert!(!rendered.contains("... [truncated]"));
        assert!(rendered.contains("body_len: 12"));
        assert!(rendered.contains("upstream_type: Some(\"rate_limit_exceeded\")"));
        assert!(rendered.contains("retry_after: Some("));
    }

    /// Non-`Upstream` variants keep the derived Debug shape (tuple form).
    #[test]
    fn non_upstream_variant_debug_unchanged() {
        let err = Error::Validation("bad field".to_string());
        assert_eq!(format!("{err:?}"), "Validation(\"bad field\")");
    }
}
