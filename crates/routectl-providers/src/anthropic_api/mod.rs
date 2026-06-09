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
mod extras;
mod messages;
pub(crate) mod parts;
pub mod request;
pub mod response;
pub mod sse;
pub mod sse_opaque;
pub mod sse_unknown;
mod system;
mod tools;
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
    /// The required `anthropic-beta: oauth-2025-04-20` gate is auto-injected
    /// via the pinned beta floor in `build_headers` for the
    /// api.anthropic.com surface -- no manual `extra_headers` needed.
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
    /// Per-entry byte cap on the thinking cache used by the
    /// `context_management` emulation path. Resolved from the optional
    /// `[providers.X].max_thinking_entry_bytes` TOML knob; falls back
    /// to `DEFAULT_MAX_THINKING_ENTRY_BYTES` (1 MiB) when unset.
    pub max_thinking_entry_bytes: usize,
    /// Stable per-credential Claude Code session id, stamped as the
    /// `x-claude-code-session-id` header on the OauthBearer
    /// api.anthropic.com surface. `Some` only when the provider's
    /// `oauth://anthropic` credential carries a session_id minted at
    /// login; resolved once at build time via
    /// `SecretStore::peek_session_id`. `None` for ApiKey providers, a
    /// non-Anthropic base, or a credential that has none -- in every
    /// such case `build_headers` stamps no session-id header.
    pub session_id: Option<String>,
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
            // Presence only: the session_id ties requests to one logical
            // session; treat as sensitive so its value never enters logs.
            .field("session_id", &self.session_id.is_some())
            .finish()
    }
}

