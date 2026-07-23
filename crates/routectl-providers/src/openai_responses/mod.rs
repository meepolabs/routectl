//! OpenAI Responses API provider (`openai-responses` provider type).
//!
//! Three auth surfaces:
//!
//!   - `chatgpt-oauth` (default): ChatGPT subscription surface
//!     at `https://chatgpt.com/backend-api/codex`. Uses
//!     Authorization: Bearer `<jwt>` + ChatGPT-Account-Id + originator
//!     headers (codex parity). Fully wired: `complete()` + `stream()`
//!     both ship.
//!   - `api-key`: standard OpenAI surface at
//!     `https://api.openai.com/v1`. Uses Authorization: Bearer
//!     <api_key> only; `OpenAI-Organization` / `OpenAI-Project` can
//!     be set via `extra_headers` if needed.
//!   - `bedrock-mantle`: AWS Mantle proxy at
//!     `https://bedrock-mantle.<region>.api.aws/openai/v1`. Uses
//!     Authorization: Bearer `<bearer>` using the long-term Bedrock API
//!     key (resolved via api_key_ref, typically
//!     env://AWS_BEARER_TOKEN_BEDROCK).
//!
//! Wire shape: OpenAI Responses API.
//!   - Request reference: `codex-rs/codex-api/src/common.rs::
//!     ResponsesApiRequest`.
//!   - Reasoning replay: `codex-rs/app-server-protocol/schema/
//!     typescript/ResponseItem.ts` -- `{type:"reasoning",
//!     summary:[...], encrypted_content: string|null}`. Routectl
//!     emits empty `encrypted_content: ""` when no signature is
//!     present; codex's `arc_monitor.rs:325-336` treats empty as a
//!     no-op for replay so this is safe.
//!
//! Stream forcing: `complete()` always forces `stream:true` because
//! the chatgpt-oauth endpoint rejects `stream:false` with HTTP 400
//! `{"detail":"Stream must be set to true"}`. The public api-key
//! endpoint accepts both, but forcing uniformly keeps one code path.
//! Both auth surfaces share the same SSE drain extracting the
//! `response` field from `response.completed` or `response.incomplete`.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, debug_upstream_error_body,
    is_json_error_envelope, sanitize_for_log, sanitize_upstream_body, trace_outgoing_body,
    trace_upstream_success_body,
};

// Construction/config/auth-wiring types live in `client`; the test
// modules reach these through `use super::*`, so they are re-imported
// under `cfg(test)` to keep that glob surface intact without carrying
// unused imports into the non-test build.
#[cfg(test)]
use routectl_core::{StaticToken, TokenSource};
#[cfg(test)]
use std::sync::Arc;

mod client;
pub use client::{OpenAiResponsesConfig, OpenAiResponsesProvider};

pub(crate) mod auth;
pub(crate) mod cookies;
pub(crate) mod extras;
pub(crate) mod messages;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod response_types;
pub(crate) mod sse;
pub(crate) mod system;
pub(crate) mod tools;
pub(crate) mod types;

/// Format tag stamped on every reasoning_details entry emitted by the
/// Responses provider. Multi-turn callers echoing reasoning back must
/// see the same tag across the non-streaming + streaming paths so a
/// downstream ingress can differentiate the Responses shape from the
/// Anthropic shape (Anthropic carries `signature`, Responses carries
/// `encrypted_content`).
pub(crate) const OPENAI_RESPONSES_FORMAT: &str = "openai-responses-v1";

/// Provider-kind discriminator string used in tracing fields. See
/// the openai_compat module for the rationale.
const PROVIDER_KIND: &str = "openai-responses";

