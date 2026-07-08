//! Generic ingress driver: parses one HTTP body via an `IngressAdapter`,
//! routes to `Router::complete_with_options` / `stream_with_options`,
//! and renders the response/chunks via the same adapter.
//!
//! Both `/v1/chat/completions` (OpenAI) and `/v1/messages` (Anthropic)
//! handlers delegate here. The only difference between the two is the
//! adapter passed in.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use routectl_core::Error;
use routectl_core::ForwardedBearer;
use routectl_router::config::CredentialSource;
use routectl_router::{DispatchedStream, RouterOptions};
use routectl_usage::{Outcome, UsageHandle, UsageRecord};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tracing::Instrument;

use crate::handlers::pure_proxy_metrics::{PureProxyRejectionReason, record_rejection};
use crate::handlers::usage_capture::{
    StreamStage, UsageCapture, build_usage_draft, outcome_for_dispatch_err,
};
use crate::ingress::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamRequestContext,
    token_estimate::estimate_input_tokens,
};
use crate::server::AppState;
use crate::server::request_id::RequestId;

const DISABLE_FALLBACKS_HEADER: &str = "x-routectl-disable-fallbacks";

/// Inbound header that claude-code stamps with its logical session id.
/// Captured best-effort for the usage row's `session_id`; absent or
/// non-UTF-8 values yield `None`.
const SESSION_ID_HEADER: &str = "x-claude-code-session-id";

/// Grace window the streaming handler holds a dispatch un-flushed before it
/// commits the SSE `Response` and goes warm-hold. This is a flush-timing
/// backstop, NOT a failover threshold: on expiry the handler flushes the
/// synthetic early frame as the first body byte and KEEPS waiting on the
/// SAME dispatch (flush-and-continue), it never aborts or fails the request
/// over. Sized well above typical fast-error latency (sub-second, so a fast
/// chain-exhaustion still resolves inside grace and keeps its real HTTP
/// status + the SDK's pre-stream 529 retry) and far below the client-side
/// ~300s headers wall. Note p50 first-token is several seconds, so most
/// HEALTHY streams hit grace and go warm-hold -- the intended common path.
const STREAM_EARLY_FLUSH_GRACE: Duration = Duration::from_millis(2500);

/// The dispatch future held un-awaited across the early-flush grace window.
/// Boxed as a `'static` trait object (rather than borrowing the router) so
/// the grace-expiry branch can MOVE the still-pending future into the
/// spawned warm render task.
type DispatchFut = Pin<Box<dyn Future<Output = DispatchedStream> + Send>>;

