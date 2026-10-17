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
    ChatChunk, ChatRequest, ChatResponse, Error, ParsedErrorBody, Provider, ReplayScheme, Result,
    debug_upstream_error_body, sanitize_for_log, trace_outgoing_body, trace_upstream_success_body,
    upstream_message_or_excerpt,
};

use crate::stream_completion::StreamCompletion;
use crate::upstream_error::{CarriedBody, UpstreamErrorRead};

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
mod quota_headers;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod response_types;
pub(crate) mod sse;
pub(crate) mod system;
pub(crate) mod tools;
pub(crate) mod types;

/// The lane-agnostic Responses format tag this provider used to stamp on
/// every reasoning detail.
///
/// Recognized forever -- in-flight client histories carry it -- but NEVER
/// emitted: more than one lane stamped it, so an artifact bearing it names
/// no lane and can only be treated as unestablished. Emission goes through
/// [`lane_format_tag`] instead, which is why nothing outside the test
/// fixtures references this alias any more.
#[cfg(test)]
pub(crate) const OPENAI_RESPONSES_FORMAT: &str = routectl_core::OPENAI_RESPONSES_V1;

/// Provider-kind discriminator string used in tracing fields. See
/// the openai_compat module for the rationale.
pub(crate) const PROVIDER_KIND: &str = routectl_core::provider_kind::trace::OPENAI_RESPONSES;

/// The lane spelling every `record_translation_drop` /
/// `record_translation_policy_action` / `record_translation_lane_seen`
/// call on this egress uses. Bound to `PROVIDER_KIND` rather than
/// re-spelled so the telemetry lane and the provider kind can never
/// drift apart.
pub(crate) const LANE: &str = PROVIDER_KIND;

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

/// The format tag this lane stamps on the reasoning artifacts it
/// produces.
///
/// A tag is an OBSERVATION of which lane minted the artifact, so it is
/// never wrong and never needs a vocabulary migration. Keep it distinct
/// from [`lane_scheme`], which is a revisable judgment about validator
/// families -- collapsing the two would make a permanent wire value
/// hostage to a mapping that is allowed to change.
#[must_use]
pub const fn lane_format_tag(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::ChatgptOauth => routectl_core::CODEX_OAUTH,
        AuthKind::ApiKey => routectl_core::OPENAI_APIKEY,
        AuthKind::BedrockMantle => routectl_core::BEDROCK_MANTLE,
    }
}