impl AnthropicApiConfig {
    /// Default per-entry byte cap on the thinking cache used by the
    /// `context_management` emulation path. Falls back to this value
    /// when the per-provider `max_thinking_entry_bytes` knob is unset.
    /// Entries whose serialized JSON byte length exceeds the resolved
    /// cap are rejected at write time and a structured WARN is emitted.
    /// The strip-on-miss recovery in `request.rs` handles the next turn
    /// the same way it would for a TTL eviction. 1 MiB gives ~3x
    /// headroom over realistic worst-case Opus 4.6/4.7/4.8 reasoning
    /// turns at full thinking-token budgets.
    pub const MAX_THINKING_ENTRY_BYTES: usize =
        context_management::DEFAULT_MAX_THINKING_ENTRY_BYTES;

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
            max_thinking_entry_bytes: Self::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
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
        let ua = resolve_user_agent(cfg.user_agent.as_deref(), cfg.auth_kind);
        let client = crate::http_client::build(ua.as_deref());
        let cap = std::num::NonZeroUsize::new(context_management::THINKING_CACHE_CAP)
            .expect("THINKING_CACHE_CAP is non-zero");
        let thinking_cache = std::sync::Arc::new(std::sync::RwLock::new(lru::LruCache::new(cap)));
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
            context_management::THINKING_CACHE_TTL,
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
        //
        // Apply the operator allowlist to the client-supplied betas
        // before composing the header. Operator-supplied betas from
        // `header_extras` pass through unconditionally (the operator
        // typed them in config). Empty `allowed_betas` is pass-through
        // mode (no filtering); see `request::filter_anthropic_betas`.
        let filtered_req_betas = request::filter_anthropic_betas(
            &self.cfg.id,
            &req.anthropic_beta,
            &self.cfg.allowed_betas,
        );
        let mut beta_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut merged_betas: Vec<String> = Vec::new();
        for entry in filtered_req_betas.iter() {
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
        // Operator-configured model-level betas, composed by the router
        // from `[models.X] header_extras["anthropic-beta"]`. Like the
        // provider-level `config_betas` floor above, these bypass the
        // `allowed_betas` allowlist unconditionally -- that allowlist
        // gates only client-requested betas, never operator-pinned ones.
        // Empty for library consumers that bypass the router.
        for entry in req.routectl_internal.operator_betas.iter() {
            let t = entry.trim();
            if !t.is_empty() && beta_seen.insert(t.to_string()) {
                merged_betas.push(t.to_string());
            }
        }

        // Pinned Claude Code beta floor for the OauthBearer +
        // api.anthropic.com surface. These are operator-equivalent pins
        // (not client-requested), so they bypass the `allowed_betas`
        // allowlist by construction -- they never pass through
        // `filter_anthropic_betas`. Notably `oauth-2025-04-20` is
        // REQUIRED for OAuth to function on api.anthropic.com, so a
        // zero-config oauth-bearer provider works without the operator
        // hand-declaring it. Composed BEFORE the context_management strip
        // below so the emulation path can still remove
        // `context-management-2025-06-27` when active.
        if self.cfg.auth_kind == AuthKind::OauthBearer && is_anthropic_api_host(&self.cfg.base_url)
        {
            for t in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
                if beta_seen.insert(t.to_string()) {
                    merged_betas.push(t.to_string());
                }
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

        // Compiled Claude Code SDK identity defaults. Fire by default on
        // the OauthBearer path so a zero-config operator emits the
        // Stainless SDK fingerprint without hand-listing every header.
        // Inserted FIRST so the header_extras loop below OVERRIDES any
        // matching key (HeaderMap::insert replaces). ApiKey gets no
        // defaults (it is the raw-API surface, not the SDK client). Note
        // `anthropic-beta` is NOT among these -- it is composed above.
        if self.cfg.auth_kind == AuthKind::OauthBearer {
            for (k, v) in routectl_core::identity::anthropic::default_claude_code_identity_headers()
            {
                crate::http_client::insert_header(&mut header_map, &self.cfg.id, k, v);
            }

            // Claude Code session identity. These fire only on the
            // OauthBearer Claude-Code surface AND only when talking to
            // api.anthropic.com, so a non-Anthropic base (a third-party
            // /anthropic surface, a proxy) never receives the Claude-Code
            // session id. Stamped in the same "inserted first" phase as
            // the identity defaults so an operator `header_extras` entry
            // still overrides (the apply loop below replaces) and a
            // forwarded client header overrides after that.
            //   - x-client-request-id: one fresh uuid per request (the
            //     upstream pairs it with the turn).
            //   - x-claude-code-session-id: the stable per-credential id
            //     minted at login; ties requests to one logical session,
            //     stable across the credential's lifetime. Omitted when the
            //     credential has none.
            if is_anthropic_api_host(&self.cfg.base_url) {
                let request_id = uuid::Uuid::new_v4().to_string();
                crate::http_client::insert_header(
                    &mut header_map,
                    &self.cfg.id,
                    "x-client-request-id",
                    &request_id,
                );
                if let Some(sid) = &self.cfg.session_id {
                    crate::http_client::insert_header(
                        &mut header_map,
                        &self.cfg.id,
                        "x-claude-code-session-id",
                        sid,
                    );
                }
            }
        }

        // Prefer the router-composed map for non-beta headers; fall
        // back to `self.cfg.header_extras` for library consumers.
        let source = crate::http_client::effective_header_extras(
            &self.cfg.header_extras,
            req.routectl_internal.header_extras.as_ref(),
        );
        // `anthropic-beta` is list-valued and composed above from the
        // ingress + provider + model union, so it is skipped here.
        crate::http_client::apply_header_extras(
            &mut header_map,
            &source,
            &self.cfg.id,
            &["anthropic-beta"],
        );

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
                    crate::http_client::insert_header(
                        &mut header_map,
                        &self.cfg.id,
                        name.as_str(),
                        val.as_str(),
                    );
                }
            }
        }

        if !header_map.is_empty() {
            rb = rb.headers(header_map);
        }
        rb
    }
}