/// How the provider authenticates to the Responses API.
///
/// Kebab-case on the TOML wire so config writes look natural:
///   `auth_kind = "chatgpt-oauth"`
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// ChatGPT subscription via OAuth bearer JWT. Default.
    #[default]
    ChatgptOauth,
    /// Standard OpenAI API key against `api.openai.com/v1/responses`.
    ApiKey,
    /// AWS Bedrock Mantle proxy (OpenAI-shape):
    /// `Authorization: Bearer <bearer>` using the long-term Bedrock API
    /// key (resolved via api_key_ref, typically
    /// env://AWS_BEARER_TOKEN_BEDROCK).
    BedrockMantle,
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        let r = request::translate(&self.cfg, req)?;
        serde_json::to_value(&r).map_err(|e| Error::normalize_request(&self.cfg.id, e.to_string()))
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        let typed: response_types::ResponsesResponse = serde_json::from_value(raw)
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;
        response::translate(&self.cfg.id, typed)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        // The chatgpt-oauth Responses endpoint is stream-only: it returns
        // HTTP 400 {"detail":"Stream must be set to true"} when stream=false.
        // We implement complete() by forcing stream=true, consuming the SSE
        // until the `response.completed` event fires (which carries the full
        // ResponsesResponse body), then translating that body to ChatResponse.
        // Confirmed stream-only behavior: smoke 2026-05-12.
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution: for static refs this hits the
        // in-memory `StaticToken` cache; for `oauth://<provider>` refs
        // this re-reads the credentials store so rotation is picked up
        // without a daemon restart. Resolved once here, then threaded
        // into `build_headers` -> `auth::apply`.
        let token = self.cfg.auth.token().await?;

        let rb = self.build_headers(self.client.post(self.responses_url()), &req, &token)?;
        let request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) from the built
        // request. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Capture the headers BEFORE the capped body read moves `resp`;
            // the shared mapper reads the rate-limit hint off them.
            let headers = resp.headers().clone();
            let (body_text, hit_cap) = read_error_body(&self.cfg.id, status, resp).await;
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &self.cfg.auth_kind,
                &body_text,
                hit_cap,
            ));
        }

        // Drain the SSE stream until a terminal event lands. Four
        // terminal events to distinguish:
        //   response.completed  -> success: extract `response`, return Ok.
        //   response.incomplete -> success-with-cutoff: extract `response`
        //                          (the cutoff surfaces downstream as
        //                          finish_reason "length", not an error).
        //   response.failed     -> Err::upstream from the response.error.
        //   response.cancelled  -> Err::upstream (cancellation surfaces
        //                          as an explicit error, not silent
        //                          success; clients can retry).
        //
        // The chatgpt-oauth backend ships the actual model output via
        // `response.output_item.done` events and ships
        // `response.completed` with `response.output: []`. Accumulate
        // the items as they fly past so we can backfill the response
        // body when the terminal event omits them. Streaming clients
        // never hit this seam because the SseState machine consumes
        // the deltas directly.
        // Dir 3: upstream response headers, read BEFORE the SSE body
        // stream consumes `resp`. complete() is stream-only, so this
        // is the dir-3 capture point. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        futures::pin_mut!(event_stream);
        let mut completed_body: Option<Value> = None;
        let mut terminal_kind: Option<String> = None;
        let mut accumulated_items: Vec<Value> = Vec::new();
        while let Some(result) = event_stream.next().await {
            let event = result.map_err(|e| Error::Streaming(e.to_string()))?;
            if event.data.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&event.data)
                .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
            let kind = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "response.output_item.done" => {
                    if let Some(item) = parsed.get("item") {
                        // Bounded-growth guard mirroring the stream path
                        // (sse.rs): a legitimate turn emits a small handful
                        // of output items; an adversarial-or-extreme upstream
                        // could ship thousands. Truncate past the cap with a
                        // debug log -- do NOT error, so large-but-legit
                        // responses below the cap still surface.
                        if accumulated_items.len() >= sse::MAX_OUTPUT_BLOCKS {
                            tracing::debug!(
                                provider = %self.cfg.id,
                                cap = sse::MAX_OUTPUT_BLOCKS,
                                "openai-responses: output_item.done beyond cap; skipping"
                            );
                        } else {
                            accumulated_items.push(item.clone());
                        }
                    }
                }
                "response.completed"
                | "response.incomplete"
                | "response.failed"
                | "response.cancelled" => {
                    if let Some(r) = parsed.get("response") {
                        completed_body = Some(r.clone());
                    }
                    terminal_kind = Some(kind.to_string());
                    break;
                }
                _ => {}
            }
        }

        let mut raw_body = completed_body.ok_or_else(|| {
            // Two distinct cases land here:
            //   - terminal_kind = None: the stream exhausted without
            //     ever firing a terminal event (truncation, premature
            //     close, network drop).
            //   - terminal_kind = Some(failed|cancelled|completed|incomplete):
            //     the terminal event fired but did NOT carry a `response`
            //     field (malformed upstream payload).
            // Surface the actual cause so operators don't chase a
            // ghost stream-truncation when the real issue is a
            // missing response field on a known-terminal event.
            let msg = match terminal_kind.as_deref() {
                None => "openai-responses: stream ended without a terminal event".to_string(),
                Some(kind) => format!(
                    "openai-responses: terminal {kind:?} event arrived without a `response` field"
                ),
            };
            Error::upstream(&self.cfg.id, 0, msg)
        })?;

        // Backfill `response.output` from accumulated `output_item.done`
        // events when the terminal body left it empty. The
        // chatgpt-oauth backend ships an empty array on
        // `response.completed` even when output_tokens > 0; the actual
        // items only appear in the per-item done events. Applies to
        // `response.incomplete` too (truncation is success-with-cutoff,
        // same backfill need).
        let backfill_terminal = matches!(
            terminal_kind.as_deref(),
            Some("response.completed" | "response.incomplete")
        );
        if backfill_terminal && !accumulated_items.is_empty() {
            let needs_backfill = raw_body
                .get("output")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            if needs_backfill && let Some(obj) = raw_body.as_object_mut() {
                obj.insert("output".into(), Value::Array(accumulated_items));
            }
        }
        // Trace upstream success body pre-normalize. The
        // chatgpt-oauth endpoint is stream-only; this body is the
        // `response` field extracted from the terminal SSE event, not
        // raw SSE frames. Trace fires for failed/cancelled too so
        // operators can see the body shape that drove the error.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);

        if let Some("response.failed" | "response.cancelled") = terminal_kind.as_deref() {
            // Deserialize so we can use the typed error helper that
            // pulls error.message out of the body. Falls back to a
            // synthetic message when the body doesn't deserialize
            // (matches the stream() path's behavior).
            let typed: Result<crate::openai_responses::response_types::ResponsesResponse> =
                serde_json::from_value(raw_body.clone()).map_err(|e| {
                    Error::upstream(
                        &self.cfg.id,
                        0,
                        format!("openai-responses: terminal {terminal_kind:?} parse failed: {e}"),
                    )
                });
            let err = match typed {
                Ok(body) => crate::openai_responses::response::upstream_error_from_failed(
                    &self.cfg.id,
                    &body,
                ),
                Err(e) => e,
            };
            tracing::warn!(
                provider = %self.cfg.id,
                terminal = %terminal_kind.as_deref().unwrap_or("?"),
                "openai-responses non-success terminal event",
            );
            return Err(err);
        }

        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution; see the note in `complete()`.
        let token = self.cfg.auth.token().await?;

        let rb = self.build_headers(self.client.post(self.responses_url()), &req, &token)?;
        let request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) for the stream
        // path. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Capture the headers BEFORE the capped body read moves `resp`;
            // the shared mapper reads the rate-limit hint off them.
            let headers = resp.headers().clone();
            let (body_text, hit_cap) = read_error_body(&self.cfg.id, status, resp).await;
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &self.cfg.auth_kind,
                &body_text,
                hit_cap,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream below. Mirrors the complete() path so
        // both directions emit dir-3. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();

        let stream = async_stream::stream! {
            let mut state = sse::ResponsesStreamState::default();
            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        // Filter empty `data:` lines (keepalives).
                        if event.data.is_empty() {
                            continue;
                        }
                        let parsed = match sse::parse_data_line(&provider_id, &event.data) {
                            Ok(p) => p,
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        };
                        match state.parse_event(&provider_id, parsed) {
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                            Ok(chunks) => {
                                for c in chunks {
                                    yield Ok(c);
                                }
                            }
                        }
                    }
                }
            }
        };

        Ok(routectl_core::wrap_stream_with_summary(
            stream,
            "upstream",
            PROVIDER_KIND,
            self.cfg.id.clone(),
        ))
    }

    /// Forward upstream-401 to the underlying token source so an
    /// `oauth://` ref can force-refresh through the OAuth store's
    /// per-provider single-flight gate. Static-auth providers
    /// (`env://`, `file://`, `literal:`) inherit the no-op default
    /// from `TokenSource::on_auth_failure`. Mirrors the anthropic_api
    /// egress.
    async fn on_auth_failure(&self) -> Result<()> {
        self.cfg.auth.on_auth_failure().await
    }

    /// Free reachability probe: a single GET against `/models`.
    ///
    /// BINDING read-only guard: only the `ApiKey` lane holds a static,
    /// non-refreshing credential. `ChatgptOauth` (and `BedrockMantle`)
    /// resolve their token through paths a reachability probe must not
    /// trigger, so they report `UnsupportedFreeProbe` and the CLI
    /// orchestration layer owns their reachability. On the `ApiKey` lane
    /// the resolved key is a `StaticToken`, so reading it does no refresh.
    async fn probe(&self) -> routectl_core::ProbeOutcome {
        if self.cfg.auth_kind != AuthKind::ApiKey {
            return routectl_core::ProbeOutcome::UnsupportedFreeProbe;
        }
        let token = match self.cfg.auth.token().await {
            Ok(t) => t,
            Err(_) => {
                return routectl_core::ProbeOutcome::AuthFailed(
                    "provider credential unavailable".into(),
                );
            }
        };
        let url = format!("{}/models", self.cfg.base_url.trim_end_matches('/'));
        let mut headers = reqwest::header::HeaderMap::new();
        match reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(v) => {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
            Err(_) => {
                return routectl_core::ProbeOutcome::Unreachable(
                    "credential could not form an auth header".into(),
                );
            }
        }
        crate::probe::http_get_probe(
            self.cfg.user_agent.as_deref(),
            &url,
            headers,
            crate::probe::PROBE_TIMEOUT,
        )
        .await
    }
}

