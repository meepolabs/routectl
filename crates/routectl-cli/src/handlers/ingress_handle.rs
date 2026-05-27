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

use crate::ingress::{ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent};
use crate::server::AppState;

const DISABLE_FALLBACKS_HEADER: &str = "x-routectl-disable-fallbacks";

pub async fn ingress_handle<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
    adapter: A,
) -> Response {
    let envelope = adapter.error_envelope_shape();
    let Json(raw_body) = match body {
        Ok(b) => b,
        Err(e) => return render_json_rejection(envelope, e),
    };

    let req = match adapter.parse_request(&headers, raw_body) {
        Ok(r) => r,
        Err(e) => return map_error(envelope, e),
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

    // Trace-level ingress request headers (direction 1: client ->
    // routectl). Opt-in via ROUTECTL_TRACE_HEADERS. Single call site
    // here covers both dialects and both the stream + non-stream
    // paths below; inherits the request_id span like trace_ingress_body.
    // The guarded wrapper builds zero header JSON unless the toggle and
    // TRACE are both on (mirrors routectl_providers::header_trace).
    trace_ingress_headers_of(adapter.id(), &headers);

    let streaming = req.stream == Some(true);
    if streaming {
        stream_response(state, req, opts, adapter).await
    } else {
        complete_response(state, req, opts, adapter, envelope).await
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

/// Render an `axum::extract::rejection::JsonRejection` into a properly
/// shaped error envelope while preserving the rejection's status code.
///
/// Axum's body extractor surfaces 413 Payload Too Large (body-size cap),
/// 415 Unsupported Media Type (missing/wrong content-type), and 400 Bad
/// Request (parse failure) on the rejection's `status()`. Both
/// `/v1/messages` and `/v1/messages/count_tokens` route their JSON
/// rejections through here so the status code never gets collapsed to
/// 400 and the dialect-correct envelope (Anthropic vs OpenAI) is used.
pub(crate) fn render_json_rejection(
    shape: ErrorEnvelopeShape,
    e: axum::extract::rejection::JsonRejection,
) -> Response {
    // JsonRejection carries the right status code for each failure
    // mode: 413 Payload Too Large for body-size cap hits, 400 Bad
    // Request for parse errors, 415 for missing content-type, etc.
    // Mirror that status into our error envelope rather than
    // collapsing every rejection into 400.
    let status = e.status();
    let kind = if status == StatusCode::PAYLOAD_TOO_LARGE {
        "payload_too_large"
    } else if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        "unsupported_media_type"
    } else {
        "bad_request"
    };
    error_response(shape, status, kind, &e.to_string(), "invalid_request_error")
}

async fn complete_response<A: IngressAdapter>(
    state: Arc<AppState>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
    envelope: ErrorEnvelopeShape,
) -> Response {
    match state.router.complete_with_options(req, opts).await {
        Ok(resp) => match adapter.render_response(resp) {
            Ok(body) => {
                // Trace-level egress body for triage. Single
                // call site covers both ingresses (openai/anthropic)
                // because every non-streaming response funnels through
                // here after canonical -> wire serialization.
                routectl_core::trace_egress_body(adapter.id(), &body);
                let resp = (StatusCode::OK, Json(body)).into_response();
                // Dir 4: egress response headers, captured from the
                // built response so the trace reflects what the client
                // receives. Read before returning (no borrow conflict;
                // the helper only reads `resp.headers()`).
                trace_egress_headers_of(adapter.id(), &resp);
                resp
            }
            Err(e) => map_error(envelope, e),
        },
        Err(e) => map_error(envelope, e),
    }
}

async fn stream_response<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
) -> Response {
    let envelope = adapter.error_envelope_shape();
    let stream_result = state.router.stream_with_options(req, opts).await;

    let upstream = match stream_result {
        Ok(s) => s,
        Err(e) => return map_error(envelope, e),
    };

    // Inner channel carries our `SseEvent` type so the rendering loop
    // is straightforward to unit-test (drain a `mpsc::Receiver<SseEvent>`
    // and assert on event names + payload bytes). The conversion to
    // `axum::response::sse::Event` is a one-liner `.map()` on the
    // ReceiverStream and happens only on the production path below.
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Capture the current tracing span (which carries request_id from
    // the request_id middleware + ingress / router / provider span
    // hierarchy) and attach it to the spawned task. Without this,
    // `tokio::spawn` creates a fresh task with no parent span, so any
    // tracing::error! emitted from chunk-render / upstream-stream
    // failures lands without correlation context. Operators grepping
    // `request_id=<id>` would lose the streaming-error trail.
    let parent_span = tracing::Span::current();
    let ingress_id = adapter.id().to_string();
    // Clone for the post-spawn dir-4 egress-headers trace; `ingress_id`
    // itself moves into the spawned render task (EgressStreamSummary),
    // as does `adapter`, so neither is reachable after the spawn.
    let egress_id = ingress_id.clone();
    tokio::spawn(render_stream_task(upstream, adapter, ingress_id, tx).instrument(parent_span));

    let receiver_stream =
        ReceiverStream::new(rx).map(|ev| Ok::<Event, std::convert::Infallible>(sse_to_axum(ev)));
    let resp = Sse::new(receiver_stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response();
    // Dir 4 (streaming egress): capture the SSE response headers
    // (content-type: text/event-stream, keep-alive, ...) before
    // returning. Uses the pre-spawn `egress_id` clone.
    trace_egress_headers_of(&egress_id, &resp);
    resp
}

/// Drive the upstream chunk stream through the ingress adapter,
/// emitting one `SseEvent` per produced wire event. Exit paths:
///
/// 1. Upstream finishes naturally -> emit `render_eos` events,
///    mark the egress summary `clean_close=true` so the Drop summary
///    reports the observed `finish_reason`.
/// 2. Upstream errors mid-stream -> emit `render_error_eos` events
///    (the dialect-specific terminal ERROR event), then return.
///    The summary Drop synthesizes `finish_reason="truncated"`
///    via the inverse-flag pattern -- the upstream stream WAS
///    truncated even though we now signal it cleanly to the
///    client.
/// 3. Render failure (canonical chunk that the adapter cannot turn
///    into wire events) -> log + return. No EOS emission; the client
///    sees an unclean disconnect because we don't have a
///    well-formed wire event to send.
/// 4. Client disconnects (channel send returns Err) -> return.
///    Drop emits truncated.
///
/// Extracted from the spawn closure body so the streaming-error path
/// is unit-testable without spinning up the axum layer: a test can
/// build a synthesized `BoxStream<Result<ChatChunk>>` (e.g. one Ok
/// chunk followed by one Err) and drain the resulting
/// `mpsc::Receiver<SseEvent>` to assert on the wire shape of the
/// terminal error event.
async fn render_stream_task<A: IngressAdapter>(
    upstream: futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    adapter: A,
    ingress_id: String,
    tx: tokio::sync::mpsc::Sender<SseEvent>,
) {
    let mut upstream = upstream;
    let mut state: Box<dyn IngressStreamState> = adapter.new_stream_state();
    // Stream summary state. RAII guard so the summary
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
                            if tx.send(ev).await.is_err() {
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
                        // Render failure on a canonical chunk (rare;
                        // the adapter could not turn it into wire
                        // events). Emit the dialect-specific terminal
                        // error event so the client sees a clean
                        // FAILURE rather than a truncated stream and
                        // does not retry. The drop summary still
                        // reports `finish_reason=truncated` via the
                        // inverse-flag pattern. The send-failure
                        // result is intentionally discarded: if the
                        // client already disconnected, the Drop on
                        // EgressStreamSummary still fires.
                        tracing::error!(error = ?e, "ingress chunk render failed");
                        let safe_msg = sanitize_stream_error_for_client(
                            &routectl_core::Error::Streaming(e.to_string()),
                        );
                        for ev in adapter.render_error_eos(state.as_mut(), &safe_msg) {
                            let _ = tx.send(ev).await;
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                // Path 2: upstream stream errored mid-stream. Emit the
                // dialect-specific terminal ERROR event (see the
                // function-level rustdoc above for the SUCCESS-vs-ERROR
                // EOS distinction and the rationale).
                tracing::error!(
                    error = ?e,
                    "upstream stream error -- emitting terminal error event"
                );
                let safe_msg = sanitize_stream_error_for_client(&e);
                for ev in adapter.render_error_eos(state.as_mut(), &safe_msg) {
                    if tx.send(ev).await.is_err() {
                        // Client disconnected before we could
                        // deliver the terminal error event. Drop
                        // emits truncated.
                        return;
                    }
                }
                return;
            }
        }
    }
    for ev in adapter.render_eos(state.as_mut()) {
        if tx.send(ev).await.is_err() {
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

/// Map a routectl `Error` to a short, client-safe summary suitable
/// for inclusion in the streaming-error wire payload. STRIPPED of
/// provider names, upstream response bodies, and any other internal
/// tells so the wire bytes never leak secrets-store identifiers,
/// deploy hostnames, tokens, or attacker-controlled upstream content.
///
/// The `body_excerpt` from `Error::Upstream` is intentionally
/// dropped: it can carry attacker-controlled bytes that, even after
/// `sanitize_for_log`, can leak per-tenant existence info or
/// upstream-side rate limit hints we don't want to forward.
/// Operators reading routectl logs still see the full error via the
/// `tracing::error!(error = ?e, ...)` line that fires on the same
/// path -- the wire bytes are the only place this short summary
/// shows up.
///
/// Used only on the streaming-error path (`render_error_eos`). The
/// non-streaming path goes through `map_error`, which carries
/// caller-actionable validation detail and is appropriate to surface.
fn sanitize_stream_error_for_client(e: &Error) -> String {
    match e {
        Error::Upstream { status, .. } => format!("upstream stream error (HTTP {status})"),
        _ => "upstream stream error".to_string(),
    }
}

fn sse_to_axum(ev: SseEvent) -> Event {
    let mut e = Event::default().data(ev.data);
    if let Some(name) = ev.event {
        e = e.event(name);
    }
    e
}

/// True only when an axum-side header trace should be BUILT here: the
/// operator opted in via ROUTECTL_TRACE_HEADERS. Cheap env-toggle check
/// that skips the `headers_to_json` allocation on the default path.
///
/// The TRACE-LEVEL gate is intentionally NOT checked here:
/// `event_enabled!` resolves against THIS module's target
/// (`routectl_cli::*`, which runs at `info` under the usual filter), so
/// a level check here would always be false and suppress every header
/// trace. The core `trace_*_headers` emitters re-check TRACE against the
/// `routectl_core::log_safe` target where it is actually enabled. Kept
/// CLI-side so routectl-core stays decoupled from the axum / http
/// `HeaderMap` type.
fn header_trace_enabled_here() -> bool {
    routectl_core::header_trace_enabled()
}

/// Trace dir-1 ingress request headers (client -> routectl) from the
/// inbound axum `HeaderMap`. No-op, and no allocation, unless header
/// tracing is on. Single call site covers both dialects and the
/// stream + non-stream paths.
fn trace_ingress_headers_of(ingress_id: &str, headers: &HeaderMap) {
    if !header_trace_enabled_here() {
        return;
    }
    routectl_core::trace_ingress_headers(
        ingress_id,
        &routectl_core::headers_to_json(headers.iter().map(|(k, v)| (k.as_str(), v.as_bytes()))),
    );
}

/// Trace dir-4 egress response headers (routectl -> client) from a
/// built `Response`. No-op, and no allocation, unless header tracing
/// is on. Shared by the non-streaming and streaming egress paths so
/// both emit an `egress response headers` line alongside the `egress
/// response body` trace.
fn trace_egress_headers_of(ingress_id: &str, resp: &Response) {
    if !header_trace_enabled_here() {
        return;
    }
    routectl_core::trace_egress_headers(
        ingress_id,
        &routectl_core::headers_to_json(
            resp.headers()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes())),
        ),
    );
}

pub(crate) fn map_error(shape: ErrorEnvelopeShape, e: Error) -> Response {
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
    error_response(shape, status, type_str, &public_message, type_str)
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
        Error::NotImplemented(_, _) => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

fn error_response(
    shape: ErrorEnvelopeShape,
    status: StatusCode,
    err_type: &str,
    message: &str,
    code: &str,
) -> Response {
    let body: Value = match shape {
        ErrorEnvelopeShape::OpenAi => json!({
            "error": {
                "message": message,
                "type": err_type,
                "code": code,
            }
        }),
        ErrorEnvelopeShape::Anthropic => json!({
            "type": "error",
            "error": {
                "type": anthropic_error_type(err_type, status),
                "message": message,
            }
        }),
    };
    (status, Json(body)).into_response()
}

/// Map routectl's internal `err_type` tag (the second field in
/// `error_status_and_type`) to the wire string Anthropic clients
/// expect on `error.type`. The Anthropic API uses a small fixed
/// vocabulary -- `invalid_request_error`, `authentication_error`,
/// `permission_error`, `not_found_error`, `rate_limit_error`,
/// `api_error`, `overloaded_error`. routectl's tags are richer
/// (e.g. `validation_error`, `payload_too_large`,
/// `unsupported_media_type`); collapse them to the closest
/// Anthropic equivalent so claude-code's per-`error.type` handling
/// fires correctly. Status code is consulted for `upstream_error`
/// to distinguish `overloaded_error` (503/529) from the generic
/// `api_error` bucket.
fn anthropic_error_type(err_type: &str, status: StatusCode) -> &'static str {
    match (err_type, status.as_u16()) {
        ("unknown_alias", _) | ("unknown_provider", _) => "not_found_error",
        ("bad_request", _)
        | ("validation_error", _)
        | ("payload_too_large", _)
        | ("unsupported_media_type", _) => "invalid_request_error",
        ("auth_error", _) | ("authentication_error", _) => "authentication_error",
        ("upstream_error", 503) | ("upstream_error", 529) => "overloaded_error",
        ("upstream_error", _) | ("streaming_error", _) | ("bad_gateway", _) => "api_error",
        (_, _) => "api_error",
    }
}

/// RAII guard that emits a single `direction=egress` stream summary on
/// drop. Complements the upstream-side
/// `routectl_core::StreamWithSummary` RAII guarantee: every exit path
/// of the spawned SSE-render task emits a summary so operators see a
/// matching `direction=egress` line for every `direction=upstream`
/// line.
///
/// Note the deliberate divergence from `StreamWithSummary`: the
/// upstream guard preserves the observed `last_finish` on cancellation
/// (it reports what the provider sent before drop). This guard
/// OVERRIDES `last_finish` with `"truncated"` whenever
/// `clean_close == false` so the egress side authoritatively reports
/// whether the client received a complete stream. Operators
/// correlating an egress `truncated` line with the upstream summary
/// see the actual upstream finish_reason on the upstream side.
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
///
/// `chunks` semantics: counted on `observe()` BEFORE the chunk is
/// rendered to SSE events and sent to the client. On a disconnect
/// during `tx.send()`, `chunks` includes the unsent final chunk --
/// it measures upstream chunks the egress task processed, NOT
/// chunks the client successfully received. Operators reading the
/// summary should treat `chunks` as a work-done counter, not a
/// delivery counter.
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
        // runs without our code path running first). The egress
        // summary is authoritative on egress-side completion --
        // override any observed `last_finish` with `"truncated"`
        // when `clean_close == false` so operators can grep
        // `direction=egress finish_reason=truncated` to enumerate
        // cuts. The upstream-side summary still carries the
        // observed upstream finish_reason for correlation.
        let finish_reason = if self.clean_close {
            self.last_finish.as_deref()
        } else {
            Some("truncated")
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

#[cfg(test)]
#[path = "ingress_handle_tests.rs"]
mod tests;
