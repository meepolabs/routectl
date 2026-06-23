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
//! `response` field from `response.completed` or `response.incomplete`.

use std::sync::Arc;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::{
    debug_upstream_error_body, is_json_error_envelope, sanitize_for_log, sanitize_upstream_body,
    trace_outgoing_body, trace_upstream_success_body, ChatChunk, ChatRequest, ChatResponse, Error,
    Provider, Result, StaticToken, TokenSource,
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
    /// backend sees a consistent codex client identity.
    pub user_agent: Option<String>,
    /// Stable per-credential codex session id, stamped as the
    /// `session-id` header on the ChatgptOauth surface. `Some` only when
    /// the provider's `oauth://codex` credential carries a session_id
    /// minted at login; resolved once at build time via
    /// `SecretStore::peek_session_id`. `None` for ApiKey / BedrockMantle
    /// providers or a credential that has none -- in every such case
    /// `build_headers` stamps no `session-id` header.
    pub session_id: Option<String>,
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
            // Presence only: the session_id ties requests to one logical
            // session; treat it as sensitive so its value never enters logs.
            .field("session_id", &self.session_id.is_some())
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
            session_id: None,
        }
    }
}

pub struct OpenAiResponsesProvider {
    cfg: OpenAiResponsesConfig,
    client: Client,
    /// Per-provider codex window-id (UUIDv4), generated once in `new()`
    /// and reused on every ChatgptOauth request as the
    /// `x-codex-window-id` header. Stable for the life of this provider
    /// instance so a single logical session keeps one window-id; a
    /// router rebuild (hot-reload) mints a fresh one, which is
    /// acceptable for the operator-driven header_extras model.
    window_id: String,
    /// Cloudflare cookie jar shared with the reqwest client. `Arc`d so
    /// the provider can persist the jar to disk on Drop while reqwest
    /// continues to read / write through it on every request. `None`
    /// when the persistence path cannot be resolved (no `HOME` and no
    /// `ROUTECTL_COOKIE_FILE` set, or `HOME` is empty). An empty
    /// `ROUTECTL_COOKIE_FILE` falls through to the HOME-based default
    /// path -- it does NOT disable persistence.
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
            window_id: uuid::Uuid::new_v4().to_string(),
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
        let mut rb = auth::apply(rb, &self.cfg, bearer)?;

        // Build a per-request HeaderMap so the generated codex identity
        // headers (below) can OVERRIDE any same-named header_extras
        // entry. reqwest's `RequestBuilder::header()` APPENDS on
        // collision; `HeaderMap::insert` replaces. The insertion order
        // encodes the override precedence (later wins):
        //   1. compiled codex identity defaults (ChatgptOauth only)
        //   2. operator header_extras (overrides matching defaults)
        //   3. per-request / per-provider UUIDs (always win)
        let mut header_map = reqwest::header::HeaderMap::new();

        // Compiled codex identity defaults. Fire by default on the
        // ChatgptOauth path so a zero-config operator (auth_kind +
        // api_key_ref only) still emits a full codex fingerprint. The
        // header_extras loop below OVERRIDES any matching key. ApiKey /
        // BedrockMantle get no defaults (no codex fingerprint).
        if self.cfg.auth_kind == AuthKind::ChatgptOauth {
            for (k, v) in default_codex_identity_headers() {
                crate::http_client::insert_header(&mut header_map, &self.cfg.id, k, v);
            }
            // Stable per-credential id minted at login; ties requests to
            // one logical session. Inserted in the defaults phase (before
            // the header_extras loop) so an operator `header_extras` entry
            // for `session-id` still wins, and omitted when the credential
            // carries none. Value never logged.
            if let Some(sid) = &self.cfg.session_id {
                crate::http_client::insert_header(&mut header_map, &self.cfg.id, "session-id", sid);
            }
        }

        // Prefer the router-composed map (provider + model merged at
        // dispatch) if present; fall back to `self.cfg.header_extras`
        // for library consumers that built the provider directly.
        let source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        crate::http_client::apply_header_extras(&mut header_map, &source, &self.cfg.id, &[]);

