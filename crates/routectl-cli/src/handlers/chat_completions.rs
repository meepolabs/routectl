use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use routectl_core::{ChatRequest, Error};
use routectl_router::RouterOptions;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::server::AppState;

const DISABLE_FALLBACKS_HEADER: &str = "x-routectl-disable-fallbacks";

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<ChatRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "bad_request",
                &e.to_string(),
                "invalid_request_error",
            );
        }
    };

    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = header_truthy(&headers, DISABLE_FALLBACKS_HEADER);

    let streaming = req.stream == Some(true);
    if streaming {
        stream_response(state, req, opts).await
    } else {
        complete_response(state, req, opts).await
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

async fn complete_response(
    state: Arc<AppState>,
    req: ChatRequest,
    opts: RouterOptions,
) -> Response {
    match state.router.complete_with_options(req, opts).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => map_error(e),
    }
}

async fn stream_response(state: Arc<AppState>, req: ChatRequest, opts: RouterOptions) -> Response {
    let stream_result = state.router.stream_with_options(req, opts).await;

    let upstream = match stream_result {
        Ok(s) => s,
        Err(e) => return map_error(e),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        let mut upstream = upstream;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    let data = match serde_json::to_string(&chunk) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = ?e, "failed to serialize chunk");
                            break;
                        }
                    };
                    if tx.send(Ok(Event::default().data(data))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "upstream stream error");
                    break;
                }
            }
        }
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
    });

    let receiver_stream = ReceiverStream::new(rx);
    Sse::new(receiver_stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn map_error(e: Error) -> Response {
    let (status, type_str) = error_status_and_type(&e);
    error_response(status, type_str, &e.to_string(), type_str)
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
