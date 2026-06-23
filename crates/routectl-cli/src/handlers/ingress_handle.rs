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
use routectl_usage::{Outcome, UsageHandle, UsageRecord};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tracing::Instrument;

use crate::handlers::usage_capture::{build_usage_draft, outcome_for_dispatch_err, UsageCapture};
use crate::ingress::{ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent};
use crate::server::request_id::RequestId;
use crate::server::AppState;

const DISABLE_FALLBACKS_HEADER: &str = "x-routectl-disable-fallbacks";

/// Inbound header that claude-code stamps with its logical session id.
/// Captured best-effort for the usage row's `session_id`; absent or
/// non-UTF-8 values yield `None`.
const SESSION_ID_HEADER: &str = "x-claude-code-session-id";

pub async fn ingress_handle<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    headers: HeaderMap,
    request_id: Option<RequestId>,
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

    // Snapshot the live Router once per request so a hot-swap mid-
    // request does not mix old + new routing state. Every read after
    // this point goes through `router`, not `state.router.load*`.
    let router = state.router.load_full();

    let mut opts = RouterOptions::new();
    // Gate `x-routectl-disable-fallbacks` behind the server-side
    // `[server] allow_disable_fallbacks` knob (default true). When the
    // operator turns it off (hardened multi-tenant deployments), the
    // header is silently ignored regardless of client intent so a
    // malicious client cannot disable HA fallbacks or probe per-
    // provider health.
    if router.config.server.allow_disable_fallbacks {
        opts.disable_fallbacks = header_truthy(&headers, DISABLE_FALLBACKS_HEADER);
    }

    // Trace-level ingress request headers (direction 1: client ->
    // routectl). Opt-in via ROUTECTL_TRACE_HEADERS. Single call site
    // here covers both dialects and both the stream + non-stream
    // paths below; inherits the request_id span like trace_ingress_body.
    // The guarded wrapper builds zero header JSON unless the toggle and
    // TRACE are both on (mirrors routectl_providers::header_trace).
    trace_ingress_headers_of(adapter.id(), &headers);

    // Seed the usage-capture draft from the request shape + identity
    // BEFORE dispatch, so a row is emitted on every exit path including
    // a pre-dispatch gate block (where no DispatchMeta-derived served_*
    // fields exist yet). The dispatch + token + outcome fields are
    // stamped later by the capture guard.
    let request_id = request_id.map(|r| r.0).unwrap_or_default();
    let session_id = session_id_of(&headers);
    let draft = build_usage_draft(adapter.id(), &req, request_id, session_id);

    let streaming = req.stream == Some(true);
    if streaming {
        stream_response(router, req, opts, adapter, state.usage.clone(), draft).await
    } else {
        complete_response(
            router,
            req,
            opts,
            adapter,
            envelope,
            state.usage.clone(),
            draft,
        )
        .await
    }
}

/// Best-effort logical session id from the inbound
/// `x-claude-code-session-id` header. `None` when absent or non-UTF-8.
fn session_id_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
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
    error_response(
        shape,
        status,
        kind,
        &e.to_string(),
        "invalid_request_error",
        None,
        None,
    )
}