        // On the ChatgptOauth path, inject the per-request and
        // per-provider codex identity headers. These OVERRIDE any
        // same-named header_extras entry (HeaderMap::insert replaces):
        //   - thread-id / x-client-request-id: one fresh UUIDv4 per
        //     request, shared between the two. Codex pairs them.
        //   - x-codex-window-id: the per-provider UUID from
        //     `self.window_id`, stable across requests on this instance.
        if self.cfg.auth_kind == AuthKind::ChatgptOauth {
            let thread_id = uuid::Uuid::new_v4().to_string();
            crate::http_client::insert_header(
                &mut header_map,
                &self.cfg.id,
                "thread-id",
                &thread_id,
            );
            crate::http_client::insert_header(
                &mut header_map,
                &self.cfg.id,
                "x-client-request-id",
                &thread_id,
            );
            crate::http_client::insert_header(
                &mut header_map,
                &self.cfg.id,
                "x-codex-window-id",
                &self.window_id,
            );
        }

        if !header_map.is_empty() {
            rb = rb.headers(header_map);
        }
        Ok(rb)
    }
}

/// Compiled codex identity-header defaults for the ChatgptOauth path.
/// These ship with routectl and fire by default so a zero-config
/// operator (auth_kind + api_key_ref only) emits a full codex
/// fingerprint without hand-listing every header in `header_extras`.
/// An operator `header_extras` entry for any of these keys OVERRIDES
/// the default (the build_headers loop inserts after these). The
/// per-request UUIDs (thread-id / x-client-request-id /
/// x-codex-window-id) are NOT defaults -- they are generated per
/// request and always win.
///
/// `version` tracks `PINNED_CODEX_VERSION`; bump that constant each
/// release so the wire fingerprint stays current (the chatgpt.com risk
/// system flags stale fingerprints).
fn default_codex_identity_headers() -> [(&'static str, &'static str); 3] {
    use routectl_core::identity::codex::{
        CODEX_ORIGINATOR, ORIGINATOR_HEADER_NAME, PINNED_CODEX_VERSION, RESIDENCY_HEADER_NAME,
        RESIDENCY_HEADER_VALUE,
    };
    [
        (ORIGINATOR_HEADER_NAME, CODEX_ORIGINATOR),
        (RESIDENCY_HEADER_NAME, RESIDENCY_HEADER_VALUE),
        ("version", PINNED_CODEX_VERSION),
    ]
}

