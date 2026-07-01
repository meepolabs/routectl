//! `POST /v1/responses` handler. Thin wrapper around the generic
//! ingress driver with `ResponsesIngress`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;
use crate::server::request_id::RequestId;

#[tracing::instrument(skip_all, fields(ingress = "openai-responses"))]
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request_id: Option<Extension<RequestId>>,
    body: Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ingress = ResponsesIngress;
    ingress_handle(state, headers, request_id.map(|e| e.0), body, ingress).await
}
