//! Anthropic Messages API provider (api.anthropic.com).
//!
//! Wire format: <https://docs.anthropic.com/en/api/messages>
//! Extended thinking: <https://platform.claude.com/docs/en/docs/build-with-claude/extended-thinking>
//!
//! Reasoning normalization:
//! - Request: `reasoning.max_tokens` -> `thinking.budget_tokens`,
//!   `reasoning.effort` -> proportional `budget_tokens`.
//! - Response: content[] thinking blocks -> `reasoning_details[format="anthropic-claude-v1"]`
//!   with signature preserved for multi-turn tool-use continuity.
//! - Multi-turn: thinking blocks are passed back unmodified; signature is mandatory.

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
    StaticToken, TokenCount, TokenSource,
};

pub(crate) mod context_management;
pub(crate) mod parts;
pub mod request;
pub mod response;
pub mod sse;
pub mod sse_opaque;
pub mod sse_unknown;
pub(crate) mod types;
pub(crate) mod types_sse;

/// Provider-kind discriminator string used in tracing fields. See
/// the openai_compat module for the rationale.
const PROVIDER_KIND: &str = "anthropic";

/// Anthropic wire-format tag for reasoning details. A single canonical
/// definition shared by all sub-modules (context_management, request,
/// response, sse) via `super::ANTHROPIC_FORMAT` paths.
pub(crate) const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

use sse::SseState;

/// How the provider authenticates to the Anthropic Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// Standard `x-api-key: <key>` header. Default for `sk-ant-api03-...` keys.
    #[default]
    ApiKey,
    /// OAuth bearer for subscription tokens (e.g. Claude Code's
    /// `sk-ant-oat01-...` access token). Sends `Authorization: Bearer <key>`.
    /// You must declare the `anthropic-beta: oauth-2025-04-20` gate
    /// yourself via `extra_headers` -- routectl no longer auto-injects it.
    OauthBearer,
}

#[derive(Clone)]
pub struct AnthropicApiConfig {
    pub id: String,
    /// Source of the bearer/API-key token. For env/file/literal
    /// secret refs, this is a `StaticToken` resolved once at
    /// construction. For `oauth://<provider>` refs, the factory
    /// passes a `ManagedToken` impl that re-resolves through
    /// `SecretStore::get` per request -- so token rotation in
    /// `~/.config/routectl/credentials.json` is picked up live
    /// without restarting routectl.
    pub auth: Arc<dyn TokenSource>,
    pub base_url: String,
    pub anthropic_version: String,
    pub auth_kind: AuthKind,
    /// Provider-level extra HTTP headers (renamed from `extra_headers`
    /// in v0.6.0). The router's dispatch layer merges this with the
    /// per-model `header_extras` before reaching the egress (see
    /// `Router::merge_header_extras`). Use this to declare
    /// vendor-required headers; `anthropic-beta` flags are
    /// composed dynamically (see `build_headers`).
    pub header_extras: Vec<(String, String)>,
    /// Override the User-Agent on outbound requests. Useful for IAM
    /// policies that gate access on `aws:UserAgent` (e.g. Claude Code's
    /// Bedrock role). `None` keeps reqwest's default UA.
    pub user_agent: Option<String>,
    /// Operator-supplied allowlist for `anthropic_beta` flags.
    /// Empty (default) is pass-through: every beta the client
    /// requests via the `anthropic-beta` HTTP header or body field
    /// reaches api.anthropic.com unchanged. When non-empty, ingress-
    /// lifted values not in the list are dropped at DEBUG level.
    /// Mirrors the Bedrock-egress `[bedrock] allowed_betas` shape so
    /// multi-tenant or API-gateway deployments can constrain which
    /// betas authenticated clients can opt into.
    pub allowed_betas: Vec<String>,
    /// Strict allowlist of inbound `x-claude-code-*` header names the
    /// egress is permitted to forward upstream. The Anthropic ingress
    /// greedy-captures the whole namespace into
    /// `req.routectl_internal.claude_code_headers`; this list is the
    /// operator's filter to pick which captured names actually go to
    /// api.anthropic.com. Empty (default) drops every captured header --
    /// secure-by-default for new providers. Names match
    /// case-insensitively. Values not on the list are dropped at the
    /// egress for defense-in-depth (the ingress capture remains
    /// namespace-bounded so debug surface stays useful even when the
    /// allowlist is empty).
    pub forward_client_headers: Vec<String>,
    /// When true, routectl emulates Anthropic's context-management-2025-06-27
    /// beta server-side for this provider. Set this for non-Anthropic
    /// anthropic-api providers (e.g. DeepSeek's /anthropic surface) that do
    /// not honor the beta natively. Default false: routectl forwards the body
    /// verbatim and the real Anthropic server handles the beta itself.
    pub context_management: bool,
    /// Per-entry byte cap on writes to the thinking cache used by the
    /// `context_management` emulation path. Entries whose serialized JSON
    /// representation exceeds this value are rejected at write time and a
    /// structured WARN is emitted; the strip-thinking-on-miss recovery
    /// in `request.rs` then handles the next turn the same way it would
    /// a TTL eviction. Defaults to
    /// `AnthropicApiConfig::DEFAULT_MAX_THINKING_ENTRY_BYTES`
    /// (256 KB) -- generous for ordinary thinking turns while bounding
    /// the LRU's worst-case footprint.
    pub max_thinking_entry_bytes: usize,
}

