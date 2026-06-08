//! Unit + integration tests for `ingress_handle`. Split out so
//! `ingress_handle.rs` stays under the project's 800-line file
//! ceiling. Loaded via
//! `#[cfg(test)] #[path = "ingress_handle_tests.rs"] mod tests;` from
//! `ingress_handle.rs`. `super::*` resolves to the `ingress_handle`
//! module since this file is the body of `mod tests` declared inside
//! `ingress_handle`.
//!
//! Coverage:
//!
//! - `map_error` envelope shape per dialect (Anthropic, OpenAI).
//! - `sanitize_stream_error_for_client`: provider names + upstream
//!   bodies must not leak through to the SSE wire bytes.
//! - `render_stream_task`: mid-stream upstream error path drives the
//!   adapter's `render_error_eos` to emit a dialect-specific terminal
//!   error event AFTER the chunks already rendered.
//!
//! The integration tests in
//! `crates/routectl-cli/tests/anthropic_ingress.rs` cover the
//! end-to-end path through axum; these tests pin the in-process
//! mapping without needing a server.

use super::*;
use axum::body::to_bytes;
use routectl_core::Error;

async fn body_to_value(resp: Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn anthropic_envelope_unknown_alias_emits_not_found_error() {
    // Arrange
    let err = Error::UnknownAlias("nonesuch".into());

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("nonesuch"));
}

#[tokio::test]
async fn anthropic_envelope_validation_error_emits_invalid_request_error() {
    // Arrange
    let err = Error::Validation("max_tokens must be positive".into());

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("max_tokens"));
}

#[tokio::test]
async fn anthropic_envelope_5xx_emits_api_error_or_overloaded() {
    // 503 -> overloaded_error
    let err503 = Error::upstream("p", 503, "service unavailable");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err503);
    let status = resp.status();
    let body = body_to_value(resp).await;
    assert_eq!(status.as_u16(), 503);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");

    // 529 -> overloaded_error
    let err529 = Error::upstream("p", 529, "anthropic overloaded");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err529);
    assert_eq!(resp.status().as_u16(), 529);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "overloaded_error");

    // 502 -> api_error
    let err502 = Error::upstream("p", 502, "bad gateway");
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err502);
    assert_eq!(resp.status().as_u16(), 502);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "api_error");
}

#[tokio::test]
async fn openai_envelope_unchanged_regression_pin() {
    // Pin the legacy OpenAI envelope shape so a future refactor
    // doesn't accidentally Anthropic-ify it. claude-code's
    // chat-completions adapter parses the flat `{"error":{...}}`
    // shape with `code` populated.
    let err = Error::UnknownAlias("nonesuch".into());

    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let status = resp.status();
    let body = body_to_value(resp).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.get("type").is_none(), "OpenAI envelope is flat");
    assert_eq!(body["error"]["type"], "unknown_alias");
    assert_eq!(body["error"]["code"], "unknown_alias");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("nonesuch"));
}

// -------- Layer B: OpenAI ingress preserves upstream type/code -------