pub async fn ingress_handle<A: IngressAdapter + 'static>(
    state: Arc<AppState>,
    headers: HeaderMap,
    request_id: Option<RequestId>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
    adapter: A,
) -> Response {
    let envelope = adapter.error_envelope_shape();

    // Snapshot the live Router once per request so a hot-swap mid-
    // request does not mix old + new routing state. Every read after
    // this point goes through `router`, not `state.router.load*`.
    let router = state.router.load_full();

    // Forwarded-mode (pure-proxy) admission gate: reject an invalid
    // forwarded request at the ingress boundary BEFORE body parse and
    // dispatch. This is the SINGLE shared admission point for all three
    // dialects -- `messages` (Anthropic), `chat_completions` (OpenAI), and
    // `responses` all funnel through here -- so the non-Anthropic-dialect
    // check reaches the OpenAI + Responses adapters too, not only the
    // Anthropic one. A no-op in own mode: every non-forwarded request is
    // byte-identical to the pre-passthrough path.
    if let Some(resp) = enforce_pure_proxy_admission(&headers, &router, envelope) {
        return resp;
    }

    let Json(raw_body) = match body {
        Ok(b) => b,
        Err(e) => return render_json_rejection(envelope, e),
    };

    let mut req = match adapter.parse_request(&headers, raw_body) {
        Ok(r) => r,
        Err(e) => return map_error(envelope, e),
    };

    // Forwarded-mode capture gate (first-party passthrough): stash the
    // inbound bearer for opt-in relay to the upstream ONLY when the MITM
    // seam header is present (header-is-a-hint) AND the resolved
    // credential_source is Forwarded (config-is-the-capability). Every
    // own-mode and non-forwarded path leaves `forwarded_bearer` None, so
    // the carrier state is byte-identical to the pre-passthrough path.
    capture_forwarded_bearer(&headers, &router, &mut req);

    // Same forwarded-mode gate: capture the client's inbound `x-stainless-*`
    // SDK fingerprint headers so the Anthropic-API egress can present the
    // client's real identity on the forwarded leg (overriding the minted
    // cloak fingerprint). Own mode leaves `stainless_headers` empty.
    capture_stainless_headers(&headers, &router, &mut req);

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

/// Populate `req.routectl_internal.forwarded_bearer` with the inbound
/// `Authorization` bearer ONLY when BOTH keys of the two-key gate turn:
///
/// - (a) header-is-a-hint: the `x-routectl-mitm-proxied` seam header is
///   present. The MITM front-proxy (`proxy::split`) stamps it exclusively
///   on the re-injected api.anthropic.com inference leg, so its presence
///   marks a request that arrived through that channel; and
/// - (b) config-is-the-capability: the resolved `[mitm] credential_source`
///   is `Forwarded`. An absent `[mitm]` block resolves to `None`, which is
///   never `Forwarded`.
///
/// When either key is missing -- own mode, no `[mitm]` block, no seam
/// header, or no inbound bearer -- `forwarded_bearer` stays `None` and the
/// carrier is byte-identical to the non-passthrough path. The raw token is
/// wrapped in `ForwardedBearer` (redact-on-Debug) the instant it is
/// captured, and this path emits no tracing, so the token is never logged.
fn capture_forwarded_bearer(
    headers: &HeaderMap,
    router: &routectl_router::Router,
    req: &mut routectl_core::ChatRequest,
) {
    if !forwarded_capture_armed(headers, router) {
        return;
    }
    if let Some(token) = extract_authorization_bearer(headers) {
        req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new(token));
    }
}

/// Header-name prefix (case-insensitive) of the Stainless SDK fingerprint
/// headers captured on the forwarded leg. Owns its own namespace,
/// disjoint from the `x-claude-code-*` set on `claude_code_headers`.
const STAINLESS_HEADER_PREFIX: &str = "x-stainless-";

/// Populate `req.routectl_internal.stainless_headers` with the inbound
/// `x-stainless-*` SDK fingerprint headers, under the SAME two-key gate
/// as [`capture_forwarded_bearer`] (seam header present AND resolved
/// `[mitm] credential_source == Forwarded`). On the forwarded (pure-proxy)
/// leg the Anthropic-API egress presents the client's real identity, so
/// these client-supplied Stainless headers override routectl's minted
/// cloak fingerprint downstream.
///
/// A SEPARATE carrier from `claude_code_headers` on purpose: that field is
/// contractually `x-claude-code-*`-only. These are NON-secret fingerprint
/// values (no redaction). When either gate key is missing -- own mode, no
/// `[mitm]` block, or no seam header -- `stainless_headers` stays empty and
/// the carrier is byte-identical to the non-passthrough path. Non-UTF-8
/// values are skipped; capture order mirrors inbound order.
fn capture_stainless_headers(
    headers: &HeaderMap,
    router: &routectl_router::Router,
    req: &mut routectl_core::ChatRequest,
) {
    if !forwarded_capture_armed(headers, router) {
        return;
    }
    for (name, val) in headers {
        if !name
            .as_str()
            .to_ascii_lowercase()
            .starts_with(STAINLESS_HEADER_PREFIX)
        {
            continue;
        }
        let Ok(v) = val.to_str() else { continue };
        req.routectl_internal
            .stainless_headers
            .push((name.as_str().to_string(), v.to_string()));
    }
}

/// The two-key forwarded-capture gate shared by `capture_forwarded_bearer`
/// and `capture_stainless_headers`:
///
/// - (a) header-is-a-hint: the `x-routectl-mitm-proxied` seam header is
///   present (the MITM front-proxy stamps it only on the re-injected
///   api.anthropic.com inference leg); AND
/// - (b) config-is-the-capability: the resolved `[mitm] credential_source`
///   is `Forwarded` (an absent `[mitm]` block resolves to `None`, which is
///   never `Forwarded`).
///
/// `false` in own mode and on every non-forwarded path, so both capture
/// sites leave their carriers byte-identical to the pre-passthrough state.
fn forwarded_capture_armed(headers: &HeaderMap, router: &routectl_router::Router) -> bool {
    if !headers.contains_key(crate::ingress::MITM_PROXIED_HEADER) {
        return false;
    }
    matches!(
        router.config.mitm.as_ref().map(|m| m.credential_source),
        Some(CredentialSource::Forwarded)
    )
}

