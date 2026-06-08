use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("provider `{provider}`: upstream HTTP {status}: {body}")]
    Upstream {
        provider: String,
        status: u16,
        body: String,
        /// Optional reset hint parsed from the upstream response (e.g.
        /// a `Retry-After` header). Consumed structurally by the router
        /// and circuit breaker to park the provider for the indicated
        /// duration; intentionally NOT surfaced in the Display string.
        /// `None` when the upstream sent no hint or it was unparseable.
        retry_after: Option<std::time::Duration>,
    },

    #[error("provider `{0}`: request normalization failed: {1}")]
    NormalizeRequest(String, String),

    #[error("provider `{0}`: response normalization failed: {1}")]
    NormalizeResponse(String, String),

    #[error("provider `{0}` not configured")]
    UnknownProvider(String),

    #[error("alias `{0}` not configured")]
    UnknownAlias(String),

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

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

// Convenience constructors used widely by provider impls.
impl Error {
    pub fn upstream(provider: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        Self::Upstream {
            provider: provider.into(),
            status,
            body: body.into(),
            retry_after: None,
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
        }
    }

    pub fn normalize_request(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeRequest(provider.into(), msg.into())
    }

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
}