#[tokio::test]
async fn openai_envelope_emits_upstream_type_when_present() {
    // Arrange: an upstream 429 carrying its own classifier.
    let err = Error::upstream_full(
        "p",
        429,
        "rate limited",
        None,
        Some("rate_limit_exceeded".into()),
        Some("rate_limited".into()),
    );

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert: the upstream type/code survive instead of "upstream_error".
    assert_eq!(body["error"]["type"], "rate_limit_exceeded");
    assert_eq!(body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn openai_envelope_falls_back_to_upstream_error_without_type() {
    // Arrange: an upstream error with no parsed classifier.
    let err = Error::upstream("p", 500, "boom");

    // Act
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let body = body_to_value(resp).await;

    // Assert: the legacy generic tag stays when no upstream type exists.
    assert_eq!(body["error"]["type"], "upstream_error");
    assert_eq!(body["error"]["code"], "upstream_error");
}

// -------- Layer C: Anthropic ingress non-stream status arms ----------

#[tokio::test]
async fn anthropic_envelope_maps_upstream_status_to_specific_types() {
    // 401 -> authentication_error
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 401, "nope"),
    );
    assert_eq!(resp.status().as_u16(), 401);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "authentication_error");

    // 403 -> permission_error
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 403, "nope"),
    );
    assert_eq!(resp.status().as_u16(), 403);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "permission_error");

    // 413 -> request_too_large
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 413, "too big"),
    );
    assert_eq!(resp.status().as_u16(), 413);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "request_too_large");

    // 503 -> overloaded_error (existing behavior preserved)
    let resp = map_error(
        ErrorEnvelopeShape::Anthropic,
        Error::upstream("p", 503, "down"),
    );
    assert_eq!(resp.status().as_u16(), 503);
    let body = body_to_value(resp).await;
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn anthropic_envelope_passes_through_valid_upstream_type() {
    // Arrange: an upstream type that is already valid Anthropic vocab
    // wins over the status-derived guess (502 would otherwise be
    // api_error).
    let err = Error::upstream_full(
        "p",
        502,
        "slow down",
        None,
        Some("rate_limit_error".into()),
        None,
    );

    // Act
    let resp = map_error(ErrorEnvelopeShape::Anthropic, err);
    let body = body_to_value(resp).await;

    // Assert
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

// -------- sanitize_stream_error_for_client --------------------------

/// The streaming-error sanitizer must NOT include the upstream
/// provider name or response body in the wire-bound message:
/// those are internal config / attacker-controlled bytes and
/// would leak through to the SDK consumer otherwise. Pin the
/// contract.
#[test]
fn sanitize_stream_error_strips_provider_and_body_from_upstream_error() {
    // Arrange
    let err = Error::upstream(
        "secret-provider-id",
        529,
        "Anthropic Overloaded: tenant-12345 exceeded quota",
    );

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert
    assert!(
        !safe.contains("secret-provider-id"),
        "provider name must not leak: {safe:?}"
    );
    assert!(
        !safe.contains("tenant-12345"),
        "upstream body must not leak: {safe:?}"
    );
    assert!(
        safe.contains("upstream stream error"),
        "kind tag present: {safe:?}"
    );
    assert!(
        safe.contains("529"),
        "HTTP status preserved for triage: {safe:?}"
    );
}

#[test]
fn sanitize_stream_error_uses_generic_message_for_streaming_kind() {
    // Arrange: Error::Streaming has no status; the sanitizer must
    // fall back to a generic "upstream stream error" string with
    // no internal detail.
    let err = Error::Streaming("anthropic in-stream error: overloaded_error".into());

    // Act
    let safe = sanitize_stream_error_for_client(&err);

    // Assert
    assert_eq!(safe, "upstream stream error");
    assert!(!safe.contains("anthropic"));
    assert!(!safe.contains("overloaded"));
}

// -------- render_stream_task: mid-stream upstream error path --------

/// Build a one-text-token canonical chunk for use in stream tests.
fn streaming_text_chunk(text: &str) -> routectl_core::ChatChunk {
    use routectl_core::{ChunkChoice, ChunkDelta};
    routectl_core::ChatChunk {
        id: "msg_test".into(),
        model: "test-model".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        usage: None,
        opaque_events: Vec::new(),
    }
}

/// Drain all SseEvents from a closed receiver. Used by the
/// integration tests below to inspect the wire-bound event
/// sequence without going through axum.
async fn drain(mut rx: tokio::sync::mpsc::Receiver<SseEvent>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

/// Anthropic ingress, mid-stream upstream error: the receiver
/// must see the rendered chunk events FIRST (`message_start`,
/// `content_block_start`, `content_block_delta`), then the
/// terminal `event: error` event, then the channel closes.
/// Without this, the stream truncated mid-chunk and Claude Code
/// SDK would retry up to 5 times on suspected truncation.
#[tokio::test]
async fn render_stream_task_anthropic_emits_chunk_then_terminal_error_event() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: a synthesized upstream stream that yields one
    // chunk then an Upstream-shaped error.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hello")),
            Err(Error::upstream(
                "secret-provider-id",
                529,
                "Anthropic Overloaded: tenant-12345",
            )),
        ]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    render_stream_task(upstream, AnthropicIngress, "anthropic".into(), tx).await;
    let events = drain(rx).await;

    // Assert: prefix chunk events + terminal error event.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().expect("Anthropic events are named"))
        .collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error",
        ],
        "expected chunk events + terminal error: {names:?}"
    );

    // The error event payload matches the Anthropic SSE spec.
    let err_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("error event present");
    let payload: Value = serde_json::from_str(&err_event.data).unwrap();
    assert_eq!(payload["type"], "error");
    // Layer D: a 529 upstream maps to overloaded_error, not api_error.
    assert_eq!(payload["error"]["type"], "overloaded_error");
    let msg = payload["error"]["message"].as_str().unwrap();
    // Sanitized: kind tag + status, NO provider id or body.
    assert!(msg.contains("upstream stream error"));
    assert!(msg.contains("529"));
    assert!(
        !msg.contains("secret-provider-id"),
        "provider must not leak: {msg:?}"
    );
    assert!(
        !msg.contains("tenant-12345"),
        "upstream body must not leak: {msg:?}"
    );
}

