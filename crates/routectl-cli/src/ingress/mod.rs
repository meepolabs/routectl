//! Ingress adapter trait and shared types.
//!
//! routectl supports multiple ingress dialects: OpenAI Chat Completions
//! (`/v1/chat/completions`) and, in v0.4.0, Anthropic Messages
//! (`/v1/messages`). Each adapter knows how to:
//!
//! 1. Parse a request body into the canonical `ChatRequest`.
//! 2. Render a canonical `ChatResponse` into wire JSON for the client.
//! 3. Render a canonical `ChatChunk` into one or more SSE events.
//! 4. Produce an end-of-stream marker (e.g. OpenAI's `[DONE]`,
//!    Anthropic's `message_stop`).
//!
//! The trait is small on purpose: anything beyond translation belongs in
//! the router (alias resolution, retry, fallback) or core.

use axum::http::HeaderMap;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Result};
use serde_json::Value;

pub mod openai;

/// One server-sent event ready to write to the response stream.
///
/// `event` carries the SSE `event:` field for named events (Anthropic
/// emits `message_start`, `content_block_delta`, etc.). OpenAI omits
/// the field, which serializes as a bare `data: ...` frame.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn unnamed(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }
}

/// Per-stream state. Each ingress adapter defines a concrete state
/// type; the handler holds a `Box<dyn IngressStreamState>` while the
/// stream runs. Marker trait so we don't leak concrete state shapes
/// into the trait surface.
pub trait IngressStreamState: Send {
    /// Allow downcasting to the concrete state when the adapter needs
    /// to read its own typed fields. Implementations should return
    /// `self` cast to `&mut dyn Any`.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Translation surface for one ingress dialect. See module docs.
pub trait IngressAdapter: Send + Sync {
    fn id(&self) -> &str;

    /// Parse an incoming JSON body + headers into the canonical
    /// `ChatRequest`. Errors map to 4xx in the handler.
    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest>;

    /// Render a canonical `ChatResponse` into wire JSON for the client.
    fn render_response(&self, resp: ChatResponse) -> Result<Value>;

    /// Initial state for a new streaming response.
    fn new_stream_state(&self) -> Box<dyn IngressStreamState>;

    /// Render one canonical chunk into zero or more SSE events. Adapters
    /// own per-stream state via `state` (block-index counter for
    /// Anthropic, no-op for OpenAI).
    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>>;

    /// Final SSE events to emit after the upstream stream ends. OpenAI
    /// emits `[DONE]`; Anthropic may emit a final `message_stop` if
    /// the stream ended without an explicit stop event.
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent>;
}
