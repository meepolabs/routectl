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
    opts.disable_fallbacks = header_truthy(&headers, DISABLE_FALLBACKS_HEADER);

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
            Ok(body) => (StatusCode::OK, Json(body)).into_response(),
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
    tokio::spawn(
        async move {
            let mut upstream = upstream;
            let mut state: Box<dyn IngressStreamState> = adapter.new_stream_state();
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(chunk) => match adapter.render_chunk(chunk, state.as_mut()) {
                        Ok(events) => {
                            for ev in events {
                                if tx.send(Ok(sse_to_axum(ev))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "ingress chunk render failed");
                            return;
                        }
                    },
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
                        // non-clean termination.
                        tracing::error!(error = ?e, "upstream stream error -- terminating SSE without EOS sentinel");
                        return;
                    }
                }
            }
            for ev in adapter.render_eos(state.as_mut()) {
                if tx.send(Ok(sse_to_axum(ev))).await.is_err() {
                    return;
                }
            }
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
