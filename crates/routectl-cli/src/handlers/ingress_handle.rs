//! Generic ingress driver: parses one HTTP body via an `IngressAdapter`,
//! routes to `Router::complete_with_options` / `stream_with_options`,
//! and renders the response/chunks via the same adapter.
//!
//! Both `/v1/chat/completions` (OpenAI) and `/v1/messages` (Anthropic)
//! handlers delegate here. The only difference between the two is the
//! adapter passed in.

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use routectl_core::Error;
use routectl_router::RouterOptions;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tracing::Instrument;

use crate::ingress::{IngressAdapter, IngressStreamState, SseEvent};
use crate::server::AppState;

const DISABLE_FALLBACKS_HEADER: &str = "x-routectl-disable-fallbacks";

pub async fn ingress_handle<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
    adapter: A,
) -> Response {
    let Json(raw_body) = match body {
        Ok(b) => b,
        Err(e) => {
            // JsonRejection carries the right status code for each
            // failure mode: 413 Payload Too Large for body-size cap
            // hits, 400 Bad Request for parse errors, 415 for missing
            // content-type, etc. Mirror that status into our error
            // envelope rather than collapsing every rejection into 400.
            let status = e.status();
            let kind = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "payload_too_large"
            } else if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
                "unsupported_media_type"
            } else {
                "bad_request"
            };
            return error_response(status, kind, &e.to_string(), "invalid_request_error");
        }
    };

    let req = match adapter.parse_request(&headers, raw_body) {
        Ok(r) => r,
        Err(e) => return map_error(e),
    };

    let mut opts = RouterOptions::new();
    // Gate `x-routectl-disable-fallbacks` behind the server-side
    // `[server] allow_disable_fallbacks` knob (default true). When the
    // operator turns it off (hardened multi-tenant deployments), the
    // header is silently ignored regardless of client intent so a
    // malicious client cannot disable HA fallbacks or probe per-
    // provider health.
    if state.router.config.server.allow_disable_fallbacks {
        opts.disable_fallbacks = header_truthy(&headers, DISABLE_FALLBACKS_HEADER);
    }

    let streaming = req.stream == Some(true);
    if streaming {
        stream_response(state, req, opts, adapter).await
    } else {
        complete_response(state, req, opts, adapter).await
    }
}

