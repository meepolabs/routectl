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

pub mod anthropic;
pub mod openai;

/// Header used by harnesses that can override the canonical `model`
/// field directly to pin routing to a specific configured alias.
pub const ALIAS_HEADER: &str = "x-routectl-alias";

/// Read the `x-routectl-alias` header. Returns the trimmed header
/// value when present and non-empty; otherwise `None`. The ingress
/// uses this to override the wire `model` field when the client
/// can't easily change it (e.g. Claude Code, the OpenAI SDK behind
/// a fixed `model:` config).
///
/// v0.6.0 collapsed the `[ingress.X.aliases]` per-dialect maps into
/// the top-level `[aliases]` table. The ingress is now alias-agnostic:
/// it reads the wire `model` value (or the override header) and
/// forwards it verbatim to the router, which does all the alias
/// resolution. See `routectl_router::Router::resolve_v6_alias`.
pub fn read_alias_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ALIAS_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod read_alias_header_tests {
    use super::*;

    fn h(name: &str, val: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            val.parse().unwrap(),
        );
        h
    }

    #[test]
    fn header_with_value_returns_trimmed_string() {
        assert_eq!(
            read_alias_header(&h(ALIAS_HEADER, "  fast  ")),
            Some("fast".into())
        );
    }

    #[test]
    fn missing_header_returns_none() {
        assert_eq!(read_alias_header(&HeaderMap::new()), None);
    }

    #[test]
    fn empty_header_value_returns_none() {
        assert_eq!(read_alias_header(&h(ALIAS_HEADER, "   ")), None);
    }
}

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

/// Wire shape used to render error responses for a given ingress
/// dialect. The Anthropic ingress emits Anthropic's standard error
/// envelope (`{"type":"error","error":{"type","message"}}`); the
/// OpenAI ingress emits OpenAI's (`{"error":{"message","type","code"}}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorEnvelopeShape {
    OpenAi,
    Anthropic,
}

/// Translation surface for one ingress dialect. See module docs.
pub trait IngressAdapter: Send + Sync {
    fn id(&self) -> &str;

    /// Wire shape this ingress uses to render error envelopes. The
    /// generic ingress driver (`handlers::ingress_handle`) branches on
    /// this so 4xx/5xx responses match the dialect the client expects:
    /// Anthropic clients (claude-code, official SDK) parse
    /// `{"type":"error","error":{...}}`; OpenAI clients parse the
    /// flat `{"error":{...}}`. NO default impl: every adapter must
    /// declare its envelope so the choice is reviewable in code.
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape;

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
