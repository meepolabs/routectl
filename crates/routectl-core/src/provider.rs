//! Provider trait. Every backend (openai-compat, anthropic-api, bedrock)
//! implements this. Reasoning normalization is mandatory: request ->
//! provider-shape, provider-shape -> response, chunks -> normalized chunks.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::schema::{ChatChunk, ChatRequest, ChatResponse, TokenCount};

/// Result of a `routectl doctor` reachability probe against a provider.
/// Every variant is a display-safe discriminant or an operator-facing
/// message; a payload never carries a token, path, or env value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// The provider answered a minimal probe request successfully.
    Reachable,
    /// The upstream rejected the probe's credentials (typically a 401).
    AuthFailed(String),
    /// The upstream could not be reached (DNS, connect, or timeout).
    Unreachable(String),
    /// The endpoint completed a round trip but answered with a status the
    /// probe cannot read as a clean pass or a credential rejection (3xx,
    /// 404, 429, 5xx). The network path and TLS work, yet the endpoint's
    /// health is unproven -- a warning, not a pass and not a hard failure.
    IndeterminateHttp {
        /// The HTTP status the endpoint answered with.
        status: u16,
    },
    /// The provider has no free reachability probe. This is the default
    /// for any provider that does not override `Provider::probe`.
    UnsupportedFreeProbe,
    /// The probe was deliberately not run (e.g. no credentials configured).
    Skipped(String),
}

/// A translating backend: normalizes canonical requests to a provider's
/// wire format, runs the upstream call, and normalizes the response back
/// to canonical shape. Implemented by every backend (openai-compat,
/// anthropic-api, bedrock).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider id, e.g. "openai-compat:deepseek". Used in errors
    /// and `routectl_provider` response field.
    fn id(&self) -> &str;

    /// Map a routectl-shape request to the provider's wire format.
    /// This is where reasoning config is translated, unsupported params are
    /// stripped, and `provider_extras` are merged.
    fn normalize_request(&self, req: &ChatRequest) -> Result<Value>;

    /// Map a provider response (raw JSON) back to routectl shape.
    /// `reasoning_content` / `<think>` / `thinking` blocks all become
    /// `reasoning_details` entries with provider-tagged `format`.
    fn normalize_response(&self, raw: Value) -> Result<ChatResponse>;

    /// One-shot completion. Implementations call upstream HTTP, then run
    /// `normalize_response` on the raw body.
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse>;

    /// Streaming completion. Each yielded chunk is already normalized.
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>>;

    /// Probe call returning the token count for a request without
    /// invoking model inference. claude-code uses this for context-budget
    /// display.
    ///
    /// Default impl returns `Error::NotImplemented` so non-Anthropic
    /// providers don't need explicit overrides; the router treats this
    /// as a hard 501 rather than retrying or falling back. Anthropic's
    /// `AnthropicApiProvider` overrides this to call
    /// `POST /v1/messages/count_tokens`.
    ///
    /// Wire reference (Anthropic):
    /// <https://docs.anthropic.com/en/api/messages-count-tokens>
    async fn count_tokens(&self, _req: ChatRequest) -> Result<TokenCount> {
        Err(Error::NotImplemented(
            self.id().to_string(),
            "count_tokens".into(),
        ))
    }

    /// Notify the provider that a token it minted just got rejected by
    /// the upstream (typically a 401). Refreshable-auth providers
    /// (today: Anthropic with an `oauth://` ref) delegate to their
    /// `TokenSource::on_auth_failure` to force a refresh; static-auth
    /// providers (Bedrock SigV4, OpenAI-compat api-key) keep the
    /// default no-op since they have no rotation path. The router
    /// calls this before retrying the same provider once with a fresh
    /// token; a non-`Ok` return surfaces directly to the caller
    /// (the OAuth identity is dead until re-login -- walking the
    /// fallback chain would mask that).
    async fn on_auth_failure(&self) -> Result<()> {
        Ok(())
    }

    /// Cheap reachability check for `routectl doctor`. Returns a display-safe
    /// [`ProbeOutcome`]; the default reports `UnsupportedFreeProbe` so a
    /// provider without a free probe path needs no override. Overriders take
    /// only `&self`: credential presence is the CLI orchestration layer's
    /// concern, not the probe's.
    async fn probe(&self) -> ProbeOutcome {
        ProbeOutcome::UnsupportedFreeProbe
    }
}
