//! OpenAI Responses API provider (`openai-responses` provider type).
//!
//! Three auth surfaces:
//!
//!   - `chatgpt-oauth` (default): ChatGPT subscription surface
//!     at `https://chatgpt.com/backend-api/codex`. Uses
//!     Authorization: Bearer <jwt> + ChatGPT-Account-Id + originator
//!     headers (codex parity). Fully wired: `complete()` + `stream()`
//!     both ship.
//!   - `api-key`: standard OpenAI surface at
//!     `https://api.openai.com/v1`. Uses Authorization: Bearer
//!     <api_key> only; `OpenAI-Organization` / `OpenAI-Project` can
//!     be set via `extra_headers` if needed.
//!   - `bedrock-mantle`: AWS Mantle proxy at
//!     `https://bedrock-mantle.<region>.api.aws/openai/v1`. Uses
//!     Authorization: Bearer <bearer> using the long-term Bedrock API
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
//! `response` field from `response.completed`.

use std::sync::Arc;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{
    debug_upstream_error_body, sanitize_for_log, sanitize_upstream_body, trace_outgoing_body,
    trace_upstream_success_body, ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result,
    StaticToken, TokenSource,
};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// ChatGPT subscription via OAuth bearer JWT. Default.
    #[default]
    ChatgptOauth,
    /// Standard OpenAI API key against `api.openai.com/v1/responses`.
    /// Wired in CG.E.
    ApiKey,
    /// AWS Bedrock Mantle proxy (OpenAI-shape):
    /// `Authorization: Bearer <bearer>` using the long-term Bedrock API
    /// key (resolved via api_key_ref, typically
    /// env://AWS_BEARER_TOKEN_BEDROCK).
    BedrockMantle,
}

/// Resolved configuration for one Responses provider entry. The
/// factory builds this from the TOML `ProviderEntry::OpenaiResponses`
/// variant after resolving secret references.
#[derive(Clone)]
pub struct OpenAiResponsesConfig {
    /// Stable id used in errors and on `routectl_provider` response
    /// fields. Format: `openai-responses:<table-key>`.
    pub id: String,
    /// Source of the bearer token (JWT for ChatgptOauth; API key for
    /// ApiKey; long-term Bedrock API key for BedrockMantle). For
    /// env/file/literal secret refs this is a `StaticToken` resolved
    /// once at construction. For `oauth://<provider>` refs the factory
    /// passes a per-request resolver that re-reads the credentials
    /// store, so ChatGPT-OAuth token rotation is picked up live
    /// without restarting routectl. Resolved once per upstream request
    /// via `auth.token().await` in `complete()` / `stream()`.
    pub auth: Arc<dyn TokenSource>,
    /// Resolved ChatGPT account ID. Required for ChatgptOauth;
    /// must be None for the other variants (enforced by the factory).
    pub account_id: Option<String>,
    /// Endpoint base URL. Defaults are auth_kind-dependent (resolved
    /// by the factory):
    ///   - ChatgptOauth: `https://chatgpt.com/backend-api/codex`
    ///   - ApiKey: `https://api.openai.com/v1`
    ///   - BedrockMantle: `https://bedrock-mantle.<region>.api.aws/openai/v1`
    pub base_url: String,
    /// Auth dispatch.
    pub auth_kind: AuthKind,
    /// Provider-level extra HTTP headers (renamed from
    /// `extra_headers` in v0.6.0). Reserved header names
    /// (`authorization`, `host`, `content-type`, ...) are rejected
    /// at apply-time to keep the auth contract intact.
    pub header_extras: Vec<(String, String)>,
    /// Override the User-Agent. `None` -> codex CLI's UA shape
    /// (`codex_cli_rs/<X.Y.Z> (...) <terminal>`) so the chatgpt.com
    /// risk system does not flag the fingerprint as drifted.
    pub user_agent: Option<String>,
    /// Override the `originator` header sent on ChatgptOauth.
    /// `None` -> `codex_cli_rs` (codex's `DEFAULT_ORIGINATOR`).
    pub originator: Option<String>,
    /// Stable per-credential session id (UUIDv4) used in the
    /// `session-id` HTTP header on outbound chatgpt-oauth traffic.
    /// `Some` only on ChatgptOauth; the factory reads / lazily mints
    /// it from credentials.json. Mirrors codex's `ModelClient`
    /// session_id.
    pub session_id: Option<String>,
    /// Stable per-install installation id (UUIDv4) used in the
    /// `x-codex-installation-id` HTTP header on outbound chatgpt-oauth
    /// traffic. `Some` only on ChatgptOauth; the factory reads /
    /// lazily mints it from `~/.config/routectl/installation_id`.
    /// Mirrors codex's installation_id, which survives login re-runs.
    pub installation_id: Option<String>,
}