fn build_error_excerpt(body_text: &str) -> String {
    serde_json::from_str::<Value>(body_text)
        .ok()
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map_or_else(
            || sanitize_upstream_body(body_text),
            std::string::ToString::to_string,
        )
}

/// Map a non-success Responses-API HTTP response into a canonical
/// `Error::Upstream`. Single source of truth shared by both
/// `complete()` and `stream()`: computes the rate-limit-gated header
/// reset hint, resolves the Codex usage-limit body hint (which wins over
/// the header because it carries the 5-hour-cap reset that Retry-After
/// does not), folds the 401/403-vs-else WARN split, and emits the full
/// upstream body once at debug level so call sites do not duplicate it.
///
/// `headers` MUST be read from the response BEFORE the body is consumed
/// (the capped body read moves the body). This ordering is a programmer
/// convention: `headers` is an owned `HeaderMap` clone here, so the
/// compiler does not couple it to the body move. Both call sites clone
/// the headers before reading the body via [`read_error_body`].
fn map_responses_upstream_error(
    provider_id: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    auth_kind: &AuthKind,
    body_text: &str,
    hit_cap: bool,
) -> Error {
    // Reset hint from response headers, gated on rate-limit statuses so a
    // stray Retry-After on a 400 doesn't park the provider.
    let header_hint = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    // The Codex usage-limit body hint wins over the header hint: it carries
    // the 5-hour-cap reset, which Retry-After does not. Attempted on the body
    // (the capped prefix on a cap trip); a truncated body simply fails to
    // parse and falls back to the header hint.
    let codex_hint = serde_json::from_str::<Value>(body_text)
        .ok()
        .and_then(|v| crate::openai_responses::response::codex_reset_hint(&v));
    let retry_after = codex_hint.or(header_hint);
    // Client body + WARN excerpt: on a cap trip the truncated body is
    // untrustworthy, so both collapse to the fixed cap message -- the client
    // never sees the partial body and the prefix never appears at WARN level.
    // Otherwise carry the RAW `{error:...}` envelope so the ingress sanitizer
    // can re-extract the upstream message (sanitized excerpt for a
    // non-envelope body), and log the sanitized excerpt.
    let (warn_excerpt, err_body) = if hit_cap {
        let capped = crate::http_client::body_cap_exceeded_message();
        (capped.clone(), capped)
    } else {
        let msg = build_error_excerpt(body_text);
        let excerpt = sanitize_for_log(&msg);
        let body = if is_json_error_envelope(body_text) {
            body_text.to_string()
        } else {
            msg
        };
        (excerpt, body)
    };
    crate::upstream_log::warn_upstream_failure(
        provider_id,
        status,
        Some(auth_kind),
        &warn_excerpt,
        "openai-responses",
    );
    // Full (capped) upstream body at debug level -- the only path where the
    // truncated prefix bytes may surface, DEBUG-gated and bounded.
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, body_text);
    Error::upstream_with_retry_after(provider_id, status, err_body, retry_after)
}

