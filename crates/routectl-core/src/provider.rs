//! Provider trait. Every backend (openai-compat, anthropic-api, bedrock)
//! implements this. Reasoning normalization is mandatory: request ->
//! provider-shape, provider-shape -> response, chunks -> normalized chunks.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::schema::{ChatChunk, ChatRequest, ChatResponse, TokenCount};

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

    /// Map one streaming SSE chunk to routectl shape. Returns `Ok(None)`
    /// for keep-alives or non-content frames the caller should drop.
    ///
    /// Providers that parse single SSE text lines synchronously (openai-compat,
    /// anthropic_api) must override this. Providers whose streaming uses a
    /// binary framing layer that the router decodes internally (Bedrock
    /// eventstream) can leave this default -- the router never calls it for
    /// those providers; chunk decoding happens inside `stream()` directly.
    fn normalize_chunk(&self, _raw: &str) -> Result<Option<ChatChunk>> {
        Ok(None)
    }

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
}