impl std::fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-rolled Debug elides the auth source so a derived
        // `{:?}` on the config (or any struct embedding it) can never
        // print the bearer/JWT. `StaticToken`'s own Debug already
        // redacts; this is the second line of defense mirroring
        // `AnthropicApiConfig`.
        f.debug_struct("OpenAiResponsesConfig")
            .field("id", &self.id)
            .field("auth", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("base_url", &self.base_url)
            .field("auth_kind", &self.auth_kind)
            .field("header_extras_len", &self.header_extras.len())
            .field("user_agent", &self.user_agent)
            .field("originator", &self.originator)
            .field("session_id_present", &self.session_id.is_some())
            .field("installation_id_present", &self.installation_id.is_some())
            .finish()
    }
}

impl OpenAiResponsesConfig {
    /// Construct with a static bearer string. The token is wrapped in
    /// `StaticToken` so the provider's resolution call site is uniform
    /// across static and managed sources. Existing callers that pass a
    /// resolved key keep their signatures unchanged.
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new_with_auth(id, Arc::new(StaticToken::new(api_key)))
    }

    /// Construct with a custom `TokenSource`. Used by the factory when
    /// wiring `oauth://<provider>` to a per-request resolver.
    pub fn new_with_auth(id: impl Into<String>, auth: Arc<dyn TokenSource>) -> Self {
        Self {
            id: id.into(),
            auth,
            account_id: None,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: Vec::new(),
            user_agent: None,
            originator: None,
            session_id: None,
            installation_id: None,
        }
    }
}

pub struct OpenAiResponsesProvider {
    cfg: OpenAiResponsesConfig,
    client: Client,
    /// Cloudflare cookie jar shared with the reqwest client. `Arc`d so
    /// the provider can persist the jar to disk on Drop while reqwest
    /// continues to read / write through it on every request. `None`
    /// when persistence is intentionally disabled (no `HOME`,
    /// `ROUTECTL_COOKIE_FILE` set to empty, etc.).
    cookie_jar: Option<Arc<reqwest_cookie_store::CookieStoreMutex>>,
    /// Persistence path for `cookie_jar`. Resolved at construction so
    /// Drop can save without re-reading env vars (Drop runs late and
    /// env mutations during teardown are race-prone).
    cookie_path: Option<std::path::PathBuf>,
}

impl OpenAiResponsesProvider {
    pub fn new(cfg: OpenAiResponsesConfig) -> Self {
        // Always pass an explicit UA string so the client-level default
        // header carries the codex-derived value. Operator-supplied
        // `cfg.user_agent` wins; otherwise fall back to the codex CLI
        // UA shape from auth::default_user_agent.
        let ua = cfg
            .user_agent
            .clone()
            .unwrap_or_else(auth::default_user_agent);

        // Cloudflare cookie jar (chatgpt-oauth path). Hydrate from
        // disk on construction; reqwest reads / writes through the
        // shared Arc on every request; Drop persists on shutdown.
        // Falling back to the cookie-less client when no path is
        // resolvable keeps tests / non-OAuth deploys working.
        let cookie_path = cookies::default_cookie_path();
        let (client, cookie_jar) = match cookie_path.as_deref() {
            Some(path) => {
                let jar = cookies::load_jar(path);
                let client =
                    crate::http_client::build_with_cookie_provider(Some(&ua), Arc::clone(&jar));
                (client, Some(jar))
            }
            None => (crate::http_client::build(Some(&ua)), None),
        };
        Self {
            cfg,
            client,
            cookie_jar,
            cookie_path,
        }
    }

    /// URL for the `/responses` endpoint. ChatgptOauth talks to the
    /// `backend-api/codex` surface; api-key talks to `/v1/responses`
    /// directly. The base_url already encodes the difference -- we
    /// just append `/responses`.
    fn responses_url(&self) -> String {
        format!("{}/responses", self.cfg.base_url.trim_end_matches('/'))
    }