/// Read a Responses-API upstream error body under the shared response-body
/// cap and return the `(capped-prefix, hit_cap)` pair the mapper builds the
/// client-facing message from. On a cap trip a single WARN records the
/// truncation (`path="error_body"`); a transport failure while reading is
/// logged and yields an empty prefix, mirroring the prior
/// `resp.text().unwrap_or_default()` resilience. `content_length` is read
/// before the body is consumed so the WARN can carry it.
async fn read_error_body(
    provider_id: &str,
    status: u16,
    resp: reqwest::Response,
) -> (String, bool) {
    let content_length = resp.content_length();
    let (bytes, hit_cap) = match crate::http_client::read_body_capped(
        resp,
        crate::http_client::MAX_RESPONSE_BODY_BYTES,
    )
    .await
    {
        Ok(read) => read,
        Err(e) => {
            tracing::warn!(
                provider = %provider_id,
                status,
                error = %e,
                "failed to read upstream error body",
            );
            (Vec::new(), false)
        }
    };
    if hit_cap {
        crate::http_client::warn_body_cap(provider_id, status, content_length, "error_body");
    }
    (String::from_utf8_lossy(&bytes).into_owned(), hit_cap)
}

#[cfg(test)]
#[path = "e2e_tests.rs"]
mod e2e_tests;

#[cfg(test)]
#[path = "excerpt_tests.rs"]
mod excerpt_tests;

#[cfg(test)]
#[path = "auth_wiring_tests.rs"]
mod auth_wiring_tests;

#[cfg(test)]
#[path = "header_merge_tests.rs"]
mod header_merge_tests;