/// Extract the token from an inbound `Authorization: Bearer <token>`
/// header. Returns the trimmed token (scheme stripped) when the header is
/// present, valid UTF-8, and carries a non-empty `Bearer` credential;
/// `None` for an absent / non-UTF-8 header, a non-`Bearer` scheme, or an
/// empty token. The scheme match is ASCII-case-insensitive per RFC 7235.
/// Pure -- no config access, no logging; the caller gates on config before
/// wrapping the result in a redacting newtype.
fn extract_authorization_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?
        .trim();
    let (scheme, token) = raw.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// Forwarded-mode (pure-proxy) ingress admission gate. Runs the
/// decision-doc Section 6 rejection matrix and, on a rejection, records the
/// `pure_proxy_rejections_total{reason}` counter + the structured rejection
/// log, then returns the dialect-correct error `Response`. Returns `None`
/// (admit) for own mode and for a well-formed forwarded request.
///
/// The dialect is read straight off `envelope`: the Anthropic ingress uses
/// `ErrorEnvelopeShape::Anthropic`; the OpenAI chat-completions and Responses
/// ingresses both use `ErrorEnvelopeShape::OpenAi`. That is the signal the
/// non-Anthropic-dialect check keys on, so gating here (the shared driver all
/// three handlers funnel through) covers every dialect at one point.
fn enforce_pure_proxy_admission(
    headers: &HeaderMap,
    router: &routectl_router::Router,
    envelope: ErrorEnvelopeShape,
) -> Option<Response> {
    let forwarded = matches!(
        router.config.mitm.as_ref().map(|m| m.credential_source),
        Some(CredentialSource::Forwarded)
    );
    let is_anthropic_dialect = envelope == ErrorEnvelopeShape::Anthropic;
    let seam_present = headers.contains_key(crate::ingress::MITM_PROXIED_HEADER);
    let has_bearer = extract_authorization_bearer(headers).is_some();
    let has_session_id = session_id_of(headers).is_some();

    let reason = classify_pure_proxy_rejection(PureProxyAdmissionInputs {
        forwarded,
        is_anthropic_dialect,
        seam_present,
        has_bearer,
        has_session_id,
    })?;
    // SAFE dimensions only: `has_session_id` is the boolean the log carries;
    // the token itself is never touched here (or captured, on this path).
    record_rejection(reason, has_session_id);
    Some(render_pure_proxy_rejection(envelope, reason))
}

/// The SAFE, request-derived facts the forwarded-mode admission matrix
/// decides on. Booleans only -- never a token, header, or body value -- so
/// the decision core cannot depend on (or leak) request content.
#[derive(Debug, Clone, Copy)]
struct PureProxyAdmissionInputs {
    /// `[mitm] credential_source == Forwarded` (config-is-the-capability).
    forwarded: bool,
    /// The request arrived on the Anthropic dialect (`/v1/messages`); false
    /// for the OpenAI chat-completions and Responses dialects.
    is_anthropic_dialect: bool,
    /// The `x-routectl-mitm-proxied` seam header is present
    /// (header-is-a-hint: it arrived through the f1 MITM inference leg).
    seam_present: bool,
    /// A usable inbound `Authorization` bearer is present.
    has_bearer: bool,
    /// The `x-claude-code-session-id` identity header is present.
    has_session_id: bool,
}