async fn complete_response<A: IngressAdapter>(
    router: Arc<routectl_router::Router>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
    envelope: ErrorEnvelopeShape,
    usage: UsageHandle,
    draft: UsageRecord,
) -> Response {
    let mut capture = UsageCapture::new(draft, usage, adapter.id().to_string());
    let dispatched = router.complete_with_options(req, opts).await;
    capture.observe_meta(&dispatched.meta);
    match dispatched.result {
        Ok(resp) => {
            // Non-streaming first byte == the response being ready.
            capture.mark_first_byte();
            capture.observe_response(&resp);
            match adapter.render_response(resp) {
                Ok(body) => {
                    // Upstream delivered AND we serialized it: this is the
                    // only path where the client receives 200 + body, so
                    // finalize `ok` here rather than before the render.
                    capture.finalize(Outcome::Ok);
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
                Err(e) => {
                    // Upstream gave a good response but we could not
                    // serialize it to wire bytes -> the client receives an
                    // error, so the row must not say `ok`.
                    capture.observe_error(&e);
                    capture.finalize(Outcome::UpstreamError);
                    map_error(envelope, e)
                }
            }
        }
        Err(e) => {
            capture.observe_error(&e);
            capture.finalize(outcome_for_dispatch_err(&dispatched.meta));
            map_error(envelope, e)
        }
    }
}

async fn stream_response<A: IngressAdapter + 'static>(
    router: Arc<routectl_router::Router>,
    req: routectl_core::ChatRequest,
    opts: RouterOptions,
    adapter: A,
    usage: UsageHandle,
    draft: UsageRecord,
) -> Response {
    let envelope = adapter.error_envelope_shape();
    let mut capture = UsageCapture::new(draft, usage, adapter.id().to_string());
    let dispatched = router.stream_with_options(req, opts).await;
    capture.observe_meta(&dispatched.meta);

    let upstream = match dispatched.result {
        Ok(s) => s,
        Err(e) => {
            // Stream never started: classify off the meta + error and
            // emit the row here (no chunk task to carry the guard).
            capture.observe_error(&e);
            capture.finalize(outcome_for_dispatch_err(&dispatched.meta));
            return map_error(envelope, e);
        }
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
    let egress_id = adapter.id().to_string();
    // The capture guard moves INTO the render task so it finalizes on
    // every stream exit (natural EOS, mid-stream error, client
    // disconnect, task cancellation -- the last two via the Drop
    // fallback). `adapter` moves in too, so neither is reachable after
    // the spawn; the dir-4 egress-headers trace uses the pre-spawn
    // `egress_id` clone.
    tokio::spawn(render_stream_task(upstream, adapter, capture, tx).instrument(parent_span));

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
    mut capture: UsageCapture,
    tx: tokio::sync::mpsc::Sender<SseEvent>,
) {
    let mut upstream = upstream;
    let mut state: Box<dyn IngressStreamState> = adapter.new_stream_state();
    // The capture guard is the RAII summary + usage row for this
    // stream. It fires on EVERY exit path (clean close, render error,
    // upstream mid-stream error, client disconnect, runtime task
    // cancellation). Truncation / outcome detection uses an inverse-flag
    // pattern: the natural EOS path calls `finalize(Outcome::Ok)`; a
    // mid-stream upstream error calls `finalize(Outcome::UpstreamError)`;
    // any exit we cannot explicitly mark (client disconnect on a `tx`
    // send failure, render failure, task cancellation) leaves the guard
    // un-finalized and Drop stamps the `client_disconnect` fallback. So
    // exactly one row lands per stream, mapped to the right outcome.
    while let Some(item) = upstream.next().await {
        match item {
            Ok(chunk) => {
                // First chunk == the first byte the client can receive:
                // mark TTFB and lift the quota/usage carried on the
                // stream head BEFORE rendering.
                capture.mark_first_byte();
                capture.observe_chunk(&chunk);
                match adapter.render_chunk(chunk, state.as_mut()) {
                    Ok(events) => {
                        for ev in events {
                            if tx.send(ev).await.is_err() {
                                // Client disconnected mid-stream. The
                                // guard stays un-finalized, so Drop
                                // stamps `client_disconnect` -- no
                                // explicit call needed.
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
                        // does not retry. Finalize the usage row as
                        // `upstream_error` (with the render failure
                        // observed for the row detail) so the row is
                        // visible to `routectl usage` as a non-ok
                        // outcome instead of being mislabeled as a
                        // bare client disconnect. The send-failure
                        // result is intentionally discarded: if the
                        // client already disconnected, the row is
                        // already finalized.
                        tracing::error!(error = ?e, "ingress chunk render failed");
                        let render_err = routectl_core::Error::Streaming(e.to_string());
                        let safe_msg = sanitize_stream_error_for_client(&render_err);
                        let class = crate::ingress::StreamErrorClass::from_error(&render_err);
                        capture.observe_error(&render_err);
                        capture.finalize(Outcome::UpstreamError);
                        for ev in adapter.render_error_eos(state.as_mut(), &safe_msg, &class) {
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
                // EOS distinction and the rationale). A mid-stream error
                // is a definite upstream fault, so finalize the row as
                // `upstream_error` regardless of whether the terminal
                // event reaches the client.
                let safe_msg = sanitize_stream_error_for_client(&e);
                let class = crate::ingress::StreamErrorClass::from_error(&e);
                // Log the sanitized client-facing detail and the error
                // class -- NOT `?e`, whose `Error::Upstream` Debug embeds
                // the raw upstream body (now the full `{error:...}`
                // envelope for structured errors). The full raw body
                // remains available at DEBUG via `debug_upstream_error_body`
                // on the egress side.
                tracing::error!(
                    detail = %safe_msg,
                    class = ?class,
                    "upstream stream error -- emitting terminal error event"
                );
                capture.observe_error(&e);
                capture.finalize(Outcome::UpstreamError);
                for ev in adapter.render_error_eos(state.as_mut(), &safe_msg, &class) {
                    if tx.send(ev).await.is_err() {
                        // Client disconnected before we could deliver the
                        // terminal error event. The row is already
                        // finalized; Drop is a no-op.
                        return;
                    }
                }
                return;
            }
        }
    }
    for ev in adapter.render_eos(state.as_mut()) {
        if tx.send(ev).await.is_err() {
            // Client disconnected during EOS render. The guard stays
            // un-finalized, so Drop stamps `client_disconnect`.
            return;
        }
    }
    // Natural EOS reached. Finalize the row as a clean completion; the
    // egress trace-summary fires from inside `finalize`.
    capture.finalize(Outcome::Ok);
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
/// Operators reading routectl logs still see the sanitized detail and
/// error class on the same path's ERROR line; the full raw upstream
/// body is available at DEBUG via `debug_upstream_error_body` on the
/// egress side -- the wire bytes are the only place this short summary
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
        Error::Internal(_) => {
            tracing::error!(error = %e, "internal error suppressed in HTTP response");
            "internal error".to_string()
        }
        // The `Error::Upstream` Display string embeds the internal
        // provider config section name (routing topology) and the raw
        // upstream body (now the full `{error:...}` envelope for
        // structured errors, which can also carry per-tenant rate-limit
        // detail and upstream-side metadata). Log only safe structured
        // fields server-side -- never the raw body via Display/Debug; the
        // full body remains available at DEBUG via
        // `debug_upstream_error_body` on the egress side. Return only the
        // HTTP status plus the upstream's own top-level `error.message` /
        // `error.type` when the body parsed as JSON -- mirroring the
        // streaming path's discipline.
        Error::Upstream {
            status,
            body,
            provider,
            upstream_type,
            ..
        } => {
            tracing::error!(
                provider = %provider,
                status = *status,
                upstream_type = ?upstream_type,
                "upstream error sanitized in HTTP response"
            );
            sanitize_upstream_for_client(*status, body)
        }
        _ => e.to_string(),
    };
    // Lift the upstream classifier so an SDK that branches on
    // `error.type` / `error.code` keeps the upstream signal instead of
    // a generic collapse. Only `Error::Upstream` carries these; every
    // other variant uses the static `type_str` mapping unchanged.
    let (upstream_type, upstream_code) = match &e {
        Error::Upstream {
            upstream_type,
            upstream_code,
            ..
        } => (upstream_type.as_deref(), upstream_code.as_deref()),
        _ => (None, None),
    };
    error_response(
        shape,
        status,
        type_str,
        &public_message,
        type_str,
        upstream_type,
        upstream_code,
    )
}

/// Build a client-safe message for an `Error::Upstream`. STRIPS the
/// internal provider config section name and the raw upstream response
/// body. When the body parsed as JSON with a top-level `error.message`
/// (or, failing that, `error.type`), surface that short upstream-authored
/// classifier alongside the HTTP status; otherwise surface the status
/// alone.
///
/// Counterpart to `sanitize_stream_error_for_client` on the streaming
/// path: both refuse to forward the provider name or the raw body. The
/// non-streaming path can afford the upstream's own top-level
/// `error.message` because the egress already parsed it from the wire,
/// but never a sibling key or the raw dump.
fn sanitize_upstream_for_client(status: u16, body: &str) -> String {
    if let Some(detail) = upstream_error_detail(body) {
        format!("upstream error (HTTP {status}): {detail}")
    } else {
        format!("upstream error (HTTP {status})")
    }
}

/// Extract the upstream's own top-level `error.message` (preferred) or
/// `error.type` from a JSON error body. Returns `None` for non-JSON
/// bodies or JSON without a top-level `error` object carrying one of
/// those string fields, so the caller falls back to a status-only
/// message rather than leaking sibling keys or the raw body.
fn upstream_error_detail(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let err_obj = parsed.get("error")?;
    err_obj
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| err_obj.get("type").and_then(Value::as_str))
        .map(str::to_string)
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
        Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        Error::NotImplemented(_, _) => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

/// Build a dialect-correct error envelope.
///
/// `err_type` / `code` are the routectl-internal static classifiers
/// from `error_status_and_type`. `upstream_type` / `upstream_code`, when
/// present, carry the upstream's own `error.type` / `error.code`
/// (populated only for `Error::Upstream`) so an SDK that branches on the
/// classifier keeps the upstream signal:
///
/// - OpenAI envelope: `error.type` becomes the upstream type (falling
///   back to `err_type`); `error.code` becomes the upstream code,
///   falling back to the upstream type, then to `code`.
/// - Anthropic envelope: a captured upstream type that is already a
///   valid Anthropic-vocabulary member passes through verbatim;
///   otherwise the status-derived guess in `anthropic_error_type` wins.
#[allow(clippy::too_many_arguments)]
fn error_response(
    shape: ErrorEnvelopeShape,
    status: StatusCode,
    err_type: &str,
    message: &str,
    code: &str,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
) -> Response {
    let body: Value = match shape {
        ErrorEnvelopeShape::OpenAi => {
            // Prefer the upstream classifier; fall back to the generic
            // routectl tags when the upstream sent none.
            let out_type = upstream_type.unwrap_or(err_type);
            let out_code = upstream_code.or(upstream_type).unwrap_or(code);
            json!({
                "error": {
                    "message": message,
                    "type": out_type,
                    "code": out_code,
                }
            })
        }
        ErrorEnvelopeShape::Anthropic => json!({
            "type": "error",
            "error": {
                "type": anthropic_error_type(err_type, status, upstream_type),
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
/// `permission_error`, `not_found_error`, `request_too_large`,
/// `rate_limit_error`, `api_error`, `overloaded_error`. routectl's tags
/// are richer (e.g. `validation_error`, `payload_too_large`,
/// `unsupported_media_type`); collapse them to the closest Anthropic
/// equivalent so claude-code's per-`error.type` handling fires
/// correctly.
///
/// When the upstream supplied its own `error.type` and it is already a
/// valid Anthropic-vocabulary member, prefer it verbatim over the
/// status-derived guess so stream + non-stream agree and the upstream
/// signal survives. Otherwise the status table decides: `upstream_error`
/// at 401/403/413 maps to `authentication_error` / `permission_error` /
/// `request_too_large`, 429 to `rate_limit_error`, 503/529 to
/// `overloaded_error`, and everything else falls back to `api_error`.
pub(crate) fn anthropic_error_type(
    err_type: &str,
    status: StatusCode,
    upstream_type: Option<&str>,
) -> &'static str {
    if let Some(member) = upstream_type.and_then(anthropic_vocab_member) {
        return member;
    }
    match (err_type, status.as_u16()) {
        ("unknown_alias", _) | ("unknown_provider", _) => "not_found_error",
        ("bad_request", _)
        | ("validation_error", _)
        | ("payload_too_large", _)
        | ("unsupported_media_type", _) => "invalid_request_error",
        ("auth_error", _) | ("authentication_error", _) => "authentication_error",
        ("upstream_error", 401) => "authentication_error",
        ("upstream_error", 403) => "permission_error",
        ("upstream_error", 413) => "request_too_large",
        ("upstream_error", 429) => "rate_limit_error",
        ("upstream_error", 503) | ("upstream_error", 529) => "overloaded_error",
        ("upstream_error", _) | ("streaming_error", _) | ("bad_gateway", _) => "api_error",
        (_, _) => "api_error",
    }
}

/// Return the static Anthropic-vocabulary spelling of `t` when `t` is
/// already a valid Anthropic `error.type` member, else `None`. Used to
/// pass an upstream-supplied type through verbatim while still yielding
/// a `&'static str` for the envelope.
fn anthropic_vocab_member(t: &str) -> Option<&'static str> {
    match t {
        "invalid_request_error" => Some("invalid_request_error"),
        "authentication_error" => Some("authentication_error"),
        "permission_error" => Some("permission_error"),
        "not_found_error" => Some("not_found_error"),
        "request_too_large" => Some("request_too_large"),
        "rate_limit_error" => Some("rate_limit_error"),
        "api_error" => Some("api_error"),
        "overloaded_error" => Some("overloaded_error"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "ingress_handle_tests.rs"]
mod tests;
