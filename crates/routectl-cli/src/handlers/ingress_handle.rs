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
                (StatusCode::OK, Json(body)).into_response()
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
mod tests {
    //! Pin the dispatch from `Error` -> envelope shape so the two
    //! ingress dialects render the right error wire shape. The
    //! integration tests in `crates/routectl-cli/tests/anthropic_ingress.rs`
    //! cover the end-to-end path through axum; these tests pin the
    //! pure mapping without needing a server.
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
        let err503 = Error::Upstream {
            provider: "p".into(),
            status: 503,
            body: "service unavailable".into(),
        };
        let resp = map_error(ErrorEnvelopeShape::Anthropic, err503);
        let status = resp.status();
        let body = body_to_value(resp).await;
        assert_eq!(status.as_u16(), 503);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "overloaded_error");

        // 529 -> overloaded_error
        let err529 = Error::Upstream {
            provider: "p".into(),
            status: 529,
            body: "anthropic overloaded".into(),
        };
        let resp = map_error(ErrorEnvelopeShape::Anthropic, err529);
        assert_eq!(resp.status().as_u16(), 529);
        let body = body_to_value(resp).await;
        assert_eq!(body["error"]["type"], "overloaded_error");

        // 502 -> api_error
        let err502 = Error::Upstream {
            provider: "p".into(),
            status: 502,
            body: "bad gateway".into(),
        };
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
}