/// True when `base_url`'s host is exactly `api.anthropic.com` -- the only
/// surface that should receive the Claude-Code session identity headers.
///
/// A precise host match, NOT a substring test: `base_url.contains(
/// "api.anthropic.com")` would also match an operator-misconfigured
/// `https://api.anthropic.com.example.com` (a sibling-domain takeover) or
/// `https://proxy.example/api.anthropic.com` (host in the path). An exact
/// host match avoids sending the session-id headers to an unintended host
/// when `base_url` is misconfigured. `base_url` is trusted operator config,
/// so this is defense in depth on a ban-risk identity surface rather than a
/// fix for attacker input.
///
/// The host is the authority between the scheme and the first `/?#`, minus
/// any `user@` credentials and `:port`. Kept dependency-free (no `url`
/// crate) since the shape is fixed and validated upstream by
/// `validate_base_url_scheme`.
fn is_anthropic_api_host(base_url: &str) -> bool {
    let after_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Drop optional `user:pass@` credentials, then the optional `:port`.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority);
    host.eq_ignore_ascii_case("api.anthropic.com")
}

/// Resolve the client-level `User-Agent` for an anthropic-api provider.
/// An operator override always wins. With no override, the OauthBearer
/// surface falls back to the Claude Code SDK UA so a zero-config
/// oauth-bearer provider emits the expected client fingerprint; the
/// ApiKey surface keeps reqwest's default UA (`None`).
fn resolve_user_agent(user_agent: Option<&str>, auth_kind: AuthKind) -> Option<String> {
    match (user_agent, auth_kind) {
        (Some(ua), _) => Some(ua.to_string()),
        (None, AuthKind::OauthBearer) => {
            Some(routectl_core::identity::anthropic::default_claude_code_user_agent().to_string())
        }
        (None, AuthKind::ApiKey) => None,
    }
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
        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Per-request token resolution: for static refs this hits
        // the in-memory `StaticToken` cache; for `oauth://<provider>`
        // refs this dives into `OAuthStore` and resolves the current
        // value (including the v0.7+ refresh path).
        let token = self.cfg.auth.token().await?;

        // Serialize first so the billing-header checksum can be re-signed
        // over the exact bytes transmitted. routectl mutates the canonical
        // body upstream of this point (effort injection, tool-id sanitize,
        // signature strip), which invalidates any checksum the ingress
        // client computed. Re-sign only on the Claude-Code OauthBearer
        // api.anthropic.com surface; every other path is a no-op.
        let mut body_bytes = serde_json::to_vec(&body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        if self.cfg.auth_kind == AuthKind::OauthBearer && is_anthropic_api_host(&self.cfg.base_url)
        {
            crate::claude_signing::resign_cch_in_place(&mut body_bytes);
        }

        let request = self
            .build_headers(self.client.post(self.messages_url()), &req, &token)
            .header("content-type", "application/json")
            .body(body_bytes)
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
                    context_management::THINKING_CACHE_TTL,
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

        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.cfg.auth.token().await?;

        // See the complete() path: re-sign the billing-header checksum
        // over the exact transmitted bytes on the Claude-Code OauthBearer
        // api.anthropic.com surface; a no-op everywhere else.
        let mut body_bytes = serde_json::to_vec(&body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        if self.cfg.auth_kind == AuthKind::OauthBearer && is_anthropic_api_host(&self.cfg.base_url)
        {
            crate::claude_signing::resign_cch_in_place(&mut body_bytes);
        }

        let request = self
            .build_headers(self.client.post(self.messages_url()), &req, &token)
            .header("content-type", "application/json")
            .body(body_bytes)
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
        let max_thinking_entry_bytes = self.cfg.max_thinking_entry_bytes;
        let thinking_cache_for_stream = Arc::clone(&self.thinking_cache);

        let stream = async_stream::stream! {
            let mut state = SseState::new(&provider_id);

            futures::pin_mut!(event_stream);
            while let Some(result) = event_stream.next().await {
                match result {
                    Err(e) => {
                        // Surface the cache-write count we are abandoning
                        // so triage can correlate a torn stream with any
                        // pending context_management snapshots that never
                        // made it into the LRU.
                        tracing::debug!(
                            provider = %provider_id,
                            pending_cache_writes_count = state.pending_cache_writes.len(),
                            "anthropic-api stream: SSE event error; aborting before post-stream cache drain"
                        );
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
                                // Same triage hint as the event-stream Err
                                // arm above: log the abandoned cache-write
                                // count before yielding so a parse failure
                                // mid-stream is correlatable.
                                tracing::debug!(
                                    provider = %provider_id,
                                    pending_cache_writes_count = state.pending_cache_writes.len(),
                                    "anthropic-api stream: SSE parse error; aborting before post-stream cache drain"
                                );
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
                        max_thinking_entry_bytes,
                        context_management::THINKING_CACHE_TTL,
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

        // count_tokens is deliberately unsigned (matches upstream).
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
    // Capture the reset hint from response headers BEFORE `resp.text()`
    // moves the body, gated on rate-limit statuses. This is the single
    // chokepoint for complete/stream/count_tokens, so all three HTTP
    // paths pick up the hint here.
    let retry_after = if crate::retry_after::is_rate_limit_status(status) {
        crate::retry_after::parse_retry_after(resp.headers())
    } else {
        None
    };
    let body_text = resp.text().await.unwrap_or_default();
    // Emit the FULL upstream error body at debug level so triage
    // doesn't have to reproduce. The caller's WARN excerpt stays
    // unchanged for `routectl-warn.log` scannability.
    debug_upstream_error_body(PROVIDER_KIND, provider_id, status, &body_text);
    let parsed = serde_json::from_str::<Value>(&body_text).ok();
    let msg = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sanitize_upstream_body(&body_text));
    // Lift the upstream classifier (Anthropic shape
    // `{type:"error",error:{type,message}}`) so an SDK that branches on
    // `error.type` keeps the upstream signal. Anthropic errors carry no
    // separate `code`, so only `upstream_type` is populated.
    let upstream_type = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/type"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let err = Error::upstream_full(
        provider_id,
        status,
        msg.clone(),
        retry_after,
        upstream_type,
        None,
    );
    (msg, err)
}

/// Build the body for `POST /v1/messages/count_tokens` from the
/// already-normalized `/v1/messages` body. Only an explicit allowlist
/// of fields gets copied through:
/// `model`, `messages`, `system`, `tools`, `tool_choice`, `thinking`,
/// `mcp_servers`.
///
/// The count_tokens schema accepts `messages`, `model`, `cache_control`,
/// `output_config`, `system`, `thinking`, `tool_choice`, and `tools`
/// (`cache_control` rides inside the message/system/tool blocks that are
/// forwarded wholesale). `metadata` is NOT part of that schema, so it must
/// be dropped or the upstream 400s with `Extra inputs are not permitted`.
/// `output_config` IS accepted but is intentionally omitted here because it
/// does not affect the input token count.
///
/// This allowlist is more defensive than strip-by-blocklist: future
/// additions to `normalize_request` won't accidentally leak into
/// count_tokens.
fn build_count_tokens_body(normalized: &Value) -> Value {
    const ALLOWED: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        // Accepted only by the MCP-connector beta variant of count_tokens
        // (routectl unions that beta header through).
        "mcp_servers",
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

    /// Body fields routectl forwards to Anthropic's
    /// `/v1/messages/count_tokens` endpoint. This is the forwarding
    /// allowlist, a subset of the count_tokens schema (`messages`,
    /// `model`, `cache_control`, `output_config`, `system`, `thinking`,
    /// `tool_choice`, `tools`); `metadata` is excluded because it is NOT
    /// in that schema. Pinning the list as a const lets the test assert
    /// that no extra fields leak into the count_tokens body even when
    /// `normalize_request` is extended.
    const COUNT_TOKENS_ALLOWED_FIELDS: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "mcp_servers",
    ];

    /// Pin: `build_count_tokens_body` copies ONLY the allowlist
    /// fields, even when `normalize_request` produces extra keys.
    /// Without this contract, a non-schema field such as `metadata`
    /// silently flows into `/v1/messages/count_tokens` and the upstream
    /// 400s with `Extra inputs are not permitted`.
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
            // Fields below MUST NOT reach the upstream count_tokens body:
            "metadata": {"user_id": "u_42"},
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
        // `metadata` is not part of the count_tokens schema; it must be dropped.
        assert!(!obj.contains_key("metadata"));
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
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
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
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
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

    /// Non-empty `allowed_betas` drops client-requested flags that are
    /// not on the operator list. The header must contain only the
    /// allowed flag and must NOT contain the blocked one.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn allowed_betas_filters_header_drops_unlisted_flag() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: vec!["allowed-only".into()],
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        // ChatRequest is #[non_exhaustive]; mutate after default().
        let mut req = ChatRequest::default();
        req.anthropic_beta = vec!["allowed-only".into(), "blocked".into()];
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        assert!(
            value.split(',').any(|s| s.trim() == "allowed-only"),
            "allowed flag must reach the header; got {value}"
        );
        assert!(
            !value.split(',').any(|s| s.trim() == "blocked"),
            "blocked flag must be dropped from the header; got {value}"
        );
    }

    /// Operator `header_extras` betas bypass the allowlist unconditionally
    /// while non-allowlisted client betas are dropped. This pins the
    /// design contract: operator-supplied config wins regardless of the
    /// client-request content, but the allowlist still gates client betas.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn operator_header_extras_beta_bypasses_allowlist() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: vec![("anthropic-beta".into(), "ops-only".into())],
            user_agent: None,
            allowed_betas: vec!["req-allowed".into()],
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let mut req = ChatRequest::default();
        req.anthropic_beta = vec!["req-allowed".into(), "client-blocked".into()];
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        assert!(
            value.split(',').any(|s| s.trim() == "ops-only"),
            "operator header_extras beta must bypass allowlist and reach the header; got {value}"
        );
        assert!(
            value.split(',').any(|s| s.trim() == "req-allowed"),
            "allowlisted client beta must reach the header; got {value}"
        );
        assert!(
            !value.split(',').any(|s| s.trim() == "client-blocked"),
            "non-allowlisted client beta must be dropped; got {value}"
        );
    }

    /// Empty `allowed_betas` is pass-through mode: every requested
    /// beta reaches the header unchanged. This is the default for all
    /// deployments that do not set an explicit allowlist.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn allowed_betas_empty_passes_all_through() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        // ChatRequest is #[non_exhaustive]; mutate after default().
        let mut req = ChatRequest::default();
        req.anthropic_beta = vec!["beta-one".into(), "beta-two".into()];
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        assert!(
            value.split(',').any(|s| s.trim() == "beta-one"),
            "beta-one must pass through with empty allowlist; got {value}"
        );
        assert!(
            value.split(',').any(|s| s.trim() == "beta-two"),
            "beta-two must pass through with empty allowlist; got {value}"
        );
    }

    /// Model-level operator betas (composed by the router onto
    /// `routectl_internal.operator_betas`) bypass the allowlist
    /// unconditionally, while non-allowlisted client betas folded into
    /// `req.anthropic_beta` are still dropped. This pins the invariant:
    /// `allowed_betas` gates only client-requested betas, never the
    /// betas an operator pinned in `[models.X] header_extras`.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn model_level_operator_beta_bypasses_allowlist() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: vec!["req-allowed".into()],
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let mut req = ChatRequest::default();
        // The router folds the model-level beta into the full union on
        // `req.anthropic_beta` AND records it as an operator floor on
        // `operator_betas`. The allowlist filter drops it from the union,
        // but the floor re-adds it unconditionally.
        req.anthropic_beta = vec![
            "req-allowed".into(),
            "client-blocked".into(),
            "ctx-1m".into(),
        ];
        req.routectl_internal.operator_betas = vec!["ctx-1m".into()];
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        assert!(
            value.split(',').any(|s| s.trim() == "ctx-1m"),
            "model-level operator beta must bypass allowlist and reach the header; got {value}"
        );
        assert!(
            value.split(',').any(|s| s.trim() == "req-allowed"),
            "allowlisted client beta must reach the header; got {value}"
        );
        assert!(
            !value.split(',').any(|s| s.trim() == "client-blocked"),
            "non-allowlisted client beta must be dropped; got {value}"
        );
    }

    /// Build an oauth-bearer config with the given header_extras and a
    /// `user_agent` override (None to exercise the SDK default).
    fn oauth_cfg(
        header_extras: Vec<(String, String)>,
        user_agent: Option<String>,
    ) -> AnthropicApiConfig {
        AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("oat-token")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            header_extras,
            user_agent,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        }
    }

    /// On the OauthBearer path with empty `header_extras`, the compiled
    /// Stainless SDK defaults appear on the outgoing request. Zero-config
    /// posture: auth_kind + api_key_ref alone yields the full fingerprint.
    #[test]
    fn oauth_bearer_emits_stainless_defaults_with_empty_extras() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = ChatRequest::default();
        assert_eq!(
            outbound_header_value(&provider, &req, "x-app").as_deref(),
            Some("cli"),
            "x-app default must appear on oauth-bearer",
        );
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-lang").as_deref(),
            Some("js"),
            "x-stainless-lang default must appear on oauth-bearer",
        );
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-timeout").as_deref(),
            Some("600"),
            "x-stainless-timeout default must appear on oauth-bearer",
        );
        // Dynamic entries present and mapped (not raw Rust cfg strings).
        let arch = outbound_header_value(&provider, &req, "x-stainless-arch")
            .expect("x-stainless-arch present");
        assert_ne!(arch, "x86_64", "arch must be mapped to Node shape");
        let os = outbound_header_value(&provider, &req, "x-stainless-os")
            .expect("x-stainless-os present");
        assert_ne!(os, "linux", "os must be mapped to capitalized shape");
    }

    /// An operator `header_extras` entry for a default key OVERRIDES the
    /// compiled Stainless default (insert replaces; the loop runs after
    /// the defaults).
    #[test]
    fn oauth_bearer_header_extras_overrides_stainless_default() {
        let provider = AnthropicApiProvider::new(oauth_cfg(
            vec![("x-stainless-timeout".into(), "999".into())],
            None,
        ));
        let req = ChatRequest::default();
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-timeout").as_deref(),
            Some("999"),
            "operator header_extras must override the compiled default",
        );
    }

    /// On the ApiKey path, no Stainless SDK defaults are injected even
    /// with empty `header_extras`. The api-key surface is the raw API,
    /// not the SDK client, so it carries no SDK fingerprint.
    #[test]
    fn api_key_path_emits_no_stainless_defaults() {
        let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
        let req = ChatRequest::default();
        for absent in [
            "x-app",
            "x-stainless-lang",
            "x-stainless-runtime",
            "x-stainless-runtime-version",
            "x-stainless-package-version",
            "x-stainless-timeout",
            "x-stainless-retry-count",
            "x-stainless-arch",
            "x-stainless-os",
            "anthropic-dangerous-direct-browser-access",
        ] {
            assert!(
                outbound_header_value(&provider, &req, absent).is_none(),
                "{absent:?} must NOT be injected on the api-key path",
            );
        }
    }

    /// On OauthBearer with `user_agent = None`, the resolved client UA
    /// falls back to the Claude Code SDK default. An operator override
    /// always wins; the ApiKey surface keeps reqwest's default (`None`).
    /// We assert the resolver directly: reqwest applies a client-level
    /// default UA only at send time, not at `RequestBuilder::build()`,
    /// so the value is not observable on a non-executed request.
    #[test]
    fn oauth_bearer_user_agent_defaults_to_claude_cli() {
        assert_eq!(
            resolve_user_agent(None, AuthKind::OauthBearer).as_deref(),
            Some("claude-cli/2.1.167 (external, sdk-cli)"),
            "oauth-bearer with no override must default to the Claude Code SDK UA",
        );
        assert_eq!(
            resolve_user_agent(None, AuthKind::ApiKey),
            None,
            "api-key with no override must keep reqwest's default UA",
        );
        assert_eq!(
            resolve_user_agent(Some("op-ua/9.9"), AuthKind::OauthBearer).as_deref(),
            Some("op-ua/9.9"),
            "operator override must win over the SDK default",
        );
    }

    /// Build an oauth-bearer config with an explicit base_url and an
    /// optional session_id, plus optional header_extras. Used by the
    /// Claude-Code session-identity header tests.
    fn oauth_cfg_with_session(
        base_url: &str,
        session_id: Option<String>,
        header_extras: Vec<(String, String)>,
    ) -> AnthropicApiConfig {
        AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("oat-token")),
            base_url: base_url.into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            header_extras,
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id,
        }
    }

    /// Two requests through an OauthBearer api.anthropic.com provider
    /// carrying a session_id must stamp the SAME `x-claude-code-session-id`
    /// (stable per credential) and DIFFERENT, valid-uuid
    /// `x-client-request-id` values (fresh per request).
    #[test]
    fn oauth_anthropic_base_stamps_stable_session_and_fresh_request_id() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("session-stable-123".into()),
            Vec::new(),
        ));
        let req = ChatRequest::default();

        let sid_1 = outbound_header_value(&provider, &req, "x-claude-code-session-id");
        let sid_2 = outbound_header_value(&provider, &req, "x-claude-code-session-id");
        assert_eq!(sid_1.as_deref(), Some("session-stable-123"));
        assert_eq!(
            sid_1, sid_2,
            "session-id must be stable across requests on one credential"
        );

        let rid_1 = outbound_header_value(&provider, &req, "x-client-request-id")
            .expect("x-client-request-id must be present");
        let rid_2 = outbound_header_value(&provider, &req, "x-client-request-id")
            .expect("x-client-request-id must be present");
        assert_ne!(
            rid_1, rid_2,
            "x-client-request-id must be fresh per request"
        );
        assert!(
            uuid::Uuid::parse_str(&rid_1).is_ok(),
            "x-client-request-id must be a valid uuid; got {rid_1}"
        );
        assert!(
            uuid::Uuid::parse_str(&rid_2).is_ok(),
            "x-client-request-id must be a valid uuid; got {rid_2}"
        );
    }

    /// The ApiKey surface is the raw API, not the Claude-Code SDK client:
    /// neither session-identity header is stamped.
    #[test]
    fn api_key_path_stamps_no_session_identity_headers() {
        // cfg_with_allowlist builds an ApiKey config on the
        // api.anthropic.com base.
        let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
        let req = ChatRequest::default();
        assert!(
            outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
            "ApiKey path must not stamp x-client-request-id",
        );
        assert!(
            outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
            "ApiKey path must not stamp x-claude-code-session-id",
        );
    }

    /// OauthBearer but a non-anthropic base (a third-party /anthropic
    /// surface): the Claude-Code session identity must NOT leak there.
    #[test]
    fn oauth_non_anthropic_base_stamps_no_session_identity_headers() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://example.invalid",
            Some("session-stable-123".into()),
            Vec::new(),
        ));
        let req = ChatRequest::default();
        assert!(
            outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
            "non-anthropic base must not stamp x-client-request-id",
        );
        assert!(
            outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
            "non-anthropic base must not stamp x-claude-code-session-id",
        );
    }

    /// An operator `header_extras` entry for `x-claude-code-session-id`
    /// OVERRIDES the built-in value: the identity stamping is in the
    /// "inserted first" phase, the header_extras apply loop runs after
    /// and replaces.
    #[test]
    fn operator_header_extras_overrides_built_in_session_id() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("built-in-session".into()),
            vec![(
                "x-claude-code-session-id".into(),
                "from-operator-config".into(),
            )],
        ));
        let req = ChatRequest::default();
        let value = outbound_header_value(&provider, &req, "x-claude-code-session-id")
            .expect("session-id header must be present");
        assert_eq!(
            value, "from-operator-config",
            "operator header_extras must override the built-in session id; got {value}"
        );
    }

    #[test]
    fn is_anthropic_api_host_matches_only_the_exact_host() {
        // Exact host, with and without a path / port, matches.
        assert!(is_anthropic_api_host("https://api.anthropic.com"));
        assert!(is_anthropic_api_host(
            "https://api.anthropic.com/v1/messages"
        ));
        assert!(is_anthropic_api_host("https://api.anthropic.com:443/v1"));
        assert!(is_anthropic_api_host("https://API.Anthropic.Com")); // case-insensitive host
                                                                     // Sibling-domain takeover and host-in-path must NOT match.
        assert!(!is_anthropic_api_host(
            "https://api.anthropic.com.evil.example"
        ));
        assert!(!is_anthropic_api_host(
            "https://proxy.example/api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host(
            "https://evil.example#api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host(
            "https://evil.example?h=api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host("https://anthropic.com"));
        // A credentials prefix on the authority is stripped before the host
        // check, so it cannot be used to smuggle a different real host.
        assert!(is_anthropic_api_host("https://user:pass@api.anthropic.com"));
        assert!(!is_anthropic_api_host(
            "https://api.anthropic.com@evil.example"
        ));
    }

    /// A non-anthropic base that merely CONTAINS the host substring must
    /// not stamp the Claude-Code session identity (defends the precise
    /// host check end-to-end through build_headers).
    #[test]
    fn lookalike_anthropic_base_stamps_no_session_identity_headers() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com.evil.example",
            Some("session-stable-123".into()),
            Vec::new(),
        ));
        let req = ChatRequest::default();
        assert!(
            outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
            "a look-alike host must not stamp x-client-request-id",
        );
        assert!(
            outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
            "a look-alike host must not stamp x-claude-code-session-id",
        );
    }

    // -- Beta floor tests --------------------------------------------------

    /// On OauthBearer + api.anthropic.com, all 9 pinned floor betas
    /// appear in the outbound `anthropic-beta` header.
    #[test]
    fn beta_floor_all_nine_present_on_oauth_anthropic_host() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = ChatRequest::default();
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        let betas: Vec<&str> = value.split(',').map(str::trim).collect();
        for expected in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
            assert!(
                betas.contains(expected),
                "floor beta {expected} must be present on oauth+anthropic host; got: {value}"
            );
        }
    }

    /// When context_management emulation is active, the
    /// `context-management-2025-06-27` floor beta is stripped from the
    /// outbound header (the emulation path handles the semantics, so
    /// forwarding it upstream would cause a 400 on non-Anthropic hosts).
    #[test]
    fn beta_floor_context_management_stripped_when_emulation_active() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("oat-token")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: true,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = ChatRequest::default();
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        let betas: Vec<&str> = value.split(',').map(str::trim).collect();
        assert!(
            !betas.contains(&context_management::CONTEXT_MANAGEMENT_BETA),
            "context-management beta must be stripped when emulation is active; got: {value}"
        );
        // Other floor betas must still be present.
        assert!(
            betas.contains(&"oauth-2025-04-20"),
            "non-stripped floor betas must still be present; got: {value}"
        );
    }

    /// On OauthBearer with a non-Anthropic base, the floor betas must
    /// NOT appear -- the floor is scoped to api.anthropic.com only.
    #[test]
    fn beta_floor_absent_on_non_anthropic_host() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("oat-token")),
            base_url: "https://proxy.example.com/".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = ChatRequest::default();
        // No client betas, no operator betas -> header absent entirely.
        assert!(
            outbound_header_value(&provider, &req, "anthropic-beta").is_none(),
            "beta floor must NOT appear on a non-anthropic host"
        );
    }

    /// On ApiKey (even with api.anthropic.com base), the floor betas
    /// must NOT appear -- the floor is scoped to OauthBearer only.
    #[test]
    fn beta_floor_absent_on_api_key_auth() {
        let cfg = AnthropicApiConfig {
            id: "test".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = ChatRequest::default();
        // No client betas, no operator betas -> header absent entirely.
        assert!(
            outbound_header_value(&provider, &req, "anthropic-beta").is_none(),
            "beta floor must NOT appear on the api-key path"
        );
    }
}