/// OpenAI ingress, mid-stream upstream error: the receiver must
/// see the rendered chunk first, then the error chunk, then the
/// `[DONE]` terminator, then the channel closes. `[DONE]` is the
/// OpenAI universal stream terminator; without it the SDK
/// treats the close as a truncation and retries.
#[tokio::test]
async fn render_stream_task_openai_emits_chunk_then_error_chunk_then_done() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one Ok chunk then one Err.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hi")),
            Err(Error::upstream(
                "secret-provider-id",
                503,
                "Service Unavailable",
            )),
        ]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    render_stream_task(upstream, OpenAiIngress, "openai".into(), tx).await;
    let events = drain(rx).await;

    // Assert: three events. OpenAI emits unnamed (bare data:)
    // frames, so .event is None on each.
    assert_eq!(events.len(), 3, "expected chunk + error + [DONE]");
    assert!(events.iter().all(|e| e.event.is_none()));

    // Event 0: the rendered chunk's serialized JSON contains
    // the text content.
    assert!(
        events[0].data.contains("\"content\":\"hi\""),
        "chunk event 0 missing content delta: {:?}",
        events[0].data
    );
    // Event 1: error envelope.
    let err_payload: Value = serde_json::from_str(&events[1].data).unwrap();
    // Layer D: a 503 upstream maps to overloaded_error, not api_error.
    assert_eq!(err_payload["error"]["type"], "overloaded_error");
    let msg = err_payload["error"]["message"].as_str().unwrap();
    assert!(msg.contains("upstream stream error"));
    assert!(msg.contains("503"));
    assert!(
        !msg.contains("secret-provider-id"),
        "provider must not leak: {msg:?}"
    );
    // Event 2: the universal [DONE] terminator.
    assert_eq!(events[2].data, "[DONE]");
}

/// Counterpart: the natural EOS path is unchanged. This pins
/// that the helper still emits `render_eos` events (and not
/// the error variant) when the upstream stream finishes
/// without an error.
#[tokio::test]
async fn render_stream_task_natural_eos_emits_render_eos_not_error() {
    use crate::ingress::openai::OpenAiIngress;

    // Arrange: one Ok chunk, then upstream ends naturally.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![Ok(streaming_text_chunk("hi"))]));
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    render_stream_task(upstream, OpenAiIngress, "openai".into(), tx).await;
    let events = drain(rx).await;

    // Assert: chunk + [DONE]. No error chunk.
    assert_eq!(events.len(), 2);
    assert!(events[0].data.contains("\"content\":\"hi\""));
    assert_eq!(events[1].data, "[DONE]");
    // Only [DONE] terminates a clean stream; if a stray error
    // envelope landed, we'd see three events.
    assert!(!events[0].data.contains("\"error\""));
}