impl std::fmt::Debug for AnthropicApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-rolled Debug elides the auth source (its own Debug
        // already redacts, but this saves one round-trip if a
        // future TokenSource impl ever leaks).
        f.debug_struct("AnthropicApiConfig")
            .field("id", &self.id)
            .field("auth", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("anthropic_version", &self.anthropic_version)
            .field("auth_kind", &self.auth_kind)
            .field("header_extras_len", &self.header_extras.len())
            .field("user_agent", &self.user_agent)
            .field("allowed_betas_len", &self.allowed_betas.len())
            .field(
                "forward_client_headers",
                &format!("[{} entries]", self.forward_client_headers.len()),
            )
            .field("context_management", &self.context_management)
            .field("max_thinking_entry_bytes", &self.max_thinking_entry_bytes)
            .finish()
    }
}

impl AnthropicApiConfig {
    /// Default per-entry byte cap on the thinking cache. Operators can
    /// override per provider via `[providers.X] max_thinking_entry_bytes`
    /// (anthropic-api kind). See the field docs for the rejection semantics.
    pub const DEFAULT_MAX_THINKING_ENTRY_BYTES: usize =
        context_management::MAX_THINKING_ENTRY_BYTES;

    /// Construct with a static API-key string. The token is wrapped
    /// in `StaticToken` so the provider's resolution call site is
    /// uniform across static and managed sources. Existing callers
    /// (tests, in-tree builders) that pass `"sk-ant-..."` keep their
    /// signatures unchanged.
    pub fn new(id: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new_with_auth(id, Arc::new(StaticToken::new(api_key)))
    }

    /// Construct with a custom `TokenSource`. Used by the factory
    /// when wiring `oauth://<provider>` to a per-request resolver.
    pub fn new_with_auth(id: impl Into<String>, auth: Arc<dyn TokenSource>) -> Self {
        Self {
            id: id.into(),
            auth,
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: Self::DEFAULT_MAX_THINKING_ENTRY_BYTES,
        }
    }
}

pub struct AnthropicApiProvider {
    cfg: AnthropicApiConfig,
    client: Client,
    thinking_cache: std::sync::Arc<std::sync::RwLock<context_management::ThinkingCache>>,
}

