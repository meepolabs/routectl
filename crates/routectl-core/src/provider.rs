//! Provider trait. Every backend (openai-compat, anthropic-api) implements
//! this. Reasoning normalization is mandatory: request -> provider-shape,
//! provider-shape -> response, chunks -> normalized chunks.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::error::Result;
use crate::schema::{ChatChunk, ChatRequest, ChatResponse};

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
}

