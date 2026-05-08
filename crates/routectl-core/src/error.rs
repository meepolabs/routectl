use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("provider `{provider}`: upstream HTTP {status}: {body}")]
    Upstream {
        provider: String,
        status: u16,
        body: String,
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

    #[error("config: {0}")]
    Config(String),

    /// Request rejected before reaching an upstream provider. Produced by
    /// ingress adapters when a request body fails a static invariant
    /// (e.g. cache_control 4-breakpoint cap, TTL ordering). HTTP handlers
    /// surface this as 400 Bad Request.
    #[error("validation: {0}")]
    Validation(String),

    #[error("streaming: {0}")]
    Streaming(String),

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
        }
    }

    pub fn normalize_request(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeRequest(provider.into(), msg.into())
    }

    pub fn normalize_response(provider: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NormalizeResponse(provider.into(), msg.into())
    }
}