impl AnthropicApiProvider {
    pub fn new(cfg: AnthropicApiConfig) -> Self {
        let client = crate::http_client::build(cfg.user_agent.as_deref());
        let thinking_cache = std::sync::Arc::new(std::sync::RwLock::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(context_management::THINKING_CACHE_CAP)
                .expect("THINKING_CACHE_CAP is non-zero"),
        )));
        Self {
            cfg,
            client,
            thinking_cache,
        }
    }

    /// Seed a thinking observation directly into the cache for integration
    /// tests that need a pre-populated cache without driving a full SSE
    /// response. Gated behind the `test-utils` Cargo feature so it is
    /// absent from production builds. Integration tests that call this
    /// must enable `--features test-utils` (or `bedrock,test-utils`).
    #[cfg(feature = "test-utils")]
    pub fn seed_thinking_for_test(
        &self,
        provider_id: &str,
        tool_use_id: &str,
        thinking: Vec<routectl_core::ReasoningDetail>,
    ) {
        context_management::snapshot_to_cache(
            &self.thinking_cache,
            provider_id,
            tool_use_id,
            thinking,
            self.cfg.max_thinking_entry_bytes,
            "test-seed",
        );
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    fn count_tokens_url(&self) -> String {
        format!(
            "{}/v1/messages/count_tokens",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    fn build_headers(
        &self,
        rb: reqwest::RequestBuilder,
        req: &ChatRequest,
        token: &str,
    ) -> reqwest::RequestBuilder {
        let mut rb = rb.header("anthropic-version", &self.cfg.anthropic_version);
        rb = match self.cfg.auth_kind {
            AuthKind::ApiKey => rb.header("x-api-key", token),
            AuthKind::OauthBearer => rb.header("authorization", format!("Bearer {token}")),
        };

        // anthropic-beta composition. The router's dispatch-layer
        // (`Router::merge_header_extras`) is the canonical source: it
        // unions ingress `req.anthropic_beta` + provider
        // header_extras["anthropic-beta"] + model
        // header_extras["anthropic-beta"] and lands the result on
        // `req.anthropic_beta`. For direct library consumers that
        // bypass the router, the config-side
        // `header_extras["anthropic-beta"]` is the only source -- we
        // union it in here too (deduplicated) so a `cfg.header_extras
        // = [("anthropic-beta", "ctx-1m")]` works without a router.
        let mut beta_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut merged_betas: Vec<String> = Vec::new();
        for entry in &req.anthropic_beta {
            let t = entry.trim();
            if !t.is_empty() && beta_seen.insert(t.to_string()) {
                merged_betas.push(t.to_string());
            }
        }
        let config_betas = self
            .cfg
            .header_extras
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        for entry in config_betas.split(',') {
            let t = entry.trim();
            if !t.is_empty() && beta_seen.insert(t.to_string()) {
                merged_betas.push(t.to_string());
            }
        }

        // When context_management emulation is active, strip the
        // `context-management-2025-06-27` beta from the outgoing header.
        // We handle the semantics ourselves (thinking injection, body key
        // strip), so forwarding it to a non-Anthropic upstream that
        // doesn't honour the beta would cause a 400.
        if self.cfg.context_management {
            merged_betas.retain(|b| b != context_management::CONTEXT_MANAGEMENT_BETA);
        }

        if !merged_betas.is_empty() {
            rb = rb.header("anthropic-beta", merged_betas.join(","));
        }

        // Build a per-request HeaderMap for `header_extras` and
        // forwarded client headers. We want one collision policy:
        // FORWARDED CLIENT HEADERS WIN OVER `header_extras` on the
        // same lowercase name. Rationale: the operator opted into
        // client passthrough for that specific name via
        // `forward_client_headers`; the client value is more specific
        // than the operator's static `header_extras` default.
        //
        // reqwest's `RequestBuilder::header()` APPENDS rather than
        // overrides on collision (see `header_sensitive` ->
        // `headers_mut().append(...)` in reqwest 0.12). To express
        // "client wins", we build a HeaderMap explicitly: insert
        // header_extras first, then INSERT (overriding) the client
        // headers on top, then call `rb.headers(map)` ONCE, which
        // uses `replace_headers` semantics (entries in `src` replace
        // entries in `dst` keyed by the same name).
        let mut header_map = reqwest::header::HeaderMap::new();

        // Prefer the router-composed map for non-beta headers; fall
        // back to `self.cfg.header_extras` for library consumers.
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
            if k.eq_ignore_ascii_case("anthropic-beta") || crate::http_client::is_managed_header(k)
            {
                tracing::debug!(
                    provider = %self.cfg.id,
                    header = %k,
                    "dropping managed header from header_extras; composed dynamically by routectl"
                );
                continue;
            }
            insert_header(&mut header_map, &self.cfg.id, k, v);
        }

        // Forward inbound X-Claude-Code-* headers per the operator's
        // allowlist. The ingress greedy-captures the whole namespace;
        // this step filters down to operator-blessed names. Empty list
        // = drop all, which is the secure-by-default posture for new
        // providers. Client values OVERRIDE any header_extras entry
        // with the same name (see comment above).
        if !self.cfg.forward_client_headers.is_empty() {
            for (name, val) in &req.routectl_internal.claude_code_headers {
                let lc = name.to_ascii_lowercase();
                if self
                    .cfg
                    .forward_client_headers
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&lc))
                {
                    insert_header(&mut header_map, &self.cfg.id, name.as_str(), val.as_str());
                }
            }
        }

        if !header_map.is_empty() {
            rb = rb.headers(header_map);
        }
        rb
    }
}