/// Persist the Cloudflare cookie jar on provider teardown so the next
/// process boot does not pay the Cloudflare challenge cost from a
/// cold cache. Soft-fail on I/O error -- a missing or unwritable
/// persistence path must not poison shutdown.
///
/// Implementation note: `cookies::save_jar` is blocking file I/O.
/// Performing it directly in `drop` blocks whichever async executor
/// thread holds the last `Arc` reference -- a problem on hot-reload
/// where the router rebuilds providers in place while the runtime is
/// live. Instead we detect a live Tokio runtime via
/// `Handle::try_current()` and delegate to `spawn_blocking` (a
/// best-effort fire-and-forget task on the blocking thread pool). When
/// no runtime is present (test teardown, synchronous shutdown before
/// the executor starts), we skip the save with a DEBUG rather than
/// block the calling thread.
impl Drop for OpenAiResponsesProvider {
    fn drop(&mut self) {
        // Take ownership so the values can be moved into the closure.
        let jar = match self.cookie_jar.take() {
            Some(j) => j,
            None => return,
        };
        let path = match self.cookie_path.take() {
            Some(p) => p,
            None => return,
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Fire-and-forget: the JoinHandle is dropped; the task
                // runs to completion on the blocking thread pool even
                // after the provider is gone.
                handle.spawn_blocking(move || {
                    if let Err(e) = cookies::save_jar(&jar, &path) {
                        tracing::debug!(
                            path = %path.display(),
                            error = %e,
                            "openai-responses: cookie jar persist failed; continuing"
                        );
                    }
                });
            }
            Err(_) => {
                // No runtime available (test teardown, sync shutdown).
                // Skip rather than block the calling thread. The next
                // boot will start with a cold jar, which is acceptable.
                tracing::debug!(
                    "openai-responses: no tokio runtime in Drop; skipping cookie jar persist (best-effort)"
                );
            }
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
            // Capture the headers BEFORE `resp.text()` moves the body;
            // the shared mapper reads the rate-limit hint off them.
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &self.cfg.auth_kind,
                &body_text,
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
            Some("response.completed") | Some("response.incomplete")
        );
        if backfill_terminal && !accumulated_items.is_empty() {
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
            // Capture the headers BEFORE `resp.text()` moves the body;
            // the shared mapper reads the rate-limit hint off them.
            let headers = resp.headers().clone();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(map_responses_upstream_error(
                &self.cfg.id,
                status,
                &headers,
                &self.cfg.auth_kind,
                &body_text,
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
}

fn build_error_excerpt(body_text: &str) -> String {
    serde_json::from_str::<Value>(body_text)
        .ok()
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sanitize_upstream_body(body_text))
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
/// (`resp.text()` moves the body). This ordering is a programmer
/// convention: `headers` is an owned `HeaderMap` clone here, so the
/// compiler does not couple it to the body move. Both call sites clone
/// the headers before calling `resp.text()`.
fn map_responses_upstream_error(
    provider_id: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    auth_kind: &AuthKind,
    body_text: &str,
) -> Error {
    // Reset hint from response headers, gated on rate-limit statuses so a
    // stray Retry-After on a 400 doesn't park the provider.
    let header_hint = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(headers)
    } else {
        None
    };
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, body_text);
    let msg = build_error_excerpt(body_text);
    let safe_excerpt = sanitize_for_log(&msg);
    crate::upstream_log::warn_upstream_failure(
        provider_id,
        status,
        Some(auth_kind),
        &safe_excerpt,
        "openai-responses",
    );
    // The Codex usage-limit body wins over the header hint: it carries
    // the 5-hour-cap reset, which Retry-After does not.
    let codex_hint = serde_json::from_str::<Value>(body_text)
        .ok()
        .and_then(|v| crate::openai_responses::response::codex_reset_hint(&v));
    let retry_after = codex_hint.or(header_hint);
    // When the upstream returned a structured `{error:...}` JSON envelope,
    // carry the RAW body so the ingress sanitizer can re-extract the
    // upstream's own top-level `error.message` and surface it to the
    // client. Otherwise carry the sanitized excerpt so a non-`{error}`
    // body falls back to a status-only client message -- never a raw dump.
    let err_body = if is_json_error_envelope(body_text) {
        body_text.to_string()
    } else {
        msg
    };
    Error::upstream_with_retry_after(provider_id, status, err_body, retry_after)
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

    // NOTE: tracing-test's `#[traced_test]` installs a GLOBAL default
    // subscriber; a future test in this crate that calls
    // `set_global_default` (instead of the thread-local `with_default`)
    // would pre-empt these `logs_contain` / `logs_assert` checks into
    // false-passes. Keep new log-asserting tests on `#[traced_test]`.
    use tracing_test::traced_test;

    fn make_provider(base_url: &str) -> OpenAiResponsesProvider {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:test".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct-uuid".into()),
            base_url: base_url.to_string(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: Vec::new(),
            user_agent: None,
            session_id: None,
        };
        OpenAiResponsesProvider::new(cfg)
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gpt-5-codex".into(),
            messages: vec![routectl_core::Message {
                refusal: None,
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

    /// Pin: when the model hits `max_output_tokens` the Responses API
    /// emits `response.incomplete` (status "incomplete",
    /// incomplete_details.reason "max_output_tokens") as the terminal
    /// SSE event. `complete()` must treat it as a successful
    /// truncated completion -- return Ok(ChatResponse) with
    /// finish_reason="length" and usage populated -- NOT an
    /// "stream ended without a terminal event" error. Mirrors the
    /// streaming `stream()` path (handle_incomplete -> handle_completed).
    #[tokio::test]
    async fn complete_response_incomplete_returns_length_finish_reason() {
        // Arrange
        let server = MockServer::start().await;
        let incomplete_body = serde_json::json!({
            "id": "resp_inc",
            "object": "response",
            "status": "incomplete",
            "model": "gpt-5-codex",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "partial"}]
            }],
            "usage": {"input_tokens": 5, "output_tokens": 64, "total_tokens": 69}
        });
        let event_body = format!(
            "data: {{\"type\":\"response.incomplete\",\"response\":{}}}\n\n",
            serde_json::to_string(&incomplete_body).unwrap()
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
        let resp = provider
            .complete(base_req())
            .await
            .expect("incomplete must yield Ok, not a terminal-event error");

        // Assert: truncation maps to finish_reason="length" and usage
        // survives.
        assert_eq!(resp.id, "resp_inc");
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "partial"),
            other => panic!("expected Text, got {other:?}"),
        }
        let usage = resp.usage.expect("usage present on incomplete response");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 64);
    }

    /// Pin: `response.incomplete` whose terminal body carries an empty
    /// `output` array backfills from accumulated `output_item.done`
    /// events, same as the `response.completed` path -- so a truncated
    /// streamed turn still surfaces its content.
    #[tokio::test]
    async fn complete_incomplete_backfills_output_from_item_done_events() {
        // Arrange
        let server = MockServer::start().await;
        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "streamed"}]
            }
        });
        // Terminal incomplete event with an EMPTY output array (the
        // chatgpt-oauth backend pattern).
        let incomplete_body = serde_json::json!({
            "id": "resp_inc2",
            "object": "response",
            "status": "incomplete",
            "model": "gpt-5-codex",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
            "usage": {"input_tokens": 3, "output_tokens": 32, "total_tokens": 35}
        });
        let event_body = format!(
            "data: {}\n\ndata: {{\"type\":\"response.incomplete\",\"response\":{}}}\n\n",
            serde_json::to_string(&item_done).unwrap(),
            serde_json::to_string(&incomplete_body).unwrap()
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

        // Assert: the backfilled item content surfaces; finish is length.
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
        match &resp.choices[0].message.content {
            MessageContent::Text(t) => assert_eq!(t, "streamed"),
            other => panic!("expected backfilled Text, got {other:?}"),
        }
    }

    /// Pin: `complete()` caps the `output_item.done` accumulator at
    /// `sse::MAX_OUTPUT_BLOCKS`, mirroring the stream path's bounded-
    /// growth guard. An upstream that ships more done-items than the cap
    /// (adversarial or extreme) must NOT error -- the call truncates the
    /// overflow with a debug log and returns Ok, so large-but-legit
    /// responses below the cap still surface.
    #[traced_test]
    #[tokio::test]
    async fn complete_caps_accumulated_output_items_and_logs() {
        // Arrange: build an SSE body programmatically with one more
        // done-item than the cap, followed by a terminal response.completed
        // carrying an empty output array (the chatgpt-oauth backfill
        // pattern). The accumulator must stop at MAX_OUTPUT_BLOCKS.
        let server = MockServer::start().await;
        let overflow = super::sse::MAX_OUTPUT_BLOCKS + 3;
        let mut sse = String::new();
        for i in 0..overflow {
            let item_done = serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": format!("msg_{i}"),
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": format!("t{i}")}]
                }
            });
            sse.push_str(&format!(
                "data: {}\n\n",
                serde_json::to_string(&item_done).unwrap()
            ));
        }
        let completed_body = serde_json::json!({
            "id": "resp_cap",
            "object": "response",
            "status": "completed",
            "model": "gpt-5-codex",
            "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        sse.push_str(&format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
            serde_json::to_string(&completed_body).unwrap()
        ));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        let provider = make_provider(&server.uri());

        // Act: overflow must truncate, not error.
        let resp = provider
            .complete(base_req())
            .await
            .expect("overflow must yield Ok, not an error");

        // Assert: finishes normally and the cap debug log fired.
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(
            logs_contain("output_item.done beyond cap"),
            "the accumulator cap debug log must fire on overflow"
        );
        // Direct boundary check: the cap log must fire EXACTLY
        // overflow - MAX_OUTPUT_BLOCKS (== 3) times -- one per skipped
        // item past the cap. This pins the `>=` guard at exactly
        // MAX_OUTPUT_BLOCKS: flipping it to `>` keeps one extra item, so
        // only 2 items overflow and the count drops to 2 (RED).
        let expected_skips = overflow - super::sse::MAX_OUTPUT_BLOCKS;
        logs_assert(|lines: &[&str]| {
            let skips = lines
                .iter()
                .filter(|l| l.contains("output_item.done beyond cap"))
                .count();
            if skips == expected_skips {
                Ok(())
            } else {
                Err(format!(
                    "cap log fired {skips} times; expected exactly {expected_skips} \
                     (accumulator must cap at exactly MAX_OUTPUT_BLOCKS)"
                ))
            }
        });
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
// Excerpt-sanitization tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod excerpt_tests {
    use super::{build_error_excerpt, map_responses_upstream_error, AuthKind};
    use reqwest::header::HeaderMap;
    use routectl_core::{sanitize_for_log, Error};

    #[test]
    fn excerpt_sanitizes_crlf_and_ansi() {
        let body = "boom\r\n[fake INFO] injected\x1b[31mred";
        let msg = build_error_excerpt(body);
        let safe_excerpt = sanitize_for_log(&msg);
        assert!(
            !safe_excerpt.contains('\r'),
            "CR in excerpt: {safe_excerpt:?}"
        );
        assert!(
            !safe_excerpt.contains('\n'),
            "LF in excerpt: {safe_excerpt:?}"
        );
        assert!(
            !safe_excerpt.contains('\x1b'),
            "ESC in excerpt: {safe_excerpt:?}"
        );
    }

    /// The shared mapper drives both `complete()` and `stream()`. A plain
    /// rate-limit body with a parseable `Retry-After` must surface that
    /// reset on the canonical error from the single helper.
    #[test]
    fn map_upstream_error_preserves_retry_after_for_both_callers() {
        // Arrange: a 429 with a header reset hint, no codex body hint.
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        let body = r#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#;

        // Act
        let err = map_responses_upstream_error("p", 429, &headers, &AuthKind::ApiKey, body);

        // Assert
        match err {
            Error::Upstream {
                status,
                retry_after,
                body,
                ..
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
                assert!(
                    body.contains("slow down"),
                    "message must reach body: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// The Codex usage-limit body carries the 5-hour-cap reset and must
    /// win over the header `Retry-After`. Proves the codex-hint resolution
    /// stays INSIDE the extracted helper for both callers.
    #[test]
    fn map_upstream_error_codex_body_hint_wins_over_header() {
        // Arrange: a header hint of 30s AND a codex usage-limit body whose
        // resets_in_seconds is 7200 -- the body must win.
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        let body = r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7200,"message":"capped"}}"#;

        // Act
        let err = map_responses_upstream_error("p", 429, &headers, &AuthKind::ChatgptOauth, body);

        // Assert: the body's 7200s reset, not the header's 30s.
        match err {
            Error::Upstream { retry_after, .. } => {
                assert_eq!(
                    retry_after,
                    Some(std::time::Duration::from_secs(7200)),
                    "codex body hint must override the header Retry-After"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
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

// ---------------------------------------------------------------------------
// Header-merge tests (header_extras passthrough + generated identity headers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod header_merge_tests {
    use super::*;
    use routectl_core::{ChatRequest, MessageContent, StaticToken, TokenSource};
    use std::sync::Arc;

    fn oauth_provider_with_extras(extras: Vec<(String, String)>) -> OpenAiResponsesProvider {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:hm-test".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct-uuid".into()),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: extras,
            user_agent: None,
            session_id: None,
        };
        OpenAiResponsesProvider::new(cfg)
    }

    /// Build a ChatgptOauth provider carrying an optional `session_id`,
    /// with empty `header_extras`. Used by the codex session-id tests.
    fn oauth_provider_with_session(session_id: Option<String>) -> OpenAiResponsesProvider {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:hm-session".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct-uuid".into()),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind: AuthKind::ChatgptOauth,
            header_extras: Vec::new(),
            user_agent: None,
            session_id,
        };
        OpenAiResponsesProvider::new(cfg)
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gpt-5-codex".into(),
            messages: vec![routectl_core::Message {
                refusal: None,
                role: routectl_core::Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(32),
            ..Default::default()
        }
    }

    fn header_vals(request: &reqwest::Request, name: &str) -> Vec<String> {
        request
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect()
    }

    /// On the ChatgptOauth path, an operator `header_extras` entry for
    /// `originator` now FLOWS THROUGH to the wire (the old fingerprint
    /// guard that dropped it is gone). Identity / fingerprint values are
    /// the operator's responsibility via config.
    #[test]
    fn chatgpt_oauth_header_extras_originator_reaches_wire() {
        // Arrange
        let provider = oauth_provider_with_extras(vec![(
            "originator".to_string(),
            "operator-value".to_string(),
        )]);
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let rb = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: the operator's originator value is on the wire.
        assert_eq!(
            header_vals(&request, "originator"),
            vec!["operator-value".to_string()],
            "operator originator from header_extras must reach the wire",
        );
    }

    /// On the ApiKey path, `header_extras` pass through the normal
    /// auth-guard merge. (There is no fingerprint filter anymore, so
    /// this is just the standard `is_auth_header` / `is_managed_header`
    /// behavior.)
    #[test]
    fn api_key_header_extras_not_blocked_by_fingerprint_filter() {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:hm-apikey".into(),
            auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
            account_id: None,
            base_url: "https://api.openai.com/v1".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: vec![("version".to_string(), "custom-1.0".to_string())],
            user_agent: None,
            session_id: None,
        };
        let provider = OpenAiResponsesProvider::new(cfg);
        let rb = provider.client.post("https://api.openai.com/v1/responses");

        let rb = provider
            .build_headers(rb, &base_req(), "sk-test")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // `version` header from extras must be present on api-key path.
        assert!(
            header_vals(&request, "version")
                .iter()
                .any(|v| v == "custom-1.0"),
            "version extra must pass through on api-key path; got: {:?}",
            header_vals(&request, "version"),
        );
    }

    /// thread-id rotates per request, and within a single request
    /// thread-id == x-client-request-id.
    #[test]
    fn thread_id_rotates_per_request_and_matches_x_client_request_id() {
        // Arrange
        let provider = oauth_provider_with_extras(Vec::new());

        // Act: two consecutive build_headers calls on the same provider.
        let req_a = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");
        let req_b = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");

        // Assert: thread-id present, single-valued, rotates per request.
        let tid_a = header_vals(&req_a, "thread-id");
        let tid_b = header_vals(&req_b, "thread-id");
        assert_eq!(tid_a.len(), 1, "thread-id must be single-valued: {tid_a:?}");
        assert_eq!(tid_b.len(), 1, "thread-id must be single-valued: {tid_b:?}");
        assert_ne!(tid_a[0], tid_b[0], "thread-id must rotate per request");

        // Assert: within each request, thread-id == x-client-request-id.
        assert_eq!(
            header_vals(&req_a, "x-client-request-id"),
            tid_a,
            "x-client-request-id must equal thread-id within a request",
        );
        assert_eq!(
            header_vals(&req_b, "x-client-request-id"),
            tid_b,
            "x-client-request-id must equal thread-id within a request",
        );
    }

    /// x-codex-window-id is stable across two requests on the same
    /// provider instance (generated once in `new()`).
    #[test]
    fn window_id_stable_across_requests_on_same_provider() {
        // Arrange
        let provider = oauth_provider_with_extras(Vec::new());

        // Act
        let req_a = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");
        let req_b = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");

        // Assert
        let wid_a = header_vals(&req_a, "x-codex-window-id");
        let wid_b = header_vals(&req_b, "x-codex-window-id");
        assert_eq!(wid_a.len(), 1, "window-id must be single-valued: {wid_a:?}");
        assert_eq!(
            wid_a, wid_b,
            "x-codex-window-id must be stable across requests on the same provider",
        );
    }

    /// Two requests through a ChatgptOauth provider carrying a session_id
    /// stamp the SAME `session-id` (stable per credential), while
    /// thread-id / x-client-request-id stay fresh per request.
    #[test]
    fn session_id_stable_across_requests_while_thread_id_rotates() {
        // Arrange
        let provider = oauth_provider_with_session(Some("session-stable-123".into()));

        // Act
        let req_a = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");
        let req_b = provider
            .build_headers(
                provider.client.post("https://chatgpt.test/responses"),
                &base_req(),
                "test-jwt",
            )
            .expect("build_headers ok")
            .build()
            .expect("build");

        // Assert: session-id is single-valued and identical across requests.
        let sid_a = header_vals(&req_a, "session-id");
        let sid_b = header_vals(&req_b, "session-id");
        assert_eq!(sid_a, vec!["session-stable-123".to_string()]);
        assert_eq!(
            sid_a, sid_b,
            "session-id must be stable across requests on one credential",
        );

        // Assert: the per-request identity headers still rotate.
        let tid_a = header_vals(&req_a, "thread-id");
        let tid_b = header_vals(&req_b, "thread-id");
        assert_ne!(tid_a[0], tid_b[0], "thread-id must rotate per request");
        assert_ne!(
            header_vals(&req_a, "x-client-request-id")[0],
            header_vals(&req_b, "x-client-request-id")[0],
            "x-client-request-id must rotate per request",
        );
    }

    /// A provider with `session_id == None` stamps no `session-id` header.
    #[test]
    fn no_session_id_stamps_no_session_header() {
        // Arrange
        let provider = oauth_provider_with_session(None);
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let request = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok")
            .build()
            .expect("build");

        // Assert
        assert!(
            header_vals(&request, "session-id").is_empty(),
            "session_id None must not stamp a session-id header",
        );
    }

    /// The ApiKey (non-ChatgptOauth) path stamps no `session-id` header,
    /// even when a session_id is somehow set on the config.
    #[test]
    fn api_key_path_stamps_no_session_header() {
        // Arrange: ApiKey config carrying a session_id (which would be
        // None in practice -- the factory only resolves it for
        // ChatgptOauth -- but proves the path gate, not just the value).
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:hm-apikey-session".into(),
            auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
            account_id: None,
            base_url: "https://api.openai.com/v1".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            session_id: Some("session-stable-123".into()),
        };
        let provider = OpenAiResponsesProvider::new(cfg);
        let rb = provider.client.post("https://api.openai.com/v1/responses");

        // Act
        let request = provider
            .build_headers(rb, &base_req(), "sk-test")
            .expect("build_headers ok")
            .build()
            .expect("build");

        // Assert
        assert!(
            header_vals(&request, "session-id").is_empty(),
            "ApiKey path must not stamp a session-id header",
        );
    }

    /// A `header_extras` entry named `thread-id` is OVERRIDDEN by the
    /// generated per-request value (insert replaces, not appends).
    #[test]
    fn generated_thread_id_overrides_header_extras() {
        // Arrange
        let provider = oauth_provider_with_extras(vec![(
            "thread-id".to_string(),
            "operator-thread".to_string(),
        )]);
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let rb = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: single value, and it is NOT the operator's.
        let tids = header_vals(&request, "thread-id");
        assert_eq!(tids.len(), 1, "thread-id must be single-valued: {tids:?}");
        assert_ne!(
            tids[0], "operator-thread",
            "generated thread-id must override the header_extras value",
        );
    }

    /// On the ApiKey path the three codex identity headers
    /// (thread-id, x-client-request-id, x-codex-window-id) are NOT
    /// injected, even when ChatgptOauth-shaped header_extras are present.
    #[test]
    fn api_key_path_omits_generated_identity_headers() {
        let cfg = OpenAiResponsesConfig {
            id: "openai-responses:hm-apikey-id".into(),
            auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
            account_id: None,
            base_url: "https://api.openai.com/v1".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: vec![("originator".to_string(), "codex_cli_rs".to_string())],
            user_agent: None,
            session_id: None,
        };
        let provider = OpenAiResponsesProvider::new(cfg);
        let rb = provider.client.post("https://api.openai.com/v1/responses");

        let rb = provider
            .build_headers(rb, &base_req(), "sk-test")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: the three generated identity headers are absent on the
        // api-key path. (The originator header_extra DOES pass through --
        // that is the normal merge -- but the generated identity trio
        // must not be auto-injected here.)
        for absent in ["thread-id", "x-client-request-id", "x-codex-window-id"] {
            assert!(
                header_vals(&request, absent).is_empty(),
                "{absent:?} must NOT be injected on the api-key path",
            );
        }
    }

    /// With empty `header_extras`, the compiled codex identity defaults
    /// (originator, residency, version) appear on the outgoing request.
    /// This is the zero-config posture: an operator who sets only
    /// auth_kind + api_key_ref still emits a full codex fingerprint.
    #[test]
    fn defaults_appear_on_wire_with_empty_header_extras() {
        use routectl_core::identity::codex::{
            CODEX_ORIGINATOR, PINNED_CODEX_VERSION, RESIDENCY_HEADER_VALUE,
        };

        // Arrange
        let provider = oauth_provider_with_extras(Vec::new());
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let rb = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: each compiled default lands once with its default value.
        assert_eq!(
            header_vals(&request, "originator"),
            vec![CODEX_ORIGINATOR.to_string()],
            "originator default must appear with empty header_extras",
        );
        assert_eq!(
            header_vals(&request, "x-openai-internal-codex-residency"),
            vec![RESIDENCY_HEADER_VALUE.to_string()],
            "residency default must appear with empty header_extras",
        );
        assert_eq!(
            header_vals(&request, "version"),
            vec![PINNED_CODEX_VERSION.to_string()],
            "version default must appear with empty header_extras",
        );
    }

    /// An operator `header_extras` entry for a default key OVERRIDES the
    /// compiled default: the wire shows the operator value, not the
    /// built-in one, and only once (insert replaces, not appends).
    #[test]
    fn header_extras_overrides_default_originator() {
        // Arrange
        let provider =
            oauth_provider_with_extras(vec![("originator".to_string(), "custom".to_string())]);
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let rb = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: the operator's value wins; the default is gone.
        assert_eq!(
            header_vals(&request, "originator"),
            vec!["custom".to_string()],
            "operator header_extras must override the compiled default",
        );
    }

    /// The per-request UUIDs still override a `header_extras` `thread-id`
    /// even though defaults now run before the header_extras loop. The
    /// UUIDs fire LAST, so they win over both the defaults and any
    /// operator-supplied value.
    #[test]
    fn per_request_uuid_overrides_header_extras_thread_id() {
        // Arrange
        let provider = oauth_provider_with_extras(vec![(
            "thread-id".to_string(),
            "operator-thread".to_string(),
        )]);
        let rb = provider.client.post("https://chatgpt.test/responses");

        // Act
        let rb = provider
            .build_headers(rb, &base_req(), "test-jwt")
            .expect("build_headers ok");
        let request = rb.build().expect("build");

        // Assert: single value, and it is NOT the operator's.
        let tids = header_vals(&request, "thread-id");
        assert_eq!(tids.len(), 1, "thread-id must be single-valued: {tids:?}");
        assert_ne!(
            tids[0], "operator-thread",
            "generated thread-id must override the header_extras value",
        );
    }
}