/// The validator family whose artifacts this lane accepts on replay.
///
/// Probes put the two first-party OpenAI lanes in one id-validating
/// family and the mantle lane in its own content-validating family, so
/// this mapping is many-to-one and deliberately narrower than
/// [`lane_format_tag`].
#[must_use]
pub const fn lane_scheme(kind: AuthKind) -> ReplayScheme {
    match kind {
        AuthKind::ChatgptOauth | AuthKind::ApiKey => ReplayScheme::Codex,
        AuthKind::BedrockMantle => ReplayScheme::Mantle,
    }
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        let r = request::translate(&self.cfg, req)?;
        serde_json::to_value(&r).map_err(|e| {
            Error::Internal(format!(
                "provider `{}`: serialize request body: {e}",
                self.cfg.id
            ))
        })
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        let typed: response_types::ResponsesResponse = serde_json::from_value(raw)
            .map_err(|e| Error::normalize_response(&self.cfg.id, e.to_string()))?;
        response::translate(&self.cfg.id, self.cfg.auth_kind, typed)
    }

    fn replay_lane(&self) -> ReplayScheme {
        lane_scheme(self.cfg.auth_kind)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
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
        crate::wire_declare::declare_stream_flag(&req, true);
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution: for static refs this hits the
        // in-memory `StaticToken` cache; for `oauth://<provider>` refs
        // this re-reads the credentials store so rotation is picked up
        // without a daemon restart. Resolved once here, then threaded
        // into `build_headers` -> `auth::apply`.
        let token = self.cfg.auth.token().await?;

        let rb = self.build_headers(self.client.post(self.responses_url()), &req, &token)?;
        // Serialize to bytes once: the wire tap records them, the mantle
        // signer signs over them, and they are what is transmitted. The
        // content-type is stamped explicitly (rather than via `.json()`) so
        // both lanes emit one deterministic header.
        let body_bytes = crate::wire_declare::serialize_and_deposit(&req, &body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Mantle lane: SigV4/bearer-sign the built request before any header
        // trace or execute so the trace shows the real auth header and the
        // signed input matches the transmitted bytes. No-op on the
        // first-party lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) from the built
        // request. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
        if status >= 400 {
            let read = UpstreamErrorRead::read(&self.cfg.id, resp)
                .await
                .aws_backed(self.is_mantle());
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                &self.cfg.auth_kind,
                read,
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
        // Parse the x-codex-* quota family off the response head BEFORE
        // `resp` is consumed by the SSE byte stream. None on the api-key
        // and mantle lanes (family absent), so no auth-kind gate.
        let upstream_meta = quota_headers::parse_codex_quota(resp.headers())
            .map(routectl_core::UpstreamMeta::from_codex);
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        futures::pin_mut!(event_stream);
        let response_tap = crate::wire_declare::response_tap(&req);
        let mut completed_body: Option<Value> = None;
        let mut terminal_kind: Option<String> = None;
        let mut completion = StreamCompletion::new(PROVIDER_KIND, sse::is_terminal_event);
        let mut accumulated_items: Vec<Value> = Vec::new();
        while let Some(result) = event_stream.next().await {
            let event = result.map_err(|e| Error::Streaming(e.to_string()))?;
            crate::wire_declare::push_frame(response_tap.as_ref(), &event.event, &event.data);
            if event.data.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&event.data)
                .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
            let kind = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
            completion.observe(kind);
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
                terminal if sse::is_terminal_event(terminal) => {
                    if let Some(r) = parsed.get("response") {
                        completed_body = Some(r.clone());
                    }
                    terminal_kind = Some(terminal.to_string());
                    break;
                }
                _ => {}
            }
        }
        completion.end_of_stream(&self.cfg.id)?;

        // The terminal event fired but carried no `response` field: a
        // malformed payload, reported as such rather than as a cut stream.
        let mut raw_body = completed_body.ok_or_else(|| {
            Error::upstream(
                &self.cfg.id,
                0,
                format!(
                    "openai-responses: terminal {:?} event arrived without a `response` field",
                    terminal_kind.as_deref().unwrap_or("?")
                ),
            )
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
                terminal = %sanitize_for_log(terminal_kind.as_deref().unwrap_or("?")),
                "openai-responses non-success terminal event",
            );
            return Err(err);
        }

        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        chat_resp.upstream_meta = upstream_meta;
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model), lane = tracing::field::Empty, auth_mode = tracing::field::Empty, region = tracing::field::Empty))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        #[cfg(feature = "bedrock")]
        self.record_mantle_span_fields();
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        crate::wire_declare::declare_stream_flag(&req, true);
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution; see the note in `complete()`.
        let token = self.cfg.auth.token().await?;

        let rb = self.build_headers(self.client.post(self.responses_url()), &req, &token)?;
        // See complete(): serialize once, deposit, sign and send the same
        // bytes with an explicit content-type.
        let body_bytes = crate::wire_declare::serialize_and_deposit(&req, &body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
        let mut request = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body_bytes)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Mantle lane: sign the built stream request before trace/execute
        // (see complete()). No-op on the first-party lane.
        #[cfg(feature = "bedrock")]
        self.sign_mantle(&mut request).await?;
        // Dir 2: outgoing request headers (incl. auth) for the stream
        // path. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(crate::http_client::redirect_not_followed_error(
                &self.cfg.id,
            ));
        }
        if status >= 400 {
            let read = UpstreamErrorRead::read(&self.cfg.id, resp)
                .await
                .aws_backed(self.is_mantle());
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                &self.cfg.auth_kind,
                read,
            ));
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream below. Mirrors the complete() path so
        // both directions emit dir-3. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        // Parse the x-codex-* quota family off the response head BEFORE
        // `resp` is moved into the byte stream. The parsed carrier is
        // attached to the FIRST canonical chunk yielded by the stream;
        // consumers must not assume it on later chunks. None on the
        // api-key and mantle lanes (family absent).
        let upstream_meta = quota_headers::parse_codex_quota(resp.headers())
            .map(routectl_core::UpstreamMeta::from_codex);

        let provider_id = self.cfg.id.clone();
        let auth_kind = self.cfg.auth_kind;
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        let response_tap = crate::wire_declare::response_tap(&req);

        let stream = async_stream::stream! {
            let mut pending_upstream_meta = upstream_meta;
            let mut state = sse::ResponsesStreamState::new(auth_kind);
            let mut completion = StreamCompletion::new(PROVIDER_KIND, sse::is_terminal_event);
            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        crate::wire_declare::push_frame(response_tap.as_ref(), &event.event, &event.data);
                        // Filter empty `data:` lines (keepalives).
                        if event.data.is_empty() {
                            continue;
                        }
                        // Gateway-style end sentinel. Not a terminal event:
                        // ends the transport, and the completion verdict
                        // below decides whether the turn finished first.
                        if event.data.trim() == "[DONE]" {
                            break;
                        }
                        let parsed = match sse::parse_data_line(&provider_id, &event.data) {
                            Ok(p) => p,
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        };
                        completion.observe(parsed.kind.as_str());
                        match state.parse_event(&provider_id, parsed) {
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                            Ok(chunks) => {
                                for mut c in chunks {
                                    // Attach the codex-quota carrier to the
                                    // FIRST canonical chunk only; `take()`
                                    // leaves None for every subsequent chunk.
                                    // Held across empty/keepalive events and
                                    // events that emit zero chunks.
                                    if let Some(head) = pending_upstream_meta.take() {
                                        c.upstream_meta = Some(match c.upstream_meta.take() {
                                            Some(own) => head.merge(own),
                                            None => head,
                                        });
                                    }
                                    yield Ok(c);
                                }
                            }
                        }
                    }
                }
            }
            if let Err(e) = completion.end_of_stream(&provider_id) {
                tracing::warn!(
                    provider = %provider_id,
                    "openai-responses stream: upstream closed before a terminal event"
                );
                yield Err(e);
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
        // Mantle lane: authenticates with SigV4/bearer and exposes no free
        // models-list surface, so route to the credential-resolve probe
        // instead of dialing the inference host (mirrors the Bedrock and
        // anthropic-api mantle posture).
        #[cfg(feature = "bedrock")]
        if let Some(mantle) = self.cfg.mantle.as_ref() {
            return crate::mantle::probe(&mantle.creds).await;
        }
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

/// Map a non-success Responses-API HTTP response into a canonical
/// `Error::Upstream`. Single source of truth shared by both `complete()`
/// and `stream()`: the response-derived pieces (gated header reset hint,
/// classifier tokens, correlation id) come from the shared
/// [`UpstreamErrorRead`]; this lane resolves the Codex usage-limit body
/// hint (which wins over the header because it carries the 5-hour-cap reset
/// that Retry-After does not), builds the client body, emits the
/// upstream-failure WARN, and logs the full body once at debug level.
fn map_responses_upstream_error(
    provider_id: &str,
    auth_kind: &AuthKind,
    read: UpstreamErrorRead,
) -> Error {
    // A truncated body carries no parse and falls back to the header hint.
    let codex_hint = read
        .parsed()
        .map(ParsedErrorBody::value)
        .and_then(crate::openai_responses::response::codex_reset_hint);
    let retry_after = codex_hint.or(read.retry_after());
    // Client body + WARN excerpt: on a cap trip both collapse to the fixed
    // cap message, so the partial body never reaches the client or the WARN
    // line. A native `{"error":{...}}` envelope is carried RAW with its parse
    // so the ingress can surface the upstream message. Any other body is a
    // potential mantle (AWS) body and routes through the shared Bedrock
    // scrub, gated on shape rather than on a lifted token. The carried parse
    // is always the parse of the carried text.
    let (warn_excerpt, carried) = if read.hit_cap() {
        let capped = crate::http_client::body_cap_exceeded_message();
        (capped.clone(), CarriedBody::Replacement(capped))
    } else if read.is_native_envelope() && !read.needs_access_denied_scrub() {
        let excerpt = sanitize_for_log(&upstream_message_or_excerpt(read.text(), read.parsed()));
        (excerpt, CarriedBody::Original)
    } else {
        let scrubbed = read.non_envelope_client_message();
        let excerpt = sanitize_for_log(&scrubbed);
        (excerpt, read.aws_carried(scrubbed))
    };
    crate::upstream_log::warn_upstream_failure(
        provider_id,
        read.status(),
        Some(auth_kind),
        &warn_excerpt,
        "openai-responses",
    );
    debug_upstream_error_body(
        PROVIDER_KIND,
        provider_id,
        read.status(),
        &read.debug_body(),
    );
    read.into_error(provider_id, carried, retry_after)
}

#[cfg(test)]
#[path = "e2e_tests.rs"]
mod e2e_tests;

#[cfg(test)]
#[path = "redirect_tests.rs"]
mod redirect_tests;

#[cfg(test)]
#[path = "excerpt_tests.rs"]
mod excerpt_tests;

#[cfg(test)]
#[path = "parse_once_tests.rs"]
mod parse_once_tests;

#[cfg(test)]
#[path = "auth_wiring_tests.rs"]
mod auth_wiring_tests;

#[cfg(test)]
#[path = "header_merge_tests.rs"]
mod header_merge_tests;

#[cfg(test)]
#[path = "lane_mapping_tests.rs"]
mod lane_mapping_tests;

#[cfg(test)]
#[path = "lane_tag_emission_tests.rs"]
mod lane_tag_emission_tests;

#[cfg(test)]
#[path = "reasoning_continuity_tests.rs"]
mod reasoning_continuity_tests;

#[cfg(test)]
#[path = "stream_terminal_tests.rs"]
mod stream_terminal_tests;

#[cfg(test)]
#[path = "wire_tap_tests.rs"]
mod wire_tap_tests;