/// Insert a header name+value into a `HeaderMap`, replacing any
/// existing entry with the same (case-insensitive) name. Skips the
/// entry with a WARN if either the name or value cannot be parsed
/// into the http-crate types -- an invalid value would otherwise
/// poison `RequestBuilder::headers()`'s merge.
fn insert_header(map: &mut reqwest::header::HeaderMap, provider_id: &str, name: &str, value: &str) {
    let header_name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                provider = %provider_id,
                header = %name,
                error = %e,
                "skipping malformed header name",
            );
            return;
        }
    };
    let header_value = match reqwest::header::HeaderValue::from_str(value) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                provider = %provider_id,
                header = %name,
                error = %e,
                "skipping malformed header value",
            );
            return;
        }
    };
    map.insert(header_name, header_value);
}

#[async_trait]
impl Provider for AnthropicApiProvider {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn normalize_request(&self, req: &ChatRequest) -> Result<Value> {
        request::normalize(
            &self.cfg.id,
            req,
            req.routectl_internal.supports_adaptive_thinking,
            &self.cfg.allowed_betas,
            self.cfg.context_management,
            if self.cfg.context_management {
                Some(&*self.thinking_cache)
            } else {
                None
            },
        )
    }

    fn normalize_response(&self, raw: Value) -> Result<ChatResponse> {
        response::normalize(&self.cfg.id, raw)
    }

    /// Stateless single-frame parse. For full streaming use stream().
    fn normalize_chunk(&self, raw: &str) -> Result<Option<ChatChunk>> {
        sse::parse_stateless(&self.cfg.id, raw)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut body = self.normalize_request(&req)?;
        // Ensure stream is absent / false for the non-streaming path.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("stream");
            // `api.anthropic.com` (especially the OAuth-Bearer
            // flavor) rejects `anthropic_beta` as a top-level body
            // field with `Extra inputs are not permitted`. Betas
            // travel on the `anthropic-beta` HTTP header
            // (build_headers emits the merged value). Bedrock's
            // body-shape egress keeps the field via its own assembly
            // path, so this strip is scoped to the api.anthropic.com
            // egress.
            obj.remove("anthropic_beta");
        }

        // Emit the outgoing body at trace level so a grep by
        // request_id correlates ingress -> egress -> upstream
        // response in one pass during triage. Gated by the
        // `tracing::Level::TRACE` filter -- production with default
        // info level pays nothing.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution: for static refs this hits
        // the in-memory `StaticToken` cache; for `oauth://<provider>`
        // refs this dives into `OAuthStore` and resolves the current
        // value (including the v0.7+ refresh path landing in a prior change).
        let token = self.cfg.auth.token().await?;