    fn build_headers(
        &self,
        rb: reqwest::RequestBuilder,
        req: &ChatRequest,
        bearer: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let mut rb = auth::apply(
            rb,
            &self.cfg,
            bearer,
            routectl_core::codex_fingerprint::codex_window_id(),
        )?;
        // Prefer the router-composed map (provider + model merged at
        // dispatch) if present; fall back to `self.cfg.header_extras`
        // for library consumers that built the provider directly.
        let source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        for (k, v) in &source {
            if crate::http_client::is_auth_header(k) {
                tracing::warn!(
                    provider = %self.cfg.id,
                    header = %k,
                    "ignoring auth-reserved header from header_extras (would bypass provider auth)"
                );
                continue;
            }
            if crate::http_client::is_managed_header(k) {
                tracing::debug!(
                    provider = %self.cfg.id,
                    header = %k,
                    "dropping managed header from header_extras; composed dynamically by routectl"
                );
                continue;
            }
            rb = rb.header(k.as_str(), v.as_str());
        }
        Ok(rb)
    }
}

/// Persist the Cloudflare cookie jar on provider teardown so the next
/// process boot does not pay the Cloudflare challenge cost from a
/// cold cache. Soft-fail on I/O error -- a missing or unwritable
/// persistence path must not poison shutdown.
impl Drop for OpenAiResponsesProvider {
    fn drop(&mut self) {
        let (Some(jar), Some(path)) = (self.cookie_jar.as_ref(), self.cookie_path.as_ref()) else {
            return;
        };
        if let Err(e) = cookies::save_jar(jar, path) {
            tracing::debug!(
                provider = %self.cfg.id,
                path = %path.display(),
                error = %e,
                "openai-responses: cookie jar persist failed; continuing"
            );
        }
    }
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
            let body_text = resp.text().await.unwrap_or_default();
            debug_upstream_error_body(PROVIDER_KIND, &self.cfg.id, status, &body_text);
            let msg = serde_json::from_str::<Value>(&body_text)
                .ok()
                .as_ref()
                .and_then(|v| v.pointer("/error/message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitize_upstream_body(&body_text));
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %msg,
                    "openai-responses upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "openai-responses upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, msg));
        }

        // Drain the SSE stream until a terminal event lands. Three
        // terminal events to distinguish:
        //   response.completed -> success: extract `response`, return Ok.
        //   response.failed    -> Err::upstream from the response.error.
        //   response.cancelled -> Err::upstream (cancellation surfaces
        //                         as an explicit error, not silent
        //                         success; clients can retry).
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
                        accumulated_items.push(item.clone());
                    }
                }
                "response.completed" | "response.failed" | "response.cancelled" => {
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
            //   - terminal_kind = Some(failed|cancelled|completed): the
            //     terminal event fired but did NOT carry a `response`
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
        // items only appear in the per-item done events.
        if terminal_kind.as_deref() == Some("response.completed") && !accumulated_items.is_empty() {
            let needs_backfill = raw_body
                .get("output")
                .and_then(Value::as_array)
                .map(Vec::is_empty)
                .unwrap_or(true);
            if needs_backfill {
                if let Some(obj) = raw_body.as_object_mut() {
                    obj.insert("output".into(), Value::Array(accumulated_items));
                }
            }
        }
        // Trace upstream success body pre-normalize. The
        // chatgpt-oauth endpoint is stream-only; this body is the
        // `response` field extracted from the terminal SSE event, not
        // raw SSE frames. Trace fires for failed/cancelled too so
        // operators can see the body shape that drove the error.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);

        match terminal_kind.as_deref() {
            Some("response.failed") | Some("response.cancelled") => {
                // Deserialize so we can use the typed error helper that
                // pulls error.message out of the body. Falls back to a
                // synthetic message when the body doesn't deserialize
                // (matches the stream() path's behavior).
                let typed: Result<crate::openai_responses::response_types::ResponsesResponse> =
                    serde_json::from_value(raw_body.clone()).map_err(|e| {
                        Error::upstream(
                            &self.cfg.id,
                            0,
                            format!(
                                "openai-responses: terminal {terminal:?} parse failed: {e}",
                                terminal = terminal_kind
                            ),
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
            _ => {}
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
            let body_text = resp.text().await.unwrap_or_default();
            debug_upstream_error_body(PROVIDER_KIND, &self.cfg.id, status, &body_text);
            let msg = serde_json::from_str::<Value>(&body_text)
                .ok()
                .as_ref()
                .and_then(|v| v.pointer("/error/message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitize_upstream_body(&body_text));
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %msg,
                    "openai-responses upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %msg,
                    "openai-responses upstream error",
                );
            }
            return Err(Error::upstream(&self.cfg.id, status, msg));
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
                        match state.process_event(&provider_id, parsed) {
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
}

// ---------------------------------------------------------------------------
// End-to-end tests (wiremock-driven complete + stream paths)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use futures::StreamExt;
    use routectl_core::{ChatRequest, MessageContent};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_provider(base_url: &str) -> OpenAiResponsesProvider {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:test".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct-uuid".into()),
            base_url: base_url.to_string(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: Vec::new(),
            user_agent: None,
            originator: None,
            session_id: None,
            installation_id: None,
        };
        OpenAiResponsesProvider::new(cfg)
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gpt-5-codex".into(),
            messages: vec![routectl_core::Message {
                role: routectl_core::Role::User,
                content: MessageContent::Text("ping".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn complete_post_returns_chat_response() {
        // Arrange: complete() forces stream=true and drains SSE until
        // `response.completed`. The mock must return a proper SSE stream
        // with that terminal event (not a plain JSON body).
        let server = MockServer::start().await;
        let completed_body = serde_json::json!({
            "id": "resp_01",
            "object": "response",
            "status": "completed",
            "model": "gpt-5-codex",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "pong"}]
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        // Wrap in a `response.completed` SSE event (the only one we need).
        let event_body = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
            serde_json::to_string(&completed_body).unwrap()
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(event_body),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let resp = provider.complete(base_req()).await.expect("complete");

        // Assert
        assert_eq!(resp.id, "resp_01");
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            resp.routectl_provider.as_deref(),
            Some("openai-responses:test")
        );
    }

    /// Pin: when the SSE stream's terminal event is `response.failed`,
    /// `complete()` must return `Err::Upstream` with the body's
    /// `error.message` -- NOT a 200 ChatResponse with finish_reason="error".
    #[tokio::test]
    async fn complete_response_failed_returns_upstream_error() {
        let server = MockServer::start().await;
        let failed_body = serde_json::json!({
            "id": "resp_failed",
            "object": "response",
            "status": "failed",
            "model": "gpt-5-codex",
            "error": {"code": "rate_limited", "message": "rate limit exceeded"},
            "output": []
        });
        let event_body = format!(
            "data: {{\"type\":\"response.failed\",\"response\":{}}}\n\n",
            serde_json::to_string(&failed_body).unwrap()
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(event_body),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        let err = provider.complete(base_req()).await.unwrap_err();
        match err {
            Error::Upstream { body, .. } => {
                assert!(
                    body.contains("rate limit exceeded"),
                    "expected error.message, got body: {body}"
                );
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    /// Pin: `response.cancelled` also surfaces as Err::Upstream so
    /// callers can distinguish from a clean completion (and route
    /// retries appropriately).
    #[tokio::test]
    async fn complete_response_cancelled_returns_upstream_error() {
        let server = MockServer::start().await;
        let cancelled_body = serde_json::json!({
            "id": "resp_cancelled",
            "object": "response",
            "status": "cancelled",
            "model": "gpt-5-codex",
            "output": []
        });
        let event_body = format!(
            "data: {{\"type\":\"response.cancelled\",\"response\":{}}}\n\n",
            serde_json::to_string(&cancelled_body).unwrap()
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(event_body),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        let err = provider.complete(base_req()).await.unwrap_err();
        match err {
            Error::Upstream { .. } => {}
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_non_2xx_returns_upstream_error_with_body_excerpt() {
        // Arrange
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string("{\"error\":{\"message\":\"oops\"}}"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let err = provider
            .complete(base_req())
            .await
            .expect_err("expected upstream err");

        // Assert
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 500);
                assert!(body.contains("oops"), "body: {body}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_yields_error_on_truncated_sse() {
        // Arrange: a wiremock body that opens an SSE event but never
        // terminates it (no final `\n\n` framing, no `[DONE]`). The
        // stream loop should either yield a Streaming Err or simply
        // exhaust without panicking; what it MUST NOT do is loop
        // forever or unwrap a partial event.
        let server = MockServer::start().await;
        // Open `data: ` but no terminating blank line + no JSON body.
        // The eventsource decoder will treat this as a parse error or
        // as no event emitted; in both cases the stream must terminate
        // cleanly without panicking.
        let truncated = "data: {\"type\":\"response.created\",\"resp";
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(truncated)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let mut s = provider.stream(base_req()).await.expect("stream");
        let mut chunks: Vec<Result<ChatChunk, Error>> = Vec::new();
        while let Some(item) = s.next().await {
            chunks.push(item);
            // Bound the loop defensively so a regression doesn't hang
            // the test forever.
            if chunks.len() >= 16 {
                break;
            }
        }

        // Assert: stream terminated (didn't panic) and no chunks
        // beyond what could be parsed (an Err is acceptable too).
        let oks = chunks.iter().filter(|r| r.is_ok()).count();
        let errs = chunks.iter().filter(|r| r.is_err()).count();
        // Either we got 0 successful chunks + an Err, or we got
        // nothing at all (parser ate the partial line). Both are
        // acceptable; what we're guarding against is panic / hang.
        assert!(
            errs >= 1 || (oks == 0 && errs == 0),
            "expected truncated stream to yield either an Err or empty; got {oks} oks + {errs} errs"
        );
    }

    #[tokio::test]
    async fn stream_yields_chat_chunks_for_full_session() {
        // Arrange
        let server = MockServer::start().await;
        // Construct an SSE body with `data: <json>\n\n` framing.
        let events = [
            serde_json::json!({"type": "response.created", "response": {"id":"r","model":"m"}}),
            serde_json::json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "id":"m1", "role":"assistant", "content":[]}
            }),
            serde_json::json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id":"r", "status":"completed", "model":"m",
                    "output":[{"type":"message","id":"m1","role":"assistant",
                                "content":[{"type":"output_text","text":"hi"}]}],
                    "usage": {"input_tokens":1, "output_tokens":1, "total_tokens":2}
                }
            }),
        ];
        let sse_body: String = events.iter().map(|e| format!("data: {}\n\n", e)).collect();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act
        let mut s = provider.stream(base_req()).await.expect("stream");
        let mut chunks: Vec<ChatChunk> = Vec::new();
        while let Some(item) = s.next().await {
            chunks.push(item.expect("chunk ok"));
        }

        // Assert: created (role) + text delta + final = 3 chunks.
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("hi"));
        let final_c = chunks.last().unwrap();
        assert_eq!(final_c.choices[0].finish_reason.as_deref(), Some("stop"));
    }
}

// ---------------------------------------------------------------------------
// Auth-wiring tests (TokenSource delegation + Debug redaction)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod auth_wiring_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `TokenSource` that counts `on_auth_failure` invocations so we
    /// can assert the provider delegates to it. `token()` returns a
    /// fixed value; the counter proves the delegation wiring.
    #[derive(Default)]
    struct CountingTokenSource {
        on_auth_failure_calls: AtomicUsize,
    }

    impl std::fmt::Debug for CountingTokenSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CountingTokenSource").finish()
        }
    }

    #[async_trait]
    impl TokenSource for CountingTokenSource {
        async fn token(&self) -> Result<String> {
            Ok("counting-jwt".into())
        }

        async fn on_auth_failure(&self) -> Result<()> {
            self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// `Provider::on_auth_failure` must delegate to the underlying
    /// `TokenSource::on_auth_failure` so an `oauth://` source can
    /// force-refresh. Verified by a fake source that counts calls.
    #[tokio::test]
    async fn on_auth_failure_delegates_to_token_source() {
        // Arrange
        let source = Arc::new(CountingTokenSource::default());
        let mut cfg = OpenAiResponsesConfig::new("openai-responses:test", "unused");
        cfg.auth = source.clone();
        let provider = OpenAiResponsesProvider::new(cfg);

        // Act
        provider
            .on_auth_failure()
            .await
            .expect("on_auth_failure ok");
        provider
            .on_auth_failure()
            .await
            .expect("on_auth_failure ok");

        // Assert: each Provider-level call reached the token source.
        assert_eq!(source.on_auth_failure_calls.load(Ordering::SeqCst), 2);
    }

    /// Debug for `OpenAiResponsesConfig` must redact the auth source:
    /// the inner token must never appear, and a `[REDACTED]` marker
    /// must be present in its place.
    #[test]
    fn config_debug_redacts_auth_token() {
        // Arrange
        let cfg = OpenAiResponsesConfig::new("openai-responses:test", "super-secret-jwt");

        // Act
        let dbg = format!("{cfg:?}");

        // Assert
        assert!(
            !dbg.contains("super-secret-jwt"),
            "Debug must not leak the auth token; got: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "Debug must mark the auth field redacted; got: {dbg}"
        );
    }
}