/// Treat a header value as truthy when set to "1", "true", or "yes"
/// (case-insensitive). Absent or empty headers are false.
fn header_truthy(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

async fn complete_response<A: IngressAdapter>(
    state: Arc<AppState>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
) -> Response {
    match state.router.complete_with_options(req, opts).await {
        Ok(resp) => match adapter.render_response(resp) {
            Ok(body) => {
                // FR-2: trace-level egress body for triage. Single
                // call site covers both ingresses (openai/anthropic)
                // because every non-streaming response funnels through
                // here after canonical -> wire serialization.
                routectl_core::trace_egress_body(adapter.id(), &body);
                (StatusCode::OK, Json(body)).into_response()
            }
            Err(e) => map_error(e),
        },
        Err(e) => map_error(e),
    }
}

async fn stream_response<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
) -> Response {
    let stream_result = state.router.stream_with_options(req, opts).await;

    let upstream = match stream_result {
        Ok(s) => s,
        Err(e) => return map_error(e),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    // Capture the current tracing span (which carries request_id from
    // the request_id middleware + ingress / router / provider span
    // hierarchy) and attach it to the spawned task. Without this,
    // `tokio::spawn` creates a fresh task with no parent span, so any
    // tracing::error! emitted from chunk-render / upstream-stream
    // failures lands without correlation context. Operators grepping
    // `request_id=<id>` would lose the streaming-error trail.
    let parent_span = tracing::Span::current();
    let ingress_id = adapter.id().to_string();
    tokio::spawn(
        async move {
            let mut upstream = upstream;
            let mut state: Box<dyn IngressStreamState> = adapter.new_stream_state();
            // FR-2: stream summary state. RAII guard so the summary
            // fires on EVERY exit path (clean close, render error,
            // upstream mid-stream error, client disconnect, runtime
            // task cancellation). Truncation detection uses an
            // inverse-flag pattern: the natural EOS path calls
            // `mark_clean_close()`; any other exit (including ones
            // we cannot explicitly mark, like task cancellation)
            // leaves `clean_close = false` and Drop synthesizes
            // `finish_reason="truncated"` so operators can
            // distinguish a clean close from a cut.
            let mut summary = EgressStreamSummary::new(ingress_id);
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(chunk) => {
                        summary.observe(&chunk);
                        match adapter.render_chunk(chunk, state.as_mut()) {
                            Ok(events) => {
                                for ev in events {
                                    if tx.send(Ok(sse_to_axum(ev))).await.is_err() {
                                        // Client disconnected mid-stream.
                                        // Drop emits truncated by
                                        // default (clean_close stays
                                        // false), so no explicit
                                        // marker call is needed.
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = ?e, "ingress chunk render failed");
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // Upstream stream errored mid-stream. Drop
                        // the channel without emitting adapter EOS
                        // (`[DONE]` for OpenAI, `message_stop` for
                        // Anthropic) -- emitting it would let the
                        // client see a clean completion despite the
                        // failure, AND would skew router-side health
                        // accounting (failed probe counted as a
                        // successful stream). The client sees the
                        // SSE connection close mid-stream, which
                        // SSE consumers should already treat as a
                        // non-clean termination. Egress summary
                        // emits truncated via the inverse-flag Drop.
                        tracing::error!(error = ?e, "upstream stream error -- terminating SSE without EOS sentinel");
                        return;
                    }
                }
            }
            for ev in adapter.render_eos(state.as_mut()) {
                if tx.send(Ok(sse_to_axum(ev))).await.is_err() {
                    // Client disconnected during EOS render. Drop
                    // emits truncated.
                    return;
                }
            }
            // Natural EOS reached. Mark clean close so the Drop
            // emit reports the observed finish_reason rather than
            // synthesizing "truncated".
            summary.mark_clean_close();
            // Clean close. The egress stream summary fires on Drop;
            // no explicit emit needed here.
            drop(summary);
        }
        .instrument(parent_span),
    );

    let receiver_stream = ReceiverStream::new(rx);
    Sse::new(receiver_stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn sse_to_axum(ev: SseEvent) -> Event {
    let mut e = Event::default().data(ev.data);
    if let Some(name) = ev.event {
        e = e.event(name);
    }
    e
}

fn map_error(e: Error) -> Response {
    let (status, type_str) = error_status_and_type(&e);
    // For server-internal classes (config / auth resolution), the
    // detailed Display message can leak diagnostic info to remote
    // clients -- AWS SDK error strings, env var names, file paths,
    // profile names. Log the full error server-side at ERROR level
    // (operators have logs) and return an opaque message in the
    // HTTP body. Other classes (Upstream, Validation, ...) carry
    // user-actionable detail and stay verbose.
    let public_message: String = match &e {
        Error::Auth(_) => {
            tracing::error!(error = %e, "auth error suppressed in HTTP response");
            "auth error: server-side credential resolution failed".to_string()
        }
        Error::Config(_) => {
            tracing::error!(error = %e, "config error suppressed in HTTP response");
            "internal configuration error".to_string()
        }
        _ => e.to_string(),
    };
    error_response(status, type_str, &public_message, type_str)
}

fn error_status_and_type(e: &Error) -> (StatusCode, &'static str) {
    match e {
        Error::UnknownAlias(_) | Error::UnknownProvider(_) => {
            (StatusCode::NOT_FOUND, "unknown_alias")
        }
        Error::Upstream { status, .. } => {
            let s = *status;
            let code = if (400..500).contains(&s) {
                StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_REQUEST)
            } else {
                StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)
            };
            (code, "upstream_error")
        }
        Error::NormalizeRequest(_, _) => (StatusCode::BAD_REQUEST, "bad_request"),
        Error::NormalizeResponse(_, _) => (StatusCode::BAD_GATEWAY, "bad_gateway"),
        Error::Validation(_) => (StatusCode::BAD_REQUEST, "validation_error"),
        Error::Auth(_) => (StatusCode::SERVICE_UNAVAILABLE, "auth_error"),
        Error::Streaming(_) => (StatusCode::BAD_GATEWAY, "streaming_error"),
        Error::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

fn error_response(status: StatusCode, err_type: &str, message: &str, code: &str) -> Response {
    let body: Value = json!({
        "error": {
            "message": message,
            "type": err_type,
            "code": code
        }
    });
    (status, Json(body)).into_response()
}

/// RAII guard that emits a single `direction=egress` stream summary on
/// drop. Mirrors the upstream-side `routectl_core::StreamWithSummary`
/// RAII guard that emits a single `direction=egress` stream summary on
/// drop. Mirrors the upstream-side `routectl_core::StreamWithSummary`
/// drop semantics: every exit path of the spawned SSE-render task
/// emits a summary so operators see a matching `direction=egress`
/// line for every `direction=upstream` line.
///
/// Truncation detection uses an inverse-flag pattern: the guard
/// starts with `clean_close = false`, and `mark_clean_close()` is
/// called only at the natural exit point. Drop emits
/// `finish_reason="truncated"` whenever `clean_close` is still
/// false -- which automatically covers ALL abnormal exit paths
/// including ones we cannot explicitly mark (most importantly
/// runtime task cancellation, where the future is dropped without
/// running any of our code paths first). Explicit error paths can
/// still call no special method; Drop alone handles them.
struct EgressStreamSummary {
    ingress_id: String,
    chunks: u64,
    last_finish: Option<String>,
    last_prompt: u32,
    last_completion: u32,
    last_total: u32,
    clean_close: bool,
}

impl EgressStreamSummary {
    fn new(ingress_id: String) -> Self {
        Self {
            ingress_id,
            chunks: 0,
            last_finish: None,
            last_prompt: 0,
            last_completion: 0,
            last_total: 0,
            clean_close: false,
        }
    }

    fn observe(&mut self, chunk: &routectl_core::ChatChunk) {
        self.chunks += 1;
        // Reverse-scan for the last non-None finish_reason, matching
        // the upstream-side StreamWithSummary semantics. Multi-choice
        // streams may carry the terminal finish on a non-last choice.
        for choice in chunk.choices.iter().rev() {
            if let Some(fr) = &choice.finish_reason {
                self.last_finish = Some(fr.clone());
                break;
            }
        }
        if let Some(u) = &chunk.usage {
            if let Some(p) = u.prompt_tokens {
                self.last_prompt = p;
            }
            if let Some(c) = u.completion_tokens {
                self.last_completion = c;
            }
            if let Some(t) = u.total_tokens {
                self.last_total = t;
            }
        }
    }

    /// Mark this stream as having reached the natural EOS. Drop will
    /// emit the summary with the observed `finish_reason` instead of
    /// the synthetic `"truncated"` value used for abnormal exits.
    fn mark_clean_close(&mut self) {
        self.clean_close = true;
    }
}

impl Drop for EgressStreamSummary {
    fn drop(&mut self) {
        let usage = (self.last_prompt != 0 || self.last_completion != 0 || self.last_total != 0)
            .then_some(routectl_core::Usage {
                prompt_tokens: self.last_prompt,
                completion_tokens: self.last_completion,
                total_tokens: self.last_total,
                ..Default::default()
            });
        // Inverse-flag truncation detection: any drop without a
        // matching `mark_clean_close()` is a non-clean exit. Covers
        // explicit error returns AND task cancellation (where Drop
        // runs without our code path running first).
        let finish_reason = if self.clean_close {
            self.last_finish.as_deref()
        } else {
            self.last_finish.as_deref().or(Some("truncated"))
        };
        routectl_core::trace_stream_summary(
            "egress",
            "ingress",
            &self.ingress_id,
            self.chunks,
            finish_reason,
            usage.as_ref(),
        );
    }
}