        let request = self
            .build_headers(self.client.post(self.messages_url()), &req, &token)
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) from the built
        // request -- auth is only present after build_headers applies
        // the resolved token. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        // On non-2xx, read the body as text FIRST so a non-JSON
        // upstream response (HTML 502 from a misconfigured proxy,
        // a CDN cleartext "rate limited" page, Anthropic's
        // occasional plain-text 529 "overloaded") doesn't get
        // collapsed into an opaque serde error. JSON parse is
        // attempted opportunistically to lift `error.message`; on
        // parse failure we fall back to a sanitized text excerpt
        // matching the openai-compat / bedrock pattern. Operators
        // grepping `body_excerpt=...` get a consistent shape across
        // providers.
        if status >= 400 {
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            // Extend the auth-only WARN to all 4xx/5xx so an operator
            // never has to guess WHY a request failed. Auth failures
            // keep the auth_kind field for parity with the documented
            // log shape; other errors get a generic "anthropic
            // upstream error" tag. Sanitize before tracing: the
            // upstream may return attacker-controlled bytes (CRLF,
            // control chars, very long lines) that would otherwise
            // forge log lines on text-format subscribers.
            let safe_excerpt = sanitize_for_log(&msg);
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %safe_excerpt,
                    "anthropic upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %safe_excerpt,
                    "anthropic upstream error",
                );
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume (resp.json() takes ownership). Opt-in via
        // ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        // Trace upstream success body pre-normalize.
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);
        // Clone the raw body before normalize consumes it. Only pay the
        // allocation cost on the context_management emulation path; the
        // default false path skips the clone entirely.
        let raw_for_cache = if self.cfg.context_management {
            Some(raw_body.clone())
        } else {
            None
        };
        let mut chat_resp = self.normalize_response(raw_body)?;
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        // Context-management cache write. Extracts (tool_use_id, thinking)
        // pairs from the upstream content blocks and inserts them into the
        // shared thinking cache for re-injection on the next turn. The write
        // lock is acquired synchronously here -- no .await after this point --
        // so it is never held across an async yield.
        if let Some(raw) = raw_for_cache {
            let blocks: Vec<types::ContentBlock> = raw
                .pointer("/content")
                .and_then(|v| serde_json::from_value::<Vec<types::ContentBlock>>(v.clone()).ok())
                .unwrap_or_default();
            let pairs = context_management::extract_tool_thinking(&blocks);
            for (tool_use_id, thinking) in pairs {
                context_management::snapshot_to_cache(
                    &self.thinking_cache,
                    &self.cfg.id,
                    &tool_use_id,
                    thinking,
                    self.cfg.max_thinking_entry_bytes,
                    "complete",
                );
            }
        }
        Ok(chat_resp)
    }

    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let mut body = self.normalize_request(&req)?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
            // See note on the complete() path: api.anthropic.com
            // rejects `anthropic_beta` as a body field; the HTTP
            // header carries them via build_headers.
            obj.remove("anthropic_beta");
        }

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.cfg.auth.token().await?;

        let request = self
            .build_headers(self.client.post(self.messages_url()), &req, &token)
            .header("content-type", "application/json")
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
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- see comment there. Helper extracted at
            // `read_anthropic_error`. Sanitize the excerpt for the
            // same reason as `complete()`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            let safe_excerpt = sanitize_for_log(&msg);
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %safe_excerpt,
                    "anthropic upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %safe_excerpt,
                    "anthropic upstream error",
                );
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream. The stream path had no dir-3 capture
        // before; this closes the gap so it matches the complete() path.
        // Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        let provider_id = self.cfg.id.clone();
        let byte_stream = resp.bytes_stream();
        let event_stream = byte_stream.eventsource();
        // Capture the context_management flag and a shared reference to
        // the thinking cache so the post-stream write tail can drain
        // pending_cache_writes synchronously without holding the lock
        // across any await point.
        let context_management_enabled = self.cfg.context_management;
        let thinking_cache_for_stream = Arc::clone(&self.thinking_cache);
        let max_thinking_entry_bytes_for_stream = self.cfg.max_thinking_entry_bytes;

        let stream = async_stream::stream! {
            let mut state = SseState::new(&provider_id);

            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        yield Err(Error::Streaming(e.to_string()));
                        return;
                    }
                    Ok(event) => {
                        let trimmed = event.data.trim();
                        // OpenRouter's `/v1/messages` endpoint appends
                        // an OpenAI-style `data: [DONE]` sentinel after
                        // `message_stop`. Real api.anthropic.com does
                        // not emit it. Treat it as a clean EOS: skip
                        // it (parse_event would fail with
                        // `bad sse json: expected value at line 1
                        // column 2`) and return so the outer stream
                        // ends naturally, letting the egress wrapper
                        // mark_clean_close and report the actual
                        // finish_reason instead of synthesizing
                        // `truncated`. Mirrors `openai_compat::stream`.
                        if trimmed == "[DONE]" {
                            tracing::debug!(
                                provider = %provider_id,
                                "anthropic-api stream: received OpenAI-style \
                                 [DONE] sentinel after message_stop (typical of \
                                 OpenRouter's /v1/messages passthrough); \
                                 closing stream cleanly"
                            );
                            break;
                        }
                        // Keepalive comment line or empty data field.
                        if trimmed.is_empty() {
                            continue;
                        }
                        match state.parse_event(&provider_id, &event.data) {
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                            Ok(Some(chunk)) => yield Ok(chunk),
                            Ok(None) => {}
                        }
                    }
                }
            }
            // Post-stream cache-write tail for context_management emulation.
            // Drains pending_cache_writes accumulated during SSE parsing into
            // the thinking cache. Each call to snapshot_to_cache acquires and
            // releases the write lock synchronously -- no await points here.
            if context_management_enabled && !state.pending_cache_writes.is_empty() {
                for (tool_use_id, thinking) in state.pending_cache_writes.drain(..) {
                    context_management::snapshot_to_cache(
                        &thinking_cache_for_stream,
                        &provider_id,
                        &tool_use_id,
                        thinking,
                        max_thinking_entry_bytes_for_stream,
                        "stream",
                    );
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

    /// `POST /v1/messages/count_tokens` -- a probe call that returns
    /// the input-token count for a request without invoking model
    /// inference. claude-code uses this for context-budget display.
    /// Wire reference:
    /// <https://docs.anthropic.com/en/api/messages-count-tokens>
    ///
    /// Body assembly: `normalize_request` produces a fully-built
    /// `/v1/messages` body. We then BUILD the count_tokens body from
    /// scratch using only the allowlist of fields the count_tokens
    /// endpoint accepts (per the Anthropic docs URL above):
    /// `model`, `messages`, `system`, `tools`, `tool_choice`,
    /// `thinking`, `mcp_servers`, `metadata`. This is more defensive
    /// than strip-by-blocklist: a future addition to
    /// `normalize_request` (e.g. `output_config.format`, which IS
    /// rejected by `/v1/messages/count_tokens`) won't accidentally
    /// leak into count_tokens.
    ///
    /// Headers are identical to `complete()` (anthropic-beta union,
    /// anthropic-version, header_extras, X-Claude-Code-* allowlist
    /// filter, auth) -- so a count_tokens call observes the same
    /// merged beta surface as the messages endpoint.
    #[tracing::instrument(skip_all, fields(provider = %self.cfg.id, model = %sanitize_for_log(&req.model)))]
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        let normalized = self.normalize_request(&req)?;
        let body = build_count_tokens_body(&normalized);

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.cfg.auth.token().await?;

        let request = self
            .build_headers(self.client.post(self.count_tokens_url()), &req, &token)
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        // Dir 2: outgoing request headers (incl. auth) for the
        // count_tokens probe. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::outgoing(PROVIDER_KIND, &self.cfg.id, request.headers());
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            // Same text-first-then-opportunistic-JSON pattern as
            // `complete()` -- a non-JSON 502/503 from a misconfigured
            // proxy must not collapse to an opaque serde error.
            // Helper extracted at `read_anthropic_error`.
            let (msg, err) = read_anthropic_error(&self.cfg.id, status, resp).await;
            // Sanitize before tracing: the upstream may return
            // attacker-controlled bytes (CRLF, control chars, very
            // long lines) and `body_excerpt = %msg` would otherwise
            // emit them verbatim into operator logs. Same posture as
            // the `complete()` and `stream()` paths above.
            let safe_excerpt = sanitize_for_log(&msg);
            if status == 401 || status == 403 {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    auth_kind = ?self.cfg.auth_kind,
                    body_excerpt = %safe_excerpt,
                    "anthropic count_tokens upstream auth failed",
                );
            } else {
                tracing::warn!(
                    provider = %self.cfg.id,
                    status,
                    body_excerpt = %safe_excerpt,
                    "anthropic count_tokens upstream error",
                );
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume. Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        let raw_body: Value = resp
            .json()
            .await
            .map_err(|e| Error::upstream(&self.cfg.id, status, e.to_string()))?;
        trace_upstream_success_body(PROVIDER_KIND, &self.cfg.id, &raw_body);
        let token_count: TokenCount = serde_json::from_value(raw_body).map_err(|e| {
            Error::normalize_response(&self.cfg.id, format!("count_tokens response parse: {e}"))
        })?;
        Ok(token_count)
    }

    /// Forward upstream-401 to the underlying token source so an
    /// `oauth://` ref can force-refresh through the OAuth store's
    /// per-provider single-flight gate. Static-auth providers
    /// (`env://`, `file://`, `literal:`) inherit the no-op default
    /// from `TokenSource::on_auth_failure`. Errors propagate so the
    /// router surfaces an actionable auth message rather than walking
    /// the fallback chain over a dead OAuth identity.
    async fn on_auth_failure(&self) -> Result<()> {
        self.cfg.auth.on_auth_failure().await
    }
}

