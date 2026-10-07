//! `POST /v1/responses` handler. Thin wrapper around the generic
//! ingress driver with `ResponsesIngress`.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

/// Bounded store backing `previous_response_id` chaining and retrieval.
use crate::server::request_id::RequestId;

#[tracing::instrument(skip_all, fields(ingress = "openai-responses"))]
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request_id: Option<Extension<RequestId>>,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let ingress = ResponsesIngress::with_store(state.responses_store.clone());
    ingress_handle(state, headers, request_id.map(|e| e.0), body, ingress).await
}

/// `GET /v1/responses/{response_id}` -- retrieve a stored response
/// object (the same shape the original request returned or streamed).
/// 404 with an OpenAI-shaped error envelope when the id is unknown or
/// evicted from the bounded store.
pub async fn retrieve(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(response_id): axum::extract::Path<String>,
) -> Response {
    use axum::response::IntoResponse;
    match state.responses_store.get(&response_id) {
        Some(v) => axum::Json(v).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "unknown_response",
                    "message": format!("no stored response with id `{response_id}`"),
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response(),
    }
}