/// Pure decision core for the forwarded-mode admission matrix. Returns the
/// rejection reason, or `None` to admit. Own mode (`!forwarded`) ALWAYS
/// admits -- none of these checks fire -- so a non-forwarded request stays
/// byte-identical to the pre-passthrough path.
///
/// Precedence (only when `forwarded`):
/// 1. non-Anthropic dialect -> `NonAnthropicDialect`. The dialect itself is
///    disqualifying, independent of the seam header.
/// 2. Anthropic dialect, seam header ABSENT -> `NotMitm` (a direct :9100
///    loopback client -- not a valid pure-proxy path).
/// 3. Anthropic dialect, seam header PRESENT, no bearer -> `TokenMissing`
///    (CC not logged into claude.ai). Checked BEFORE the session id so a
///    request missing both surfaces the more fundamental missing credential.
/// 4. Anthropic dialect, seam header PRESENT, bearer present, no session
///    id -> `IdentityMissing` (fail before minting identity).
const fn classify_pure_proxy_rejection(
    inputs: PureProxyAdmissionInputs,
) -> Option<PureProxyRejectionReason> {
    if !inputs.forwarded {
        return None;
    }
    if !inputs.is_anthropic_dialect {
        return Some(PureProxyRejectionReason::NonAnthropicDialect);
    }
    if !inputs.seam_present {
        return Some(PureProxyRejectionReason::NotMitm);
    }
    if !inputs.has_bearer {
        return Some(PureProxyRejectionReason::TokenMissing);
    }
    if !inputs.has_session_id {
        return Some(PureProxyRejectionReason::IdentityMissing);
    }
    None
}

/// Build the dialect-correct error envelope for a forwarded-mode admission
/// rejection, reusing the shared `error_response` / `anthropic_error_type`
/// mapping (Anthropic envelope for the Anthropic path, OpenAI-shaped for the
/// OpenAI / Responses path). The client message carries the safe `reason=`
/// tag -- never the token or any request-derived value.
fn render_pure_proxy_rejection(
    shape: ErrorEnvelopeShape,
    reason: PureProxyRejectionReason,
) -> Response {
    let status = reason.status();
    // Route the internal err_type through the same status -> vocab table the
    // rest of the ingress uses: a 401 becomes `authentication_error`, a 400
    // becomes `invalid_request_error` on the Anthropic path; the OpenAI shape
    // surfaces the tag verbatim.
    let err_type = if status == StatusCode::UNAUTHORIZED {
        "authentication_error"
    } else {
        "bad_request"
    };
    let message = pure_proxy_rejection_message(reason);
    error_response(shape, status, err_type, &message, err_type, None, None)
}

/// Operator-actionable, token-free client message per rejection reason. Each
/// carries the safe `reason=<...>` tag so an SDK / operator can branch on it
/// without parsing prose.
fn pure_proxy_rejection_message(reason: PureProxyRejectionReason) -> String {
    let detail = match reason {
        PureProxyRejectionReason::TokenMissing => {
            "forwarded (pure-proxy) mode requires an inbound Authorization \
             bearer; log Claude Code into claude.ai"
        }
        PureProxyRejectionReason::NotMitm => {
            "forwarded (pure-proxy) mode accepts Anthropic-dialect requests \
             only through the MITM proxy"
        }
        PureProxyRejectionReason::IdentityMissing => {
            "forwarded (pure-proxy) mode requires the x-claude-code-session-id \
             identity header"
        }
        PureProxyRejectionReason::NonAnthropicDialect => {
            "forwarded (pure-proxy) mode supports the Anthropic dialect only"
        }
    };
    format!("{detail} (reason={})", reason.as_str())
}