/// Read a 4xx/5xx upstream response body and build a routectl
/// `Error::Upstream` from it. Encapsulates the
/// "text-first-then-opportunistic-JSON" pattern shared by
/// `complete()`, `stream()`, and `count_tokens()`: a non-JSON
/// upstream response (HTML 502 from a misconfigured proxy, a CDN
/// cleartext error page, plain-text 529) must not collapse to an
/// opaque serde error. Returns both the parsed message (for the
/// caller's `body_excerpt` log) and the constructed `Error::Upstream`.
async fn read_anthropic_error(
    provider_id: &str,
    status: u16,
    resp: reqwest::Response,
) -> (String, Error) {
    let body_text = resp.text().await.unwrap_or_default();
    // Emit the FULL upstream error body at debug level so triage
    // doesn't have to reproduce. The caller's WARN excerpt stays
    // unchanged for `routectl-warn.log` scannability.
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, &body_text);
    let msg = serde_json::from_str::<Value>(&body_text)
        .ok()
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sanitize_upstream_body(&body_text));
    let err = Error::upstream(provider_id, status, msg.clone());
    (msg, err)
}

/// Build the body for `POST /v1/messages/count_tokens` from the
/// already-normalized `/v1/messages` body. Only the explicit allowlist
/// of fields the count_tokens endpoint accepts (per
/// <https://docs.anthropic.com/en/api/messages-count-tokens>) gets
/// copied through:
/// `model`, `messages`, `system`, `tools`, `tool_choice`, `thinking`,
/// `mcp_servers`, `metadata`.
///
/// This is more defensive than strip-by-blocklist: future additions
/// to `normalize_request` (e.g. `output_config.format`, which IS
/// rejected by some Anthropic endpoints) won't accidentally leak
/// into count_tokens.
fn build_count_tokens_body(normalized: &Value) -> Value {
    const ALLOWED: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "mcp_servers",
        "metadata",
    ];
    let mut out = serde_json::Map::new();
    let Some(src) = normalized.as_object() else {
        return Value::Object(out);
    };
    for &k in ALLOWED {
        if let Some(v) = src.get(k) {
            if !v.is_null() {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allowlist of body fields accepted by Anthropic's
    /// `/v1/messages/count_tokens` endpoint. Pulled from
    /// <https://docs.anthropic.com/en/api/messages-count-tokens>.
    /// Pinning the list as a const lets the test assert that no
    /// extra fields leak into the count_tokens body even when
    /// `normalize_request` is extended.
    const COUNT_TOKENS_ALLOWED_FIELDS: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "mcp_servers",
        "metadata",
    ];

    /// Pin: `build_count_tokens_body` copies ONLY the allowlist
    /// fields, even when `normalize_request` produces extra keys.
    /// Without this contract, a future field added to
    /// `normalize_request` (e.g. `output_config`) silently flows
    /// into `/v1/messages/count_tokens` and the upstream 400s with
    /// `Extra inputs are not permitted`.
    #[test]
    fn build_count_tokens_body_only_emits_allowlist_fields() {
        let normalized = serde_json::json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
            "system": "you are helpful",
            "tools": [{"name": "calculator", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "mcp_servers": [{"name": "s1", "url": "https://mcp.example.com"}],
            "metadata": {"user_id": "u_42"},
            // Fields below MUST NOT reach the upstream count_tokens body:
            "stream": true,
            "max_tokens": 4096,
            "anthropic_beta": ["context-1m-2025-08-07"],
            "temperature": 0.7,
            "top_p": 0.9,
            "stop_sequences": ["</block>"],
            "output_config": {"format": {"type": "json_schema"}},
        });

        let body = build_count_tokens_body(&normalized);
        let obj = body.as_object().expect("count_tokens body must be object");
        for k in obj.keys() {
            assert!(
                COUNT_TOKENS_ALLOWED_FIELDS.contains(&k.as_str()),
                "count_tokens body must only emit allowlist fields, found: {k}"
            );
        }
        // Allowlist fields that ARE present in the input must round-trip.
        assert_eq!(obj["model"], "claude-haiku-4-5");
        assert_eq!(obj["system"], "you are helpful");
        assert_eq!(obj["tools"][0]["name"], "calculator");
        assert_eq!(obj["thinking"]["type"], "enabled");
        assert_eq!(obj["metadata"]["user_id"], "u_42");
    }

    /// Allowlist fields not present on the input must NOT be synthesized
    /// (e.g. `mcp_servers: null`); the helper only forwards keys that
    /// existed and were non-null in the normalized body.
    #[test]
    fn build_count_tokens_body_skips_absent_allowlist_fields() {
        let normalized = serde_json::json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let body = build_count_tokens_body(&normalized);
        let obj = body.as_object().expect("body is object");
        assert!(obj.contains_key("model"));
        assert!(obj.contains_key("messages"));
        assert!(!obj.contains_key("system"));
        assert!(!obj.contains_key("tools"));
        assert!(!obj.contains_key("tool_choice"));
        assert!(!obj.contains_key("thinking"));
        assert!(!obj.contains_key("mcp_servers"));
        assert!(!obj.contains_key("metadata"));
    }

    /// Drive `build_headers` end-to-end and return the assembled
    /// outbound HTTP header names (lowercased) so allowlist tests can
    /// assert which `x-claude-code-*` entries reached the wire.
    /// Building the `RequestBuilder` does no I/O; `.build()` just
    /// constructs the `reqwest::Request` object.
    fn outbound_header_names(provider: &AnthropicApiProvider, req: &ChatRequest) -> Vec<String> {
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        let rb = provider.build_headers(rb, req, "test-token");
        let request = rb.build().expect("build outbound request");
        request
            .headers()
            .iter()
            .map(|(name, _)| name.as_str().to_ascii_lowercase())
            .collect()
    }

    fn cfg_with_allowlist(forward_client_headers: Vec<String>) -> AnthropicApiConfig {
        AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers,
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::DEFAULT_MAX_THINKING_ENTRY_BYTES,
        }
    }

    fn req_with_claude_code_headers(pairs: Vec<(&str, &str)>) -> ChatRequest {
        let mut req = ChatRequest::default();
        // RoutectlInternal is `#[non_exhaustive]`, so we mutate the
        // single field we need on the default-constructed value rather
        // than using a struct expression with `..default()`.
        req.routectl_internal.claude_code_headers = pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        req
    }

    /// Empty allowlist drops every captured `x-claude-code-*` header.
    /// Secure-by-default: a fresh provider with no operator opt-in MUST
    /// NOT leak inbound attribution headers to api.anthropic.com.
    #[test]
    fn forward_client_headers_empty_drops_everything() {
        let cfg = cfg_with_allowlist(Vec::new());
        let provider = AnthropicApiProvider::new(cfg);
        let req = req_with_claude_code_headers(vec![
            ("x-claude-code-session-id", "abc"),
            ("x-claude-code-agent-id", "xyz"),
        ]);
        let names = outbound_header_names(&provider, &req);
        assert!(
            !names.iter().any(|n| n.starts_with("x-claude-code-")),
            "empty allowlist must drop every captured header; got: {names:?}"
        );
    }

    /// Names on the allowlist pass through verbatim (case preserved as
    /// sent by the client). The egress emits the inbound name string,
    /// not a normalized version.
    #[test]
    fn forward_client_headers_listed_names_pass_through() {
        let cfg = cfg_with_allowlist(vec!["x-claude-code-session-id".into()]);
        let provider = AnthropicApiProvider::new(cfg);
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let names = outbound_header_names(&provider, &req);
        assert!(
            names.iter().any(|n| n == "x-claude-code-session-id"),
            "allowlisted header must reach outbound; got: {names:?}"
        );
    }

    /// Only allowlisted names reach outbound; unlisted captured headers
    /// are dropped at the egress. This is the core defense-in-depth
    /// posture: inbound capture is namespace-bounded, but the egress
    /// owns the final filter.
    #[test]
    fn forward_client_headers_unlisted_names_dropped() {
        let cfg = cfg_with_allowlist(vec!["x-claude-code-session-id".into()]);
        let provider = AnthropicApiProvider::new(cfg);
        let req = req_with_claude_code_headers(vec![
            ("x-claude-code-session-id", "sid-42"),
            ("x-claude-code-agent-id", "aid-7"),
        ]);
        let names = outbound_header_names(&provider, &req);
        assert!(
            names.iter().any(|n| n == "x-claude-code-session-id"),
            "session-id must pass through; got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "x-claude-code-agent-id"),
            "unlisted agent-id must be dropped; got: {names:?}"
        );
    }

    /// Drive `build_headers` end-to-end and return the value of the
    /// requested header on the assembled outbound request, or `None`
    /// if the header is absent.
    fn outbound_header_value(
        provider: &AnthropicApiProvider,
        req: &ChatRequest,
        name: &str,
    ) -> Option<String> {
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        let rb = provider.build_headers(rb, req, "test-token");
        let request = rb.build().expect("build outbound request");
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Header collision policy: forwarded client headers WIN over
    /// `header_extras` for the same lowercase name. Rationale: the
    /// operator opted into client passthrough for that specific name
    /// via `forward_client_headers`; the client value is more
    /// specific than the operator's static `header_extras` default.
    /// Pre-fix the egress called `RequestBuilder::header()` per entry
    /// which APPENDS; the upstream then saw both values. With the
    /// HeaderMap+`headers()` rebuild, the policy is explicit.
    #[test]
    fn client_forwarded_headers_override_header_extras_on_collision() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: vec![(
                "x-claude-code-session-id".into(),
                "from-operator-config".into(),
            )],
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: vec!["x-claude-code-session-id".into()],
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::DEFAULT_MAX_THINKING_ENTRY_BYTES,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "from-client")]);
        let value = outbound_header_value(&provider, &req, "x-claude-code-session-id")
            .expect("session-id header missing");
        assert_eq!(
            value, "from-client",
            "client-forwarded header must override header_extras on collision; got {value}"
        );
    }
}