/// Adapter wrapper that fails on the Nth call to `render_chunk`.
/// Used to drive path 3 of `render_stream_task` (chunk-render
/// failure) without simulating an upstream-stream Err. Delegates
/// every other trait method to the inner adapter so the wire
/// shapes (envelope, EOS, error EOS) match the wrapped dialect.
struct RenderChunkFailsOnceAdapter<A: IngressAdapter> {
    inner: A,
    calls: std::sync::atomic::AtomicUsize,
    fail_at_call: usize,
}

impl<A: IngressAdapter> RenderChunkFailsOnceAdapter<A> {
    fn new(inner: A, fail_at_call: usize) -> Self {
        Self {
            inner,
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_at_call,
        }
    }
}

impl<A: IngressAdapter> IngressAdapter for RenderChunkFailsOnceAdapter<A> {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        self.inner.error_envelope_shape()
    }
    fn parse_request(
        &self,
        headers: &HeaderMap,
        body: Value,
    ) -> routectl_core::Result<routectl_core::ChatRequest> {
        self.inner.parse_request(headers, body)
    }
    fn render_response(&self, resp: routectl_core::ChatResponse) -> routectl_core::Result<Value> {
        self.inner.render_response(resp)
    }
    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        self.inner.new_stream_state()
    }
    fn render_chunk(
        &self,
        chunk: routectl_core::ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == self.fail_at_call {
            return Err(Error::Streaming(
                "synthetic render_chunk failure for path-3 coverage".into(),
            ));
        }
        self.inner.render_chunk(chunk, state)
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        self.inner.render_eos(state)
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &crate::ingress::StreamErrorClass,
    ) -> Vec<SseEvent> {
        self.inner.render_error_eos(state, error, class)
    }
}

/// Path 3 of `render_stream_task`: the adapter's `render_chunk`
/// returns `Err` mid-stream. The driver must still emit the
/// dialect-specific terminal error event so SDK consumers see a
/// clean failure rather than a truncated stream. Mirrors
/// `render_stream_task_anthropic_emits_chunk_then_terminal_error_event`
/// but with the failure source on the ingress side rather than the
/// upstream stream. Pre-fix, this path returned without emitting
/// any terminator and the SDK's truncation-retry loop fired.
#[tokio::test]
async fn render_stream_task_anthropic_render_chunk_failure_emits_terminal_error() {
    use crate::ingress::anthropic::AnthropicIngress;

    // Arrange: two Ok chunks. The wrapper fails on the second
    // render_chunk so the first chunk goes through cleanly and the
    // wire bytes mirror the upstream-error variant: one set of
    // canonical chunk events followed by the terminal error event.
    let upstream: futures::stream::BoxStream<'static, routectl_core::Result<_>> =
        Box::pin(futures::stream::iter(vec![
            Ok(streaming_text_chunk("hello")),
            Ok(streaming_text_chunk(" world")),
        ]));
    let adapter = RenderChunkFailsOnceAdapter::new(AnthropicIngress, 1);
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Act
    render_stream_task(upstream, adapter, "anthropic".into(), tx).await;
    let events = drain(rx).await;

    // Assert: prefix chunk events from the first chunk, then the
    // terminal error event.
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.event.as_deref().expect("Anthropic events are named"))
        .collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error",
        ],
        "expected first-chunk events + terminal error: {names:?}"
    );

    // The error event payload matches the Anthropic SSE spec.
    let err_event = events
        .iter()
        .find(|e| e.event.as_deref() == Some("error"))
        .expect("error event present");
    let payload: Value = serde_json::from_str(&err_event.data).unwrap();
    assert_eq!(payload["type"], "error");
    assert_eq!(payload["error"]["type"], "api_error");
    let msg = payload["error"]["message"].as_str().unwrap();
    // sanitize_stream_error_for_client falls back to the generic
    // string for non-Upstream errors (Error::Streaming has no HTTP
    // status to surface).
    assert_eq!(msg, "upstream stream error");
}