/// Treat a header value as truthy when set to "1", "true", or "yes"
/// (case-insensitive). Absent or empty headers are false.
fn header_truthy(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
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
    // Extract the canonical live session key BEFORE dispatch moves `req`
    // into the router. POST-response K-sample recording (below) keys on
    // this value, NOT the header-only usage `session_id`.
    let session_key = req.routectl_internal.inbound_session_key.clone();
    let dispatched = router.complete_with_options(req, opts).await;
    capture.observe_meta(&dispatched.meta);
    match dispatched.result {
        Ok(resp) => {
            // Non-streaming first byte == the response being ready.
            capture.mark_first_byte();
            capture.observe_response(&resp);
            // Best-effort, post-response: record the observed cache-reuse
            // into the per-session K-estimator store. Never affects the
            // response; a keyless / no-served-target request is skipped
            // inside the helper.
            capture.record_k_sample(&router, session_key.as_deref());
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
    let capture = UsageCapture::new(draft, usage, adapter.id().to_string());
    // Extract the canonical live session key BEFORE dispatch moves `req`
    // into the router; it rides into the render task for POST-response
    // K-sample recording at natural end-of-stream.
    let session_key = req.routectl_internal.inbound_session_key.clone();
    // Build the stream-state seed from `req` BEFORE dispatch moves it:
    // the local input-token estimate (for a non-zero early
    // `message_start.usage.input_tokens`) and the resolved model (for the
    // early-frame model id). Adapters that need neither ignore it.
    let stream_ctx = StreamRequestContext {
        input_tokens_estimate: estimate_input_tokens(&req),
        model: req.model.clone(),
    };
    // Hold the dispatch UN-AWAITED. `stream_with_options` borrows `&self`
    // for the returned future's life, so an `Arc` clone is MOVED into the
    // async block to make the future `'static` -- required because the
    // grace-expiry branch moves it into the spawned warm render task. The
    // original `router` Arc stays available for the render task's K-sample
    // recording.
    let router_for_dispatch = Arc::clone(&router);
    let fut: DispatchFut =
        Box::pin(async move { router_for_dispatch.stream_with_options(req, opts).await });
    stream_dispatch_gated(fut, adapter, capture, router, session_key, stream_ctx).await
}

/// Grace-gated commit (option (b')): hold the dispatch future for a bounded
/// grace window (`STREAM_EARLY_FLUSH_GRACE`) via `tokio::select!`, then
/// branch WITHOUT ever awaiting the dispatch to completion up front.
///
/// FAST (dispatch resolves within grace) -- today's behavior verbatim:
/// - `Ok(stream)` spawns the render task on the resolved stream (the
///   synthetic `message_start` still emits on the first content chunk,
///   carrying the estimate; no early frame).
/// - `Err(e)` returns a REAL HTTP error status via `map_error` -- NOT an
///   in-stream frame -- so the SDK's pre-stream 529/5xx retry still fires.
///
/// GRACE-EXPIRY (dispatch still pending) -- commit the SSE `Response` now
/// and hand the still-pending future to a WARM render task, which flushes
/// the dialect early frame as the first body byte BEFORE awaiting the
/// dispatch (`warm_render_task`). Grace never aborts or fails over; it only
/// decides WHEN to flush.
async fn stream_dispatch_gated<A: IngressAdapter + 'static>(
    mut fut: DispatchFut,
    adapter: A,
    mut capture: UsageCapture,
    router: Arc<routectl_router::Router>,
    session_key: Option<String>,
    stream_ctx: StreamRequestContext,
) -> Response {
    let envelope = adapter.error_envelope_shape();
    let egress_id = adapter.id().to_string();

    // Inner channel carries our `SseEvent` type so the rendering loop is
    // straightforward to unit-test (drain a `mpsc::Receiver<SseEvent>` and
    // assert on event names + payload bytes). Conversion to
    // `axum::response::sse::Event` happens only on the production path in
    // `build_sse_response`.
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(64);
    // Capture the current tracing span (carrying request_id from the
    // request_id middleware + ingress / router / provider span hierarchy)
    // and attach it to whichever render task is spawned, so any
    // tracing::error! from the task keeps its request_id correlation.
    let parent_span = tracing::Span::current();

    // `biased` polls the dispatch future FIRST: a dispatch that is already
    // ready takes the fast path deterministically; grace only matters when
    // the dispatch is genuinely still pending.
    let fast = tokio::select! {
        biased;
        dispatched = &mut fut => Some(dispatched),
        () = tokio::time::sleep(STREAM_EARLY_FLUSH_GRACE) => None,
    };

    match fast {
        Some(dispatched) => {
            capture.observe_meta(&dispatched.meta);
            match dispatched.result {
                Ok(upstream) => {
                    // `adapter` + `capture` move INTO the render task, which
                    // finalizes on every stream exit (see `drive_stream`).
                    tokio::spawn(
                        render_stream_task(
                            upstream,
                            adapter,
                            capture,
                            tx,
                            router,
                            session_key,
                            stream_ctx,
                        )
                        .instrument(parent_span),
                    );
                    build_sse_response(rx, &egress_id)
                }
                Err(e) => {
                    // Stream never started AND grace has not elapsed: classify
                    // off the meta + error and return a REAL HTTP error status
                    // (preserves the SDK 529 retry). No SSE response is
                    // committed -- this is NOT an in-stream frame.
                    capture.observe_error(&e);
                    capture.finalize(outcome_for_dispatch_err(&dispatched.meta));
                    map_error(envelope, e)
                }
            }
        }
        None => {
            // Grace expired with the dispatch still pending. Commit the SSE
            // response now; the warm task owns the still-pending future and
            // flushes the early frame before awaiting it.
            tokio::spawn(
                warm_render_task(fut, adapter, capture, tx, router, session_key, stream_ctx)
                    .instrument(parent_span),
            );
            build_sse_response(rx, &egress_id)
        }
    }
}

/// Wrap the render task's `SseEvent` channel into the axum SSE `Response`.
/// The conversion to `axum::response::sse::Event` is a one-liner `.map()`
/// on the `ReceiverStream`; `KeepAlive` emits the comment heartbeat that
/// keeps a warm-hold connection alive while the upstream prefill runs.
fn build_sse_response(rx: tokio::sync::mpsc::Receiver<SseEvent>, egress_id: &str) -> Response {
    let receiver_stream =
        ReceiverStream::new(rx).map(|ev| Ok::<Event, std::convert::Infallible>(sse_to_axum(ev)));
    let resp = Sse::new(receiver_stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response();
    // Dir 4 (streaming egress): capture the SSE response headers
    // (content-type: text/event-stream, keep-alive, ...) before returning.
    trace_egress_headers_of(egress_id, &resp);
    resp
}

/// Warm-hold render task (grace expired with the dispatch still pending).
///
/// For a dialect that OVERRIDES `early_frame` (Anthropic: emits
/// `message_start`), emit-then-dispatch is a hard invariant -- that frame
/// MUST flush as the FIRST body byte BEFORE awaiting the dispatch: the spike
/// proved a first body-stream poll that blocks on the dispatch never flushes
/// the response head, so the client headers wall is not defeated. A dialect
/// that keeps the default no-op `early_frame` (OpenAI) flushes nothing here;
/// it instead relies on axum's `KeepAlive` comment frame as its flush
/// trigger once the SSE response is polled.
///
/// After the (possibly empty) flush it awaits the SAME still-pending dispatch
/// future:
/// - `Ok(stream)`: drive it through the shared `drive_stream` loop. The
///   early Anthropic `message_start` already set `state.started`, so the
///   first content chunk dedups it (no duplicate `message_start`).
/// - `Err(e)`: the SSE head is already committed, so the HTTP status is
///   gone -- surface exactly ONE terminal in-stream error via
///   `render_error_eos`, tagged with the pre-content stage marker so the
///   ledger does not collapse it with a mid-stream cut.
///
/// `observe_meta` + the dispatch-error finalize live HERE (not in the
/// handler) because the future resolves inside this task. The Drop
/// `client_disconnect` fallback is reserved for a genuine hangup -- the only
/// un-finalized exit is a `tx.send` failure before the dispatch resolved
/// (the client vanished before we could flush the head).
async fn warm_render_task<A: IngressAdapter>(
    fut: DispatchFut,
    adapter: A,
    mut capture: UsageCapture,
    tx: tokio::sync::mpsc::Sender<SseEvent>,
    router: Arc<routectl_router::Router>,
    session_key: Option<String>,
    stream_ctx: StreamRequestContext,
) {
    let mut state: Box<dyn IngressStreamState> = adapter.new_stream_state(&stream_ctx);
    // Emit-then-dispatch invariant: flush the early frame FIRST, before the
    // dispatch await, so the response head actually flushes.
    for ev in adapter.early_frame(state.as_mut()) {
        if tx.send(ev).await.is_err() {
            // Client vanished before the head flushed -- a genuine
            // disconnect. Leave the guard un-finalized so Drop stamps
            // `client_disconnect`.
            return;
        }
    }
    // Now await the SAME dispatch that outran the grace window.
    let dispatched = fut.await;
    capture.observe_meta(&dispatched.meta);
    match dispatched.result {
        Ok(upstream) => {
            drive_stream(upstream, adapter, capture, tx, router, session_key, state).await;
        }
        Err(e) => {
            // Pre-content dispatch failure AFTER the SSE head committed: the
            // HTTP status is gone, so surface exactly ONE terminal in-stream
            // error. Finalize the row as `UpstreamError` with the pre-content
            // stage marker so the ledger keeps it distinct from a mid-stream
            // cut; reserve the Drop `client_disconnect` for real cancellation.
            capture.observe_error(&e);
            capture.mark_stream_stage(StreamStage::PreContentDispatch);
            capture.finalize(Outcome::UpstreamError);
            let safe_msg = sanitize_stream_error_for_client(&e);
            let class = crate::ingress::StreamErrorClass::from_error(&e);
            for ev in adapter.render_error_eos(state.as_mut(), &safe_msg, &class) {
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Fast-path render task: build the initial stream state and drive the
/// resolved upstream through the shared `drive_stream` loop. Used when the
/// dispatch resolved WITHIN the early-flush grace window (no early frame --
/// the synthetic `message_start` emits on the first content chunk as
/// before). The warm-hold path (`warm_render_task`) shares `drive_stream`
/// but pre-flushes the early frame and owns the still-pending future.
async fn render_stream_task<A: IngressAdapter>(
    upstream: futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    adapter: A,
    capture: UsageCapture,
    tx: tokio::sync::mpsc::Sender<SseEvent>,
    router: Arc<routectl_router::Router>,
    session_key: Option<String>,
    stream_ctx: StreamRequestContext,
) {
    let state: Box<dyn IngressStreamState> = adapter.new_stream_state(&stream_ctx);
    drive_stream(upstream, adapter, capture, tx, router, session_key, state).await;
}

/// Drive the upstream chunk stream through the ingress adapter, emitting
/// one `SseEvent` per produced wire event. `state` is pre-built by the
/// caller so the warm-hold path can seed it with an already-emitted early
/// frame (dedup) before handing it here. Exit paths:
///
/// 1. Upstream finishes naturally -> emit `render_eos` events,
///    mark the egress summary `clean_close=true` so the Drop summary
///    reports the observed `finish_reason`.
/// 2. Upstream errors mid-stream -> emit `render_error_eos` events
///    (the dialect-specific terminal ERROR event), tag the row with the
///    `MidStream` stage marker, then return. The summary Drop synthesizes
///    `finish_reason="truncated"` via the inverse-flag pattern -- the
///    upstream stream WAS truncated even though we now signal it cleanly
///    to the client.
/// 3. Render failure (canonical chunk that the adapter cannot turn
///    into wire events) -> log + emit the terminal error event + return.
/// 4. Client disconnects (channel send returns Err) -> return.
///    Drop emits truncated.
///
/// Extracted so the streaming-error path is unit-testable without spinning
/// up the axum layer: a test can build a synthesized
/// `BoxStream<Result<ChatChunk>>` (e.g. one Ok chunk followed by one Err)
/// and drain the resulting `mpsc::Receiver<SseEvent>` to assert on the wire
/// shape of the terminal error event.
async fn drive_stream<A: IngressAdapter>(
    mut upstream: futures::stream::BoxStream<
        'static,
        routectl_core::Result<routectl_core::ChatChunk>,
    >,
    adapter: A,
    mut capture: UsageCapture,
    tx: tokio::sync::mpsc::Sender<SseEvent>,
    router: Arc<routectl_router::Router>,
    session_key: Option<String>,
    mut state: Box<dyn IngressStreamState>,
) {
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
                capture.mark_stream_stage(StreamStage::MidStream);
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
    // Natural EOS reached. Record the observed cache-reuse into the per-
    // session K-estimator store BEFORE finalizing -- best-effort, never
    // affects the bytes already sent. Then finalize the row as a clean
    // completion; the egress trace-summary fires from inside `finalize`.
    capture.record_k_sample(&router, session_key.as_deref());
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
        ("unknown_alias" | "unknown_provider", _) => "not_found_error",
        (
            "bad_request" | "validation_error" | "payload_too_large" | "unsupported_media_type",
            _,
        ) => "invalid_request_error",
        ("auth_error" | "authentication_error", _) => "authentication_error",
        ("upstream_error", 401) => "authentication_error",
        ("upstream_error", 403) => "permission_error",
        ("upstream_error", 413) => "request_too_large",
        ("upstream_error", 429) => "rate_limit_error",
        ("upstream_error", 503 | 529) => "overloaded_error",
        ("upstream_error" | "streaming_error" | "bad_gateway", _) => "api_error",
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
