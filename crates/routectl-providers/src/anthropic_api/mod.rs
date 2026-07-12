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

use routectl_core::identity::anthropic::is_anthropic_api_host;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, StaticToken, TokenCount,
    TokenSource, debug_upstream_error_body, is_json_error_envelope, sanitize_for_log,
    sanitize_upstream_body, trace_outgoing_body, trace_upstream_success_body,
};

mod cloak;
pub(crate) mod context_management;
mod extras;
mod messages;
pub(crate) mod parts;
mod ratelimit_unified;
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

pub use cloak::{CloakConfig, CloakMode, ToolRename};

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
    /// Opt-in OAuth-cloak configuration. `CloakConfig::default()` (mode
    /// `auto`, no strict mode, no tool renames, no sensitive words) leaves
    /// the egress behavior identical to the always-on `mcp_` normalization
    /// -- zero config change for existing operators. Threaded into
    /// `cloak_body` to gate the mode and feed the tool-rename /
    /// sensitive-word passes.
    pub cloak: CloakConfig,
    /// True only for a provider entry configured
    /// `credential_source = "forwarded"` (set by the factory from
    /// `ProviderEntry::AnthropicApi::credential_source`). Gates the WIRE
    /// choke point: a captured `forwarded_bearer` is consumed ONLY when
    /// this is true (in addition to the existing bearer-present +
    /// anthropic-host legs). Default `false` -- an own-creds provider
    /// never consumes a floating forwarded bearer, even one captured for
    /// a sibling forwarded provider on the same router.
    pub use_forwarded_bearer: bool,
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
            // Cloak surface: counts/mode ONLY -- the configured tool-rename
            // pairs and sensitive words are operator content and must never
            // enter Debug output or logs.
            .field("cloak_mode", &self.cloak.mode)
            .field("cloak_strict_mode", &self.cloak.strict_mode)
            .field("cloak_tool_rename_count", &self.cloak.tool_rename.len())
            .field(
                "cloak_sensitive_words_count",
                &self.cloak.sensitive_words.len(),
            )
            .field("use_forwarded_bearer", &self.use_forwarded_bearer)
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
        }
    }
}

/// Beta-decision context computed by `build_headers` on the OauthBearer +
/// api.anthropic.com own-token lane. Carries just enough of the beta-gating
/// decision to diagnose a beta-caused 4xx (e.g. a floor-widened beta the
/// upstream rejects) without enabling full header tracing. Cheap to build
/// (six bools/copies) -- returned unconditionally from `build_headers` so
/// the 2xx hot path pays no extra allocation, only this small struct.
#[derive(Debug, Clone, Copy)]
struct BetaDecision {
    is_non_cc: bool,
    forwarded_leg: bool,
    cloak_mode: cloak::CloakMode,
    oauth_added: bool,
    has_context_1m_beta: bool,
    has_context_management_beta: bool,
}

/// The single three-way WIRE gate shared by both consumers of the forwarded
/// bearer (`resolve_effective_token` and the `build_headers` forwarded leg):
/// the forwarded credential is used EXACTLY when the provider entry was
/// explicitly configured `credential_source = "forwarded"`, a captured
/// bearer is present on this request, AND the egress host is exactly
/// `api.anthropic.com`. Any one leg false means "not the forwarded path" --
/// in particular an own-mode provider (`use_forwarded_bearer` false) never
/// consumes a floating bearer captured for a sibling forwarded provider on
/// the same router. Pure and host-check-free of any live network state so
/// its full matrix is unit-testable without a live host (host-pinned egress
/// itself cannot be driven through wiremock).
fn should_use_forwarded_bearer(
    use_forwarded_bearer: bool,
    has_bearer: bool,
    base_url: &str,
) -> bool {
    use_forwarded_bearer && has_bearer && is_anthropic_api_host(base_url)
}

pub struct AnthropicApiProvider {
    cfg: AnthropicApiConfig,
    client: Client,
    thinking_cache: std::sync::Arc<std::sync::RwLock<context_management::ThinkingCache>>,
    /// Stable Claude Code identity minted once per provider instance.
    /// `Some` only on the OauthBearer + api.anthropic.com surface (the
    /// cloak target); `None` for every other auth kind or host. The
    /// minted `session_id` (which prefers `cfg.session_id`) drives the
    /// `x-claude-code-session-id` header, and the device/account fields
    /// feed the minted metadata `user_id` on the non-CC cloak path.
    identity: Option<cloak::ClaudeCodeIdentity>,
    /// Last-seen `anthropic-ratelimit-unified-representative-claim` for
    /// this provider instance. Drives the once-per-flip overage log:
    /// steady state is silent, a flip into "overage" warns, a flip back
    /// out informs. Concurrent requests share one instance, so the state
    /// is mutex-guarded. `None` means no unified-family response has been
    /// observed yet.
    last_representative_claim: std::sync::Mutex<Option<String>>,
}

impl AnthropicApiProvider {
    pub fn new(cfg: AnthropicApiConfig) -> Self {
        let ua = resolve_user_agent(cfg.user_agent.as_deref(), cfg.auth_kind);
        let client = crate::http_client::build(ua.as_deref());
        let cap = std::num::NonZeroUsize::new(context_management::THINKING_CACHE_CAP)
            .expect("THINKING_CACHE_CAP is non-zero");
        let thinking_cache = std::sync::Arc::new(std::sync::RwLock::new(lru::LruCache::new(cap)));
        // Mint the cloak identity once, only on the OAuth anthropic-api
        // surface. The minted session_id prefers cfg.session_id (the
        // login-minted value) and falls back to a fresh uuid so a
        // credential without one still presents a stable session.
        let identity =
            if cfg.auth_kind == AuthKind::OauthBearer && is_anthropic_api_host(&cfg.base_url) {
                Some(cloak::ClaudeCodeIdentity::mint(cfg.session_id.as_deref()))
            } else {
                None
            };
        Self {
            cfg,
            client,
            thinking_cache,
            identity,
            last_representative_claim: std::sync::Mutex::new(None),
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

    /// Resolve the token to stamp as the outbound Anthropic credential,
    /// gated on the per-provider WIRE choke point.
    ///
    /// In forwarded (pure-proxy) mode the ingress captures the client's
    /// first-party claude.ai bearer onto
    /// `req.routectl_internal.forwarded_bearer`. That token is a
    /// full-scope credential and MUST reach ONLY a provider explicitly
    /// configured `credential_source = "forwarded"`, and even then ONLY
    /// `api.anthropic.com`. So the forwarded token becomes the effective
    /// credential EXACTLY when `should_use_forwarded_bearer` holds:
    /// `cfg.use_forwarded_bearer` is true, `forwarded_bearer` is `Some`
    /// (ingress ran in forwarded mode for this request), AND `base_url`'s
    /// host is exactly `api.anthropic.com` (`is_anthropic_api_host`). On
    /// that path `self.cfg.auth.token()` is NOT called: the synthetic
    /// pure-proxy provider carries no live routectl credential, so
    /// resolving it would error.
    ///
    /// On every other path -- an own-mode provider (even with a floating
    /// bearer captured for a sibling forwarded provider), no captured
    /// bearer, OR a non-anthropic base -- the forwarded token is IGNORED
    /// (never even read) and the provider resolves its own token exactly
    /// as before. The raw token is never logged.
    async fn resolve_effective_token(&self, req: &ChatRequest) -> Result<String> {
        let forwarded = req.routectl_internal.forwarded_bearer.as_ref();
        if should_use_forwarded_bearer(
            self.cfg.use_forwarded_bearer,
            forwarded.is_some(),
            &self.cfg.base_url,
        ) {
            return Ok(forwarded
                .expect("forwarded is_some checked above")
                .expose()
                .to_string());
        }
        self.cfg.auth.token().await
    }

    fn build_headers(
        &self,
        rb: reqwest::RequestBuilder,
        req: &ChatRequest,
        token: &str,
    ) -> (reqwest::RequestBuilder, BetaDecision) {
        let mut rb = rb.header("anthropic-version", &self.cfg.anthropic_version);
        rb = match self.cfg.auth_kind {
            AuthKind::ApiKey => rb.header("x-api-key", token),
            AuthKind::OauthBearer => rb.header("authorization", format!("Bearer {token}")),
        };

        // Forwarded (pure-proxy) leg: this request arrived carrying the
        // client's first-party bearer bound for api.anthropic.com, AND this
        // provider was explicitly configured `credential_source =
        // "forwarded"`. On this leg the egress is a TRANSPARENT forwarder --
        // Claude Code's REAL inbound identity (`x-stainless-*`,
        // `x-claude-code-*`, and its own `anthropic-beta` set) must reach
        // Anthropic and OVERRIDE routectl's minted cloak fingerprint. Gated
        // EXACTLY on the same three-way pin as `resolve_effective_token`
        // (`should_use_forwarded_bearer`) so the two never disagree.
        // `forwarded_bearer` is read only as a gate here -- never as a
        // header value. An own-mode provider (`use_forwarded_bearer` false)
        // never enters this branch even with a floating captured bearer, so
        // its minted fingerprint is byte-for-byte unchanged.
        let forwarded_leg = should_use_forwarded_bearer(
            self.cfg.use_forwarded_bearer,
            req.routectl_internal.forwarded_bearer.is_some(),
            &self.cfg.base_url,
        );

        // Computed once here (rather than re-derived at each 4xx log site)
        // so `BetaDecision` and the floor-gating decision below always
        // agree on the same classification for this request.
        let is_non_cc = self.is_non_cc(req);

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
            .map_or("", |(_, v)| v.as_str());
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
        for entry in &req.routectl_internal.operator_betas {
            let t = entry.trim();
            if !t.is_empty() && beta_seen.insert(t.to_string()) {
                merged_betas.push(t.to_string());
            }
        }

        // OAuth gate for the OauthBearer + api.anthropic.com surface.
        // `oauth-2025-04-20` is REQUIRED for OAuth to function on
        // api.anthropic.com, so a zero-config oauth-bearer provider works
        // without the operator hand-declaring it. Unioned UNCONDITIONALLY
        // (independent of `is_non_cc`) so both a genuine-CC request and a
        // non-CC request stay authenticated -- unlike the floor below,
        // this single flag is not a fingerprint-widening pin. Composed
        // BEFORE the context_management strip below so the emulation path
        // can still remove `context-management-2025-06-27` when active.
        //
        // SUPPRESSED on the forwarded leg: there the client supplies its
        // own real beta set (on `req.anthropic_beta`), which must reach
        // Anthropic verbatim rather than being widened by routectl's minted
        // floor -- that would be a fingerprint the client never sent.
        let mut oauth_added = false;
        if !forwarded_leg
            && self.cfg.auth_kind == AuthKind::OauthBearer
            && is_anthropic_api_host(&self.cfg.base_url)
        {
            let oauth_beta = routectl_core::identity::anthropic::OAUTH_ANTHROPIC_BETA;
            if beta_seen.insert(oauth_beta.to_string()) {
                merged_betas.push(oauth_beta.to_string());
                oauth_added = true;
            }

            // Pinned Claude Code beta floor. These are operator-equivalent
            // pins (not client-requested), so they bypass the
            // `allowed_betas` allowlist by construction -- they never pass
            // through `filter_anthropic_betas`. GATED on `is_non_cc`: a
            // genuine Claude Code client already sent its own real beta
            // set above, and force-widening it with capability betas CC
            // never asked for (e.g. `context-1m` on a haiku request) makes
            // Anthropic 400 the request. Only a non-CC client -- one
            // routectl is cloaking as Claude Code -- gets the full floor.
            if is_non_cc {
                for t in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
                    if beta_seen.insert((*t).to_string()) {
                        merged_betas.push((*t).to_string());
                    }
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

        // Snapshot the FINAL composed beta set (post context_management
        // strip) so the decision context reflects what actually egresses,
        // not an intermediate union.
        let has_context_1m_beta = merged_betas
            .iter()
            .any(|b| b == routectl_core::identity::anthropic::CONTEXT_1M_BETA);
        let has_context_management_beta = merged_betas
            .iter()
            .any(|b| b == context_management::CONTEXT_MANAGEMENT_BETA);

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
            //   - x-claude-code-session-id: the provider's effective
            //     session id (the minted identity's session_id, which
            //     prefers cfg.session_id and falls back to a stable minted
            //     uuid). Stamped only on this OAuth + anthropic-host path,
            //     where `self.identity` is always `Some`. A forwarded
            //     client header still overrides via the apply loop below.
            if is_anthropic_api_host(&self.cfg.base_url) {
                let request_id = uuid::Uuid::new_v4().to_string();
                crate::http_client::insert_header(
                    &mut header_map,
                    &self.cfg.id,
                    "x-client-request-id",
                    &request_id,
                );
                if let Some(sid) = self.identity.as_ref().map(|i| &i.session_id) {
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

        // Forwarded (pure-proxy) leg: present the CLIENT's real identity so
        // Anthropic sees the genuine Claude Code fingerprint. Inserted LAST
        // so it OVERRIDES the minted defaults, the minted session id, and
        // any `header_extras` stamped above (HeaderMap::insert replaces).
        //
        //   - `x-stainless-*`: the client's captured Stainless fingerprint
        //     (dedicated `stainless_headers` carrier) replaces the minted
        //     `default_claude_code_identity_headers` values.
        //   - `x-claude-code-*`: forwarded TRANSPARENTLY (the whole captured
        //     set, allowlist-free) -- the client IS Claude Code on this leg,
        //     so its real session id replaces the minted one.
        //
        // Own mode never reaches here (`forwarded_leg` is false), so its
        // minted fingerprint + `forward_client_headers` allowlist behavior
        // is byte-for-byte unchanged.
        if forwarded_leg {
            for (name, val) in &req.routectl_internal.stainless_headers {
                crate::http_client::insert_header(&mut header_map, &self.cfg.id, name, val);
            }
            for (name, val) in &req.routectl_internal.claude_code_headers {
                crate::http_client::insert_header(&mut header_map, &self.cfg.id, name, val);
            }
        }

        if !header_map.is_empty() {
            rb = rb.headers(header_map);
        }
        let decision = BetaDecision {
            is_non_cc,
            forwarded_leg,
            cloak_mode: self.cfg.cloak.mode,
            oauth_added,
            has_context_1m_beta,
            has_context_management_beta,
        };
        (rb, decision)
    }

    /// Single source of truth for the beta-decision 4xx log lane gate,
    /// shared by `complete()`, `stream()`, and `count_tokens()` so the
    /// three call sites can never drift into logging on different lanes.
    /// True only on an own-token (never forwarded) OauthBearer request to
    /// api.anthropic.com that failed with a 4xx (never 5xx, since a
    /// beta-gating decision cannot cause one).
    fn should_log_beta_4xx(&self, status: u16, forwarded_leg: bool) -> bool {
        (400..500).contains(&status)
            && self.cfg.auth_kind == AuthKind::OauthBearer
            && is_anthropic_api_host(&self.cfg.base_url)
            && !forwarded_leg
    }

    /// Anthropic-only structured WARN emitted on a 4xx from the own-token
    /// OauthBearer + api.anthropic.com lane, adjacent to (never replacing)
    /// the shared `upstream_log::warn_upstream_failure` call. Carries just
    /// the beta-decision context plus the already-sanitized body excerpt --
    /// no tokens, credentials, or request/response content -- so a
    /// beta-caused 400 recurrence is diagnosable without enabling header
    /// tracing. Callers gate this to the own-token OAuth-anthropic lane
    /// before calling; the message literal is stable so subscribers can
    /// filter on it independent of `context`.
    fn log_beta_decision_on_4xx(&self, status: u16, dec: &BetaDecision, excerpt: &str) {
        tracing::warn!(
            provider = %self.cfg.id,
            status,
            is_non_cc = dec.is_non_cc,
            forwarded_leg = dec.forwarded_leg,
            cloak_mode = ?dec.cloak_mode,
            oauth_added = dec.oauth_added,
            has_context_1m_beta = dec.has_context_1m_beta,
            has_context_management_beta = dec.has_context_management_beta,
            body_excerpt = %excerpt,
            "anthropic-api oauth 4xx beta decision context",
        );
    }

    /// Classify a request as non-CC (true) or genuine-CC (false) under the
    /// configured `CloakMode`: `Always` forces non-CC, `Never` forces
    /// genuine-CC, and `Auto` applies the heuristic (non-CC iff no captured
    /// `x-claude-code-session-id` header -- a genuine Claude Code client
    /// always sends one). Single source of truth for `cloak_body` and,
    /// later, `build_headers` and 4xx logging.
    fn is_non_cc(&self, req: &ChatRequest) -> bool {
        match self.cfg.cloak.mode {
            cloak::CloakMode::Always => true,
            cloak::CloakMode::Never => false,
            cloak::CloakMode::Auto => !req
                .routectl_internal
                .claude_code_headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("x-claude-code-session-id")),
        }
    }

    /// Apply the Claude Code identity cloak to the outgoing body on the
    /// OAuth anthropic-api surface. Gated on `OauthBearer +
    /// is_anthropic_api_host` -- the same surface where `self.identity` is
    /// `Some`; a no-op on every other path. `is_non_cc` is true when the
    /// inbound request did NOT carry an `x-claude-code-session-id` capture
    /// (a genuine Claude Code client always does), so a non-CC client
    /// gets the identity block + minted metadata while a genuine CC client
    /// only has its billing block stripped.
    ///
    /// Returns `Some(CloakResult)` on the cloak path (carrying the
    /// per-request tool-name reverse map) and `None` on every non-cloak
    /// path (non-OAuth / non-anthropic-host / identity absent).
    ///
    /// Cache-safety invariant: the tool-name normalization here runs
    /// AFTER `normalize_request` and BEFORE serialization and
    /// `resign_cch_in_place`, and auto-cache breakpoints are planned
    /// upstream in the router. The rename touches only tool-NAME strings
    /// -- never `cache_control` keys -- so cached bytes stay byte-stable.
    fn cloak_body(&self, body: &mut Value, req: &ChatRequest) -> Option<cloak::CloakResult> {
        if self.cfg.auth_kind != AuthKind::OauthBearer || !is_anthropic_api_host(&self.cfg.base_url)
        {
            return None;
        }
        // `Never` must short-circuit BEFORE `is_non_cc` is consulted: it
        // skips ALL cloak transforms, so the classification is irrelevant
        // here (it exists only for `is_non_cc`'s other callers, which do
        // not early-return on `Never`).
        if self.cfg.cloak.mode == cloak::CloakMode::Never {
            return None;
        }
        let identity = self.identity.as_ref()?;
        // Trust boundary: the session-id header is client-supplied, so this
        // non-CC signal is advisory, not authoritative. The fail-safe is that
        // a misclassification cannot cause a silent billing leak -- a wrong
        // call yields an upstream rejection, not a paid overage applied
        // quietly.
        let is_non_cc = self.is_non_cc(req);
        let result = cloak::cloak_oauth_egress(body, req, identity, is_non_cc, &self.cfg.cloak);
        // Decision log: provider + non-CC gate + how many tool names were
        // normalized. NEVER logs tool names or message content.
        tracing::info!(
            provider = %self.cfg.id,
            is_non_cc = is_non_cc,
            rename_count = result.tool_reverse.len(),
            "anthropic-api cloak applied to outgoing body",
        );
        Some(result)
    }

    /// Parse the `anthropic-ratelimit-unified-*` quota family from an
    /// upstream response's headers, run the once-per-flip overage
    /// detection, and return the parsed quota wrapped in `UpstreamMeta`
    /// for attachment to the canonical response. `None` when the family
    /// is absent (api-key path, or any non-subscription response).
    /// Shared by the complete() and stream() paths so the flip log and
    /// the carrier attach identically on both.
    fn observe_unified_quota(
        &self,
        headers: &reqwest::header::HeaderMap,
    ) -> Option<routectl_core::UpstreamMeta> {
        let quota = ratelimit_unified::parse_unified_quota(headers)?;
        self.log_overage_flip(&quota);
        Some(routectl_core::UpstreamMeta::from_anthropic_unified(quota))
    }

    /// Update the per-instance last-seen `representative-claim` and emit
    /// one structured log on a billing-attribution flip. Entry into
    /// overage warns; recovery out of overage informs; steady state is
    /// silent (no per-request flood). Only non-secret quota strings are
    /// logged -- never tokens or credentials.
    fn log_overage_flip(&self, quota: &routectl_core::AnthropicUnifiedQuota) {
        let current_claim = quota.representative_claim.as_deref();
        let transition = {
            let mut guard = match self.last_representative_claim.lock() {
                Ok(g) => g,
                // A poisoned mutex (a prior panic) must not break quota
                // logging; recover the inner value and carry on.
                Err(poisoned) => poisoned.into_inner(),
            };
            let t = ratelimit_unified::classify_overage_transition(guard.as_deref(), current_claim);
            *guard = current_claim.map(str::to_string);
            t
        };
        let claim = current_claim.unwrap_or("");
        let overage_status = quota.overage_status.as_deref().unwrap_or("");
        let utilization = quota.utilization.as_deref().unwrap_or("");
        let overage_utilization = quota.overage_utilization.as_deref().unwrap_or("");
        let reset = quota.reset.as_deref().unwrap_or("");
        match transition {
            Some(ratelimit_unified::OverageTransition::EnteredOverage) => {
                tracing::warn!(
                    provider = %self.cfg.id,
                    representative_claim = %claim,
                    overage_status = %overage_status,
                    utilization = %utilization,
                    overage_utilization = %overage_utilization,
                    reset = %reset,
                    "anthropic subscription billing flipped to overage",
                );
            }
            Some(ratelimit_unified::OverageTransition::RecoveredFromOverage) => {
                tracing::info!(
                    provider = %self.cfg.id,
                    representative_claim = %claim,
                    overage_status = %overage_status,
                    utilization = %utilization,
                    overage_utilization = %overage_utilization,
                    reset = %reset,
                    "anthropic subscription billing recovered from overage",
                );
            }
            None => {}
        }
    }
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

        // Cloak the outgoing body on the OAuth anthropic-api surface:
        // always strip the billing block; for a non-CC client also reduce
        // `system` to the identity line only (relocating the client system
        // into the first user message) and mint the metadata user_id. Also
        // normalize every non-`mcp__` tool name to the `mcp__` prefix. Runs
        // after normalize_request and before serialize/resign. The returned
        // reverse map restores the client's original tool names on the
        // response below.
        let cloak_result = self.cloak_body(&mut body, &req);

        // Emit the outgoing body at trace level so a grep by
        // request_id correlates ingress -> egress -> upstream
        // response in one pass during triage. Gated by the
        // `tracing::Level::TRACE` filter -- production with default
        // info level pays nothing.
        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        // Host-pinned per-request token resolution. On the
        // api.anthropic.com surface a forwarded first-party bearer
        // (forwarded / pure-proxy mode) is used verbatim; otherwise the
        // provider resolves its own token -- for static refs the in-memory
        // `StaticToken` cache, for `oauth://<provider>` refs the `OAuthStore`
        // current value (including the v0.7+ refresh path). See
        // `resolve_effective_token` for the host pin.
        let token = self.resolve_effective_token(&req).await?;

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

        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.messages_url()), &req, &token);
        let request = rb
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
            // never has to guess WHY a request failed. Sanitize before
            // tracing: the upstream may return untrusted control bytes
            // (CRLF, control chars, very long lines) that would otherwise
            // corrupt log output on text-format subscribers.
            let safe_excerpt = sanitize_for_log(&msg);
            crate::upstream_log::warn_upstream_failure(
                &self.cfg.id,
                status,
                Some(&self.cfg.auth_kind),
                &safe_excerpt,
                "anthropic",
            );
            // Beta-decision context: own-token OauthBearer +
            // api.anthropic.com lane only -- the BetaDecision only carries
            // meaning there. Fires on ANY 4xx on that lane (no error-text
            // matching), so a beta-caused 400 recurrence is diagnosable
            // without enabling header tracing. Gate is `should_log_beta_4xx`
            // (shared with stream() and count_tokens()).
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE the body
        // consume (resp.json() takes ownership). Opt-in via
        // ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());
        // Parse the anthropic-ratelimit-unified-* quota family from the
        // same headers (BEFORE the body consume) and run the overage-flip
        // log. Returns None on the api-key path (family absent).
        let upstream_meta = self.observe_unified_quota(resp.headers());
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
        // Restore the client's original tool names on the response. The
        // forward pass normalized non-`mcp__` names to the `mcp__` prefix
        // on the wire; reverse only the names this request actually
        // renamed so a client that legitimately used `mcp__` names is
        // unaffected.
        if let Some(result) = cloak_result.as_ref() {
            response::reverse_tool_names(&mut chat_resp, &result.tool_reverse);
        }
        chat_resp.routectl_provider = Some(self.cfg.id.clone());
        chat_resp.upstream_meta = upstream_meta;
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

        // See complete(): cloak the OAuth anthropic-api body before
        // serialize/resign (billing strip always; identity + metadata for
        // a non-CC client; mcp_ tool-name normalization). The reverse map
        // is threaded into SseState so streamed tool_use names are
        // restored to the client's originals.
        let cloak_result = self.cloak_body(&mut body, &req);

        // NOTE: this trace reflects the pre-resign body; the cch token
        // in the transmitted bytes differs after resign_cch_in_place.
        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.resolve_effective_token(&req).await?;

        // See the complete() path: re-sign the billing-header checksum
        // over the exact transmitted bytes on the Claude-Code OauthBearer
        // api.anthropic.com surface; a no-op everywhere else.
        let mut body_bytes = serde_json::to_vec(&body)
            .map_err(|e| Error::upstream(&self.cfg.id, 0, e.to_string()))?;
        if self.cfg.auth_kind == AuthKind::OauthBearer && is_anthropic_api_host(&self.cfg.base_url)
        {
            crate::claude_signing::resign_cch_in_place(&mut body_bytes);
        }

        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.messages_url()), &req, &token);
        let request = rb
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
            crate::upstream_log::warn_upstream_failure(
                &self.cfg.id,
                status,
                Some(&self.cfg.auth_kind),
                &safe_excerpt,
                "anthropic",
            );
            // See complete(): own-token OauthBearer + api.anthropic.com
            // lane, 4xx only, via the shared `should_log_beta_4xx` gate.
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
            }
            return Err(err);
        }

        // Dir 3: upstream response headers, read BEFORE `resp` is moved
        // into the SSE byte stream. The stream path had no dir-3 capture
        // before; this closes the gap so it matches the complete() path.
        // Opt-in via ROUTECTL_TRACE_HEADERS.
        crate::header_trace::upstream(PROVIDER_KIND, &self.cfg.id, resp.headers());

        // Parse the anthropic-ratelimit-unified-* quota family from the
        // response head (BEFORE `resp` is moved into the byte stream) and
        // run the overage-flip log once here. The parsed carrier is
        // attached to the FIRST canonical chunk yielded by the stream;
        // consumers must not assume it on later chunks. None on the
        // api-key path (family absent).
        let mut pending_upstream_meta = self.observe_unified_quota(resp.headers());

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
        // Per-request tool-name reverse map (renamed upstream name ->
        // original client name) from the cloak forward pass. Empty / None
        // when the cloak did not run or renamed nothing.
        let tool_reverse = cloak_result.map(|r| r.tool_reverse).unwrap_or_default();

        let stream = async_stream::stream! {
            let mut state = SseState::new(&provider_id);
            state.tool_reverse = tool_reverse;

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
                            Ok(Some(mut chunk)) => {
                                // Attach the unified-quota carrier to the
                                // FIRST canonical chunk only; `take()`
                                // leaves None for every subsequent chunk.
                                if pending_upstream_meta.is_some() {
                                    chunk.upstream_meta = pending_upstream_meta.take();
                                }
                                yield Ok(chunk);
                            }
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
        let mut normalized = self.normalize_request(&req)?;
        // Cloak before build_count_tokens_body reads `normalized`. The
        // metadata user_id is dropped by the count_tokens allowlist (it is
        // not in that schema), but the system-identity stamp, the billing
        // strip, and the mcp_ tool-name normalization still apply to the
        // forwarded body. count_tokens has no response tool_use surface to
        // reverse, so the returned reverse map is discarded.
        self.cloak_body(&mut normalized, &req);
        let body = build_count_tokens_body(&normalized);

        trace_outgoing_body(PROVIDER_KIND, &self.cfg.id, &body);
        routectl_core::trace_structural_summary("outgoing", PROVIDER_KIND, &self.cfg.id, &body);

        let token = self.resolve_effective_token(&req).await?;

        // count_tokens is deliberately unsigned (matches upstream).
        let (rb, beta_decision) =
            self.build_headers(self.client.post(self.count_tokens_url()), &req, &token);
        let request = rb
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
            // untrusted control bytes (CRLF, control chars, very
            // long lines) and `body_excerpt = %msg` would otherwise
            // emit them verbatim into operator logs. Same posture as
            // the `complete()` and `stream()` paths above.
            let safe_excerpt = sanitize_for_log(&msg);
            if status == 501 {
                // A 501 on count_tokens is a CAPABILITY signal, not a
                // health failure: the upstream (e.g. an anthropic-api
                // base_url that back-hops to a Bedrock egress) does not
                // implement count_tokens. The router already handles this
                // by walking to the next capable seat, so logging it at
                // WARN would flood operator logs on every client poll.
                // DEBUG mirrors the router-layer treatment.
                tracing::debug!(
                    provider = %self.cfg.id,
                    status,
                    context = "anthropic count_tokens",
                    body_excerpt = %safe_excerpt,
                    "count_tokens unsupported by upstream (501); router walks to next capable seat",
                );
            } else {
                crate::upstream_log::warn_upstream_failure(
                    &self.cfg.id,
                    status,
                    Some(&self.cfg.auth_kind),
                    &safe_excerpt,
                    "anthropic count_tokens",
                );
            }
            // See complete(): own-token OauthBearer + api.anthropic.com
            // lane, 4xx only (naturally excludes the 501 capability signal
            // above, which is a 5xx), via the shared `should_log_beta_4xx`
            // gate.
            if self.should_log_beta_4xx(status, beta_decision.forwarded_leg) {
                self.log_beta_decision_on_4xx(status, &beta_decision, &safe_excerpt);
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
        .map_or_else(
            || sanitize_upstream_body(&body_text),
            std::string::ToString::to_string,
        );
    // Lift the upstream classifier (Anthropic shape
    // `{type:"error",error:{type,message}}`) so an SDK that branches on
    // `error.type` keeps the upstream signal. Anthropic errors carry no
    // separate `code`, so only `upstream_type` is populated.
    let upstream_type = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/type"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // When the upstream returned a structured `{error:...}` JSON envelope,
    // carry the RAW body so the ingress sanitizer can re-extract the
    // upstream's own top-level `error.message` and surface it to the
    // client. A client (e.g. Claude Code) can then recognize and
    // self-heal an actionable upstream 400 instead of hitting a
    // status-only wall. When the body was NOT a `{error:...}` envelope
    // (HTML page, plain-text gateway error), carry the sanitized excerpt
    // so the sanitizer falls back to a status-only message -- never a raw
    // body dump.
    let err_body = if is_json_error_envelope(&body_text) {
        body_text
    } else {
        msg.clone()
    };
    let err = Error::upstream_full(
        provider_id,
        status,
        err_body,
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
        if let Some(v) = src.get(k)
            && !v.is_null()
        {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

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
        let (rb, _decision) = provider.build_headers(rb, req, "test-token");
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
        let (rb, _decision) = provider.build_headers(rb, req, "test-token");
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            Some("claude-cli/2.1.169 (external, cli)"),
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
    /// optional session_id, plus optional header_extras, and an explicit
    /// forwarded-gate setting. Used by both the Claude-Code
    /// session-identity header tests (own mode) and the forwarded-leg
    /// tests (`use_forwarded_bearer: true`).
    fn oauth_cfg_with_session(
        base_url: &str,
        session_id: Option<String>,
        header_extras: Vec<(String, String)>,
        use_forwarded_bearer: bool,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer,
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
            false,
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

    /// With `cfg.session_id = None` on the OauthBearer + api.anthropic.com
    /// surface, the minted identity supplies a stable session id, so a
    /// `x-claude-code-session-id` header IS now stamped (mint-when-absent)
    /// and is the SAME across two requests (one identity per provider).
    #[test]
    fn oauth_anthropic_base_mints_stable_session_when_cfg_absent() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            None,
            Vec::new(),
            false,
        ));
        let req = ChatRequest::default();
        let sid_1 = outbound_header_value(&provider, &req, "x-claude-code-session-id")
            .expect("a session id must be minted when cfg.session_id is None");
        let sid_2 = outbound_header_value(&provider, &req, "x-claude-code-session-id")
            .expect("a session id must be minted when cfg.session_id is None");
        assert_eq!(
            sid_1, sid_2,
            "minted session id must be stable across requests"
        );
        assert!(
            uuid::Uuid::parse_str(&sid_1).is_ok(),
            "minted session id must be a valid uuid; got {sid_1}"
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
            false,
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
            false,
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
        // Exhaustive host-matching cases live with the shared predicate in
        // `routectl_core::identity::anthropic`. This thin delegation test
        // pins that the WIRE gate still routes through that single source
        // of truth: an exact host matches, a sibling-domain lookalike does
        // not.
        assert!(is_anthropic_api_host("https://api.anthropic.com"));
        assert!(!is_anthropic_api_host(
            "https://api.anthropic.com.evil.example"
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
            false,
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

    /// On OauthBearer + api.anthropic.com, all pinned floor betas
    /// appear in the outbound `anthropic-beta` header.
    #[test]
    fn beta_floor_all_pinned_present_on_oauth_anthropic_host() {
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
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
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
        };
        let provider = AnthropicApiProvider::new(cfg);
        let req = ChatRequest::default();
        // No client betas, no operator betas -> header absent entirely.
        assert!(
            outbound_header_value(&provider, &req, "anthropic-beta").is_none(),
            "beta floor must NOT appear on the api-key path"
        );
    }

    /// Genuine-CC (own-mode, `is_non_cc() == false`) requests must NOT get
    /// the fingerprint-widening beta floor: real Claude Code never asked
    /// for capability betas like `context-1m` on e.g. a haiku/WebFetch
    /// call, and force-widening its own beta set makes Anthropic 400 it.
    #[test]
    fn genuine_cc_request_omits_floor_only_betas() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present (oauth gate flag)");
        let betas: Vec<&str> = value.split(',').map(str::trim).collect();
        for floor_only in ["context-1m-2025-08-07", "interleaved-thinking-2025-05-14"] {
            assert!(
                !betas.contains(&floor_only),
                "genuine-CC request must not carry floor-only beta {floor_only}; got: {value}"
            );
        }
    }

    /// Non-CC (routectl is cloaking the request as Claude Code) requests
    /// still get the FULL pinned floor, unchanged from pre-gate behavior.
    #[test]
    fn non_cc_request_gets_full_floor() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = req_with_claude_code_headers(Vec::new());
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        let betas: Vec<&str> = value.split(',').map(str::trim).collect();
        for expected in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
            assert!(
                betas.contains(expected),
                "non-CC request must carry full floor beta {expected}; got: {value}"
            );
        }
    }

    /// `oauth-2025-04-20` is required for OAuth to function on
    /// api.anthropic.com, so it is unioned unconditionally -- present on
    /// BOTH the genuine-CC and the non-CC path, independent of the floor
    /// gate.
    #[test]
    fn oauth_beta_present_for_both_genuine_cc_and_non_cc() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));

        let genuine_cc = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid")]);
        let value = outbound_header_value(&provider, &genuine_cc, "anthropic-beta")
            .expect("anthropic-beta header must be present for genuine-CC");
        assert!(
            value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
            "oauth gate flag must be present for genuine-CC; got: {value}"
        );

        let non_cc = req_with_claude_code_headers(Vec::new());
        let value = outbound_header_value(&provider, &non_cc, "anthropic-beta")
            .expect("anthropic-beta header must be present for non-CC");
        assert!(
            value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
            "oauth gate flag must be present for non-CC; got: {value}"
        );
    }

    /// The gate never strips a genuine-CC client's OWN requested betas --
    /// only the routectl-minted floor is suppressed. A real Claude Code
    /// request that itself asked for `context-1m` still gets it.
    #[test]
    fn genuine_cc_own_requested_beta_survives_the_gate() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let mut req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        req.anthropic_beta = vec!["context-1m-2025-08-07".into()];

        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present");
        assert!(
            value
                .split(',')
                .any(|b| b.trim() == "context-1m-2025-08-07"),
            "the client's own requested beta must never be stripped by the gate; got: {value}"
        );
    }

    // -- forwarded (pure-proxy) leg: client identity overrides mint --------
    //
    // On the forwarded leg (`forwarded_bearer` Some AND the base is exactly
    // api.anthropic.com) the egress is a TRANSPARENT forwarder: Claude
    // Code's REAL inbound identity headers must reach Anthropic and
    // OVERRIDE routectl's minted cloak fingerprint. Own mode
    // (`forwarded_bearer` None) is byte-for-byte unchanged -- proven by the
    // minted-fingerprint tests above plus the explicit own-mode guard here.

    /// The distinctive forwarded-bearer secret used in the leg tests. It is
    /// only ever read as a GATE (is_some) by build_headers, never emitted,
    /// so the no-leak test can assert it appears in no outbound header.
    const FORWARDED_TOKEN_CANARY: &str = "sk-ant-oat01-FWD-DO-NOT-LEAK-xyz";

    /// Build a forwarded-leg request: a captured first-party bearer plus the
    /// client's inbound identity (`x-stainless-*` on `stainless_headers`,
    /// `x-claude-code-*` on `claude_code_headers`, betas on `anthropic_beta`).
    fn forwarded_req(
        stainless: &[(&str, &str)],
        claude_code: &[(&str, &str)],
        betas: &[&str],
    ) -> ChatRequest {
        let mut req = ChatRequest::default();
        req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
            FORWARDED_TOKEN_CANARY.into(),
        ));
        req.routectl_internal.stainless_headers = stainless
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        req.routectl_internal.claude_code_headers = claude_code
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        req.anthropic_beta = betas.iter().map(|s| (*s).to_string()).collect();
        req
    }

    /// Every outbound `(name, value)` pair on the assembled request. Lets a
    /// test scan all header values (e.g. for a leaked token).
    fn outbound_header_pairs(
        provider: &AnthropicApiProvider,
        req: &ChatRequest,
    ) -> Vec<(String, String)> {
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        let (rb, _decision) = provider.build_headers(rb, req, "test-token");
        let request = rb.build().expect("build outbound request");
        request
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    /// The minted `x-stainless-package-version` default, looked up from the
    /// shared identity source so the test does not hardcode the version.
    fn minted_stainless_package_version() -> String {
        routectl_core::identity::anthropic::default_claude_code_identity_headers()
            .into_iter()
            .find_map(|(n, v)| (n == "x-stainless-package-version").then(|| v.to_string()))
            .expect("minted default carries x-stainless-package-version")
    }

    /// Forwarded leg: the client's `x-stainless-*` headers OVERRIDE the
    /// minted Stainless fingerprint on the outbound request, so Anthropic
    /// sees the genuine client SDK identity rather than routectl's mint.
    #[test]
    fn forwarded_leg_client_stainless_overrides_minted_fingerprint() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        // Sanity: without a client value the minted default would win.
        let minted = minted_stainless_package_version();
        let req = forwarded_req(
            &[
                ("x-stainless-package-version", "1.2.3-client"),
                ("x-stainless-os", "ClientOS"),
                ("x-stainless-lang", "client-lang"),
            ],
            &[],
            &[],
        );

        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
            Some("1.2.3-client"),
            "client x-stainless-package-version must override the minted default",
        );
        assert_ne!(
            outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
            Some(minted.as_str()),
            "the minted Stainless version must NOT win on the forwarded leg",
        );
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-os").as_deref(),
            Some("ClientOS"),
            "client x-stainless-os must override the minted default",
        );
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-lang").as_deref(),
            Some("client-lang"),
            "client x-stainless-lang must override the minted default",
        );
    }

    /// Forwarded leg: the client's inbound `x-claude-code-session-id`
    /// OVERRIDES routectl's minted per-credential session id, so the
    /// forwarded request carries the client's real conversation identity.
    #[test]
    fn forwarded_leg_client_session_id_overrides_minted() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        let req = forwarded_req(&[], &[("x-claude-code-session-id", "client-sid-abc")], &[]);

        assert_eq!(
            outbound_header_value(&provider, &req, "x-claude-code-session-id").as_deref(),
            Some("client-sid-abc"),
            "client session id must override the minted session id on the forwarded leg",
        );
    }

    /// Forwarded leg: the client's captured `x-claude-code-*` headers are
    /// forwarded TRANSPARENTLY -- not gated by `forward_client_headers`
    /// (empty here, the secure-by-default posture that own mode honors).
    #[test]
    fn forwarded_leg_forwards_all_client_claude_code_headers_transparently() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        let req = forwarded_req(
            &[],
            &[
                ("x-claude-code-session-id", "client-sid-abc"),
                ("x-claude-code-agent-id", "client-agent-9"),
            ],
            &[],
        );

        assert_eq!(
            outbound_header_value(&provider, &req, "x-claude-code-agent-id").as_deref(),
            Some("client-agent-9"),
            "a forwarded leg forwards every captured x-claude-code-* header, allowlist-free",
        );
    }

    /// Forwarded leg: the client's `anthropic-beta` set is emitted verbatim
    /// and the minted 14-flag Claude Code floor is SUPPRESSED, so Anthropic
    /// sees exactly the client's betas.
    #[test]
    fn forwarded_leg_client_anthropic_beta_wins_and_floor_suppressed() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        let req = forwarded_req(&[], &[], &["client-only-beta"]);

        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present with client betas");
        let betas: Vec<&str> = value.split(',').map(str::trim).collect();
        assert!(
            betas.contains(&"client-only-beta"),
            "the client's beta must reach the header; got {value}",
        );
        assert!(
            !betas.contains(&"claude-code-20250219"),
            "the minted CC beta floor must be suppressed on the forwarded leg; got {value}",
        );
        assert!(
            !betas.contains(&"oauth-2025-04-20"),
            "no minted floor beta may leak on the forwarded leg; got {value}",
        );
    }

    /// Forwarded leg: the standard Anthropic protocol version reaches the
    /// upstream (Claude Code and routectl both use 2023-06-01), so the
    /// client's version flows through unchanged.
    #[test]
    fn forwarded_leg_emits_client_anthropic_version() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        let req = forwarded_req(&[], &[], &[]);

        assert_eq!(
            outbound_header_value(&provider, &req, "anthropic-version").as_deref(),
            Some("2023-06-01"),
            "the Anthropic protocol version must reach the upstream on the forwarded leg",
        );
    }

    /// Security: the forwarded bearer is read only as a GATE by
    /// build_headers, never emitted as a header value. No outbound header
    /// (identity or otherwise) may carry the raw token.
    #[test]
    fn forwarded_leg_never_leaks_forwarded_token_in_any_header() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            true,
        ));
        let req = forwarded_req(
            &[("x-stainless-package-version", "1.2.3-client")],
            &[("x-claude-code-session-id", "client-sid-abc")],
            &["client-only-beta"],
        );

        let pairs = outbound_header_pairs(&provider, &req);
        for (name, value) in &pairs {
            assert!(
                !value.contains(FORWARDED_TOKEN_CANARY),
                "the forwarded token must never appear in any header value; leaked in {name}: {value}",
            );
        }
    }

    /// Own-mode-unchanged guard: with `forwarded_bearer` None, even when the
    /// carrier happens to hold `stainless_headers` + `claude_code_headers`,
    /// the minted fingerprint STILL wins -- the override is gated strictly
    /// on the forwarded bearer, not on the mere presence of captured
    /// headers. `forward_client_headers` is empty (own-mode secure default),
    /// so no captured header reaches the wire.
    ///
    /// The captured `x-claude-code-session-id` also makes this request
    /// `is_non_cc() == false` (genuine CC) under the default Auto cloak
    /// mode, so the fingerprint-widening beta floor is correctly SUPPRESSED
    /// here -- only the unconditional `oauth-2025-04-20` gate flag survives.
    #[test]
    fn own_mode_keeps_minted_fingerprint_even_with_captured_headers() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("minted-sid-known".into()),
            Vec::new(),
            false,
        ));
        // No forwarded bearer -> own mode, but the carrier is populated as
        // if it had been captured, to prove the gate ignores it.
        let mut req = ChatRequest::default();
        req.routectl_internal.stainless_headers =
            vec![("x-stainless-package-version".into(), "1.2.3-client".into())];
        req.routectl_internal.claude_code_headers =
            vec![("x-claude-code-session-id".into(), "client-sid-abc".into())];

        // Minted Stainless fingerprint wins (client value ignored).
        assert_eq!(
            outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
            Some(minted_stainless_package_version().as_str()),
            "own mode must keep the minted Stainless fingerprint",
        );
        // Minted session id wins (client value ignored, allowlist empty).
        assert_eq!(
            outbound_header_value(&provider, &req, "x-claude-code-session-id").as_deref(),
            Some("minted-sid-known"),
            "own mode must keep the minted session id",
        );
        // Genuine-CC (is_non_cc == false): the fingerprint-widening floor
        // is suppressed, but the OAuth gate flag still reaches the wire.
        let value = outbound_header_value(&provider, &req, "anthropic-beta")
            .expect("anthropic-beta header must be present (oauth gate flag)");
        assert!(
            !value.split(',').any(|b| b.trim() == "claude-code-20250219"),
            "genuine-CC request must NOT get the widening CC beta floor; got {value}",
        );
        assert!(
            value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
            "the unconditional oauth gate flag must still be present; got {value}",
        );
    }

    // -- cloak_body gate + body rewrite ------------------------------------
    /// Body carrying a Claude Code billing block + a client system block,
    /// used by the cloak_body tests so both the billing strip and the
    /// (non-)identity-stamp are observable in one body.
    fn cloak_test_body() -> Value {
        serde_json::json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
                {"type": "text", "text": "client system prompt"},
            ],
            "messages": [{"role": "user", "content": "hello"}]
        })
    }

    /// True when any `system` block's text starts with the billing prefix.
    fn body_has_billing(body: &Value) -> bool {
        body["system"].as_array().is_some_and(|arr| {
            arr.iter().any(|b| {
                b["text"]
                    .as_str()
                    .is_some_and(|t| t.trim_start().starts_with("x-anthropic-billing-header:"))
            })
        })
    }

    /// (a) OauthBearer + api.anthropic.com + NON-CC req (no captured
    /// `x-claude-code-session-id`): the body's `system` is reduced to the
    /// interactive identity line only, the client system is relocated into
    /// the first user message as a `<system-reminder>`, `metadata.user_id` is
    /// minted, AND the billing block is stripped.
    #[test]
    fn cloak_body_non_cc_stamps_identity_and_metadata_and_strips_billing() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("session-stable-123".into()),
            Vec::new(),
            false,
        ));
        // Non-CC: no x-claude-code-session-id captured.
        let req = req_with_claude_code_headers(vec![("x-claude-code-agent-id", "aid-7")]);
        let mut body = cloak_test_body();

        provider.cloak_body(&mut body, &req);

        // System is identity-only.
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1, "system must be reduced to identity only");
        assert_eq!(
            arr[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        // Client system relocated into the first user message as a reminder.
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "<system-reminder>\nclient system prompt\n</system-reminder>"
        );
        // Metadata user_id minted.
        assert!(
            body["metadata"]["user_id"].is_string(),
            "non-CC cloak must mint metadata.user_id"
        );
        // Billing block stripped.
        assert!(
            !body_has_billing(&body),
            "billing block must be stripped on the non-CC path"
        );
    }

    /// (b) OauthBearer + api.anthropic.com + GENUINE-CC req (captured
    /// `x-claude-code-session-id`): the billing block is stripped, but NO
    /// identity stamp and NO metadata mint.
    #[test]
    fn cloak_body_genuine_cc_strips_billing_only() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("session-stable-123".into()),
            Vec::new(),
            false,
        ));
        // Genuine CC: the session-id header is present in the capture.
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let mut body = cloak_test_body();

        provider.cloak_body(&mut body, &req);

        // Billing block stripped, leaving only the client system block.
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1, "only the client system block must remain");
        assert_eq!(arr[0]["text"], "client system prompt");
        assert!(
            !body_has_billing(&body),
            "billing block must be stripped on the genuine-CC path"
        );
        // No identity stamp, no metadata mint.
        assert!(
            body.get("metadata").is_none(),
            "genuine-CC path must not mint metadata"
        );
        // The genuine-CC path must not relocate the client system: no
        // system-reminder block appears anywhere.
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("<system-reminder>"),
            "genuine-CC path must not add a system-reminder block"
        );
    }

    /// (c) ApiKey path (api.anthropic.com): the gate skips, so the body is
    /// completely untouched -- billing block stays, no identity, no metadata.
    #[test]
    fn cloak_body_api_key_path_leaves_body_untouched() {
        let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
        let req = req_with_claude_code_headers(Vec::new());
        let mut body = cloak_test_body();
        let before = body.clone();

        provider.cloak_body(&mut body, &req);

        assert_eq!(
            body, before,
            "ApiKey path must leave the body untouched (gate skips)"
        );
    }

    /// (d) OauthBearer + NON-anthropic host: the gate skips, so the body is
    /// completely untouched.
    #[test]
    fn cloak_body_non_anthropic_host_leaves_body_untouched() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://example.invalid",
            Some("session-stable-123".into()),
            Vec::new(),
            false,
        ));
        let req = req_with_claude_code_headers(Vec::new());
        let mut body = cloak_test_body();
        let before = body.clone();

        provider.cloak_body(&mut body, &req);

        assert_eq!(
            body, before,
            "non-anthropic host must leave the body untouched (gate skips)"
        );
    }

    // -- cloak mode (T6) ---------------------------------------------------

    /// Build an OauthBearer + api.anthropic.com provider with an explicit
    /// `CloakConfig` and a stable session id, for the mode tests.
    fn oauth_provider_with_cloak(cloak: CloakConfig) -> AnthropicApiProvider {
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
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: Some("session-stable-123".into()),
            cloak,
            use_forwarded_bearer: false,
        };
        AnthropicApiProvider::new(cfg)
    }

    /// `is_non_cc` under `CloakMode::Always` is unconditionally true,
    /// regardless of whether a session-id header is present.
    #[test]
    fn is_non_cc_always_is_true() {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode: CloakMode::Always,
            ..CloakConfig::default()
        });
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        assert!(provider.is_non_cc(&req));
    }

    /// `is_non_cc` under `CloakMode::Never` is unconditionally false,
    /// regardless of whether a session-id header is present. This arm has
    /// no inline equivalent today -- it exists for `build_headers`, which
    /// (unlike `cloak_body`) does not early-return on `Never`.
    #[test]
    fn is_non_cc_never_is_false() {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode: CloakMode::Never,
            ..CloakConfig::default()
        });
        let req = req_with_claude_code_headers(Vec::new());
        assert!(!provider.is_non_cc(&req));
    }

    /// `is_non_cc` under `CloakMode::Auto` is false when a captured
    /// `x-claude-code-session-id` header is present, matched
    /// case-insensitively.
    #[test]
    fn is_non_cc_auto_is_false_when_session_header_present() {
        let provider = oauth_provider_with_cloak(CloakConfig::default());
        let req = req_with_claude_code_headers(vec![("X-Claude-Code-Session-Id", "sid-42")]);
        assert!(!provider.is_non_cc(&req));
    }

    /// `is_non_cc` under `CloakMode::Auto` is true when no session-id
    /// header was captured.
    #[test]
    fn is_non_cc_auto_is_true_when_session_header_absent() {
        let provider = oauth_provider_with_cloak(CloakConfig::default());
        let req = req_with_claude_code_headers(Vec::new());
        assert!(provider.is_non_cc(&req));
    }

    /// `mode = never` skips ALL cloak transforms: billing block NOT stripped,
    /// identity NOT injected, `mcp_` NOT normalized, and `cloak_body` returns
    /// None.
    #[test]
    fn cloak_mode_never_skips_all_transforms() {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode: CloakMode::Never,
            ..CloakConfig::default()
        });
        // Non-CC request, with a tool that would normally be normalized.
        let req = req_with_claude_code_headers(Vec::new());
        let mut body = serde_json::json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: v=1"},
                {"type": "text", "text": "client system prompt"},
            ],
            "tools": [{"name": "mcp_foo"}]
        });
        let before = body.clone();

        let result = provider.cloak_body(&mut body, &req);

        assert!(
            result.is_none(),
            "mode=never must return None from cloak_body"
        );
        assert_eq!(body, before, "mode=never must leave the body untouched");
        // Explicitly: billing block survives and mcp_ is NOT normalized.
        assert!(
            body_has_billing(&body),
            "billing block must survive mode=never"
        );
        assert_eq!(body["tools"][0]["name"], "mcp_foo");
    }

    /// `mode = always` cloaks as a non-CC client even when the request DID
    /// carry an `x-claude-code-session-id` capture (which `Auto` would treat
    /// as genuine CC): identity stamped + metadata minted.
    #[test]
    fn cloak_mode_always_stamps_identity_even_with_session_header() {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode: CloakMode::Always,
            ..CloakConfig::default()
        });
        // Genuine-CC-looking request: session-id header present.
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let mut body = cloak_test_body();

        provider.cloak_body(&mut body, &req);

        // Despite the session header, identity is stamped and metadata minted.
        assert_eq!(
            body["system"][0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert!(
            body["metadata"]["user_id"].is_string(),
            "mode=always must mint metadata.user_id even with a session header"
        );
        assert!(!body_has_billing(&body), "billing block must be stripped");
    }

    /// `mode = auto` (the default) keeps the original heuristic: a request
    /// WITH a session-id capture is treated as genuine CC (no identity stamp,
    /// no metadata), billing still stripped.
    #[test]
    fn cloak_mode_auto_matches_increment1_for_genuine_cc() {
        let provider = oauth_provider_with_cloak(CloakConfig::default());
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let mut body = cloak_test_body();

        provider.cloak_body(&mut body, &req);

        // Genuine CC under Auto: only the client block remains, no metadata.
        let arr = body["system"].as_array().expect("system is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "client system prompt");
        assert!(body.get("metadata").is_none());
        assert!(!body_has_billing(&body));
    }

    /// Build a `reqwest::Response` from a status + body for driving
    /// `read_anthropic_error` directly, without a live HTTP round-trip or
    /// the `complete()` path's global-tracing side effects (which race the
    /// `#[traced_test]` upstream_log tests in this crate's test binary).
    ///
    /// `http::Response` is only in scope under the `bedrock` feature
    /// (`dep:http`), which the default and `--all-features` builds both
    /// enable -- so these tests run under the standard gate.
    #[cfg(feature = "bedrock")]
    fn reqwest_response(status: u16, body: &str) -> reqwest::Response {
        let http_resp = http::Response::builder()
            .status(status)
            .body(body.to_string())
            .expect("build http::Response");
        reqwest::Response::from(http_resp)
    }

    /// A structured Anthropic `{error:...}` 400 must carry the RAW JSON
    /// envelope in `Error::Upstream.body` so the ingress sanitizer can
    /// re-extract the upstream's own `error.message` for the client. This
    /// is the recovery lever: Claude Code self-heals a stale thinking-block
    /// 400 only if it can SEE the message.
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn read_anthropic_error_carries_raw_envelope_for_structured_400() {
        let raw = "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\
                    \"message\":\"messages.23.content.5: `thinking` or `redacted_thinking` \
                    blocks in the latest assistant message cannot be modified.\"}}";
        let resp = reqwest_response(400, raw);

        let (msg, err) = read_anthropic_error("anthropic_oauth_prod", 400, resp).await;

        // The returned `msg` stays the clean extracted message for logging.
        assert!(
            msg.contains("cannot be modified"),
            "returned msg is the extracted message: {msg:?}"
        );
        match err {
            Error::Upstream {
                status,
                body,
                upstream_type,
                ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(upstream_type.as_deref(), Some("invalid_request_error"));
                // `.body` must be the RAW envelope so the ingress sanitizer
                // re-parses `/error/message`.
                let parsed: Value =
                    serde_json::from_str(&body).expect("body must still be the raw JSON envelope");
                assert_eq!(
                    parsed.pointer("/error/message").and_then(Value::as_str),
                    Some(
                        "messages.23.content.5: `thinking` or `redacted_thinking` \
                         blocks in the latest assistant message cannot be modified."
                    )
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    /// A non-JSON upstream body (HTML gateway page) must NOT be carried raw
    /// in `.body`; the sanitized excerpt is stored so the ingress sanitizer
    /// falls back to a status-only client message and nothing leaks.
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn read_anthropic_error_sanitizes_non_json_body() {
        let resp = reqwest_response(
            502,
            "<html><body>upstream-host gateway timeout</body></html>",
        );

        let (_msg, err) = read_anthropic_error("anthropic_oauth_prod", 502, resp).await;

        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 502);
                assert!(
                    !body.contains("upstream-host"),
                    "raw HTML body must not be carried in .body: {body:?}"
                );
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }

    // -- forwarded-bearer host-pinned token resolution --------------------

    /// A `TokenSource` whose `token()` ALWAYS errors. Used to PROVE that
    /// the forwarded-passthrough path never calls `self.cfg.auth.token()`:
    /// if the resolver touched this source, resolution would fail and the
    /// test would observe an `Err` instead of the forwarded token. The
    /// synthetic pure-proxy provider has no live routectl credential, so
    /// this models "calling cfg.auth.token() here would error".
    #[derive(Debug)]
    struct FailingTokenSource;

    #[async_trait]
    impl TokenSource for FailingTokenSource {
        async fn token(&self) -> Result<String> {
            Err(Error::Auth(
                "FailingTokenSource: token() must not be called on the forwarded path".into(),
            ))
        }
    }

    /// Build an OauthBearer provider with a chosen `base_url`, token
    /// source, and forwarded-gate setting, mirroring a
    /// `credential_source = "forwarded"` provider entry (OauthBearer,
    /// `api.anthropic.com` by default). Pass `use_forwarded_bearer: false`
    /// to model a coexisting own-creds Anthropic provider instead.
    fn oauth_cfg_with_auth(
        base_url: &str,
        auth: Arc<dyn TokenSource>,
        use_forwarded_bearer: bool,
    ) -> AnthropicApiConfig {
        AnthropicApiConfig {
            id: "test".into(),
            auth,
            base_url: base_url.into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
            cloak: CloakConfig::default(),
            use_forwarded_bearer,
        }
    }

    /// A default request carrying a forwarded first-party bearer, as the
    /// ingress populates it in forwarded (pure-proxy) mode on the MITM
    /// Anthropic leg. `RoutectlInternal` is `#[non_exhaustive]`, so mutate
    /// the single field on the default value.
    fn req_with_forwarded_bearer(token: &str) -> ChatRequest {
        let mut req = ChatRequest::default();
        req.routectl_internal.forwarded_bearer =
            Some(routectl_core::ForwardedBearer::new(token.to_string()));
        req
    }

    /// Resolve the effective token through the host-pinned resolver, stamp
    /// it via `build_headers`, and return the built outbound request so a
    /// test can inspect the exact headers that would go on the wire.
    async fn build_wire_request(
        provider: &AnthropicApiProvider,
        req: &ChatRequest,
    ) -> reqwest::Request {
        let token = provider
            .resolve_effective_token(req)
            .await
            .expect("effective token must resolve");
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        provider
            .build_headers(rb, req, &token)
            .0
            .build()
            .expect("build outbound request")
    }

    /// True when `needle` appears in ANY header value on the built request.
    fn any_header_value_contains(request: &reqwest::Request, needle: &str) -> bool {
        request
            .headers()
            .iter()
            .filter_map(|(_, v)| v.to_str().ok())
            .any(|v| v.contains(needle))
    }

    /// forwarded_bearer Some + base_url host == api.anthropic.com: the
    /// resolver returns the FORWARDED token and NEVER calls
    /// `self.cfg.auth.token()`. Proof: the auth source ERRORS on every
    /// call, yet resolution succeeds -- so the resolver could not have
    /// touched it. This is the errors-if-cfg.auth.token()-called proof.
    #[tokio::test]
    async fn resolve_forwarded_bearer_on_anthropic_host_skips_cfg_auth() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(FailingTokenSource),
            true,
        ));
        let req = req_with_forwarded_bearer("forwarded-full-scope-token");

        let token = provider
            .resolve_effective_token(&req)
            .await
            .expect("forwarded path must not call cfg.auth.token()");

        assert_eq!(
            token, "forwarded-full-scope-token",
            "forwarded token must be used verbatim as the effective token"
        );
    }

    /// WIRE: on the anthropic host, the forwarded token is stamped as the
    /// outbound `Authorization: Bearer <forwarded>` (the synthetic
    /// pure-proxy provider is OauthBearer). End-to-end through
    /// `build_headers`, with a failing auth source that is never consulted.
    #[tokio::test]
    async fn forwarded_bearer_stamped_as_bearer_on_anthropic_host() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(FailingTokenSource),
            true,
        ));
        let req = req_with_forwarded_bearer("forwarded-full-scope-token");

        let request = build_wire_request(&provider, &req).await;

        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        assert_eq!(
            auth,
            Some("Bearer forwarded-full-scope-token"),
            "forwarded token must be stamped as the outbound Bearer on the anthropic host"
        );
    }

    /// base_url host != api.anthropic.com (a proxy / self-host) +
    /// forwarded_bearer Some: the forwarded token is IGNORED. The resolver
    /// returns the provider's OWN token and the forwarded token never
    /// appears on any outbound header for that host.
    #[tokio::test]
    async fn forwarded_bearer_ignored_on_non_anthropic_host() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://proxy.example.com",
            Arc::new(StaticToken::new("provider-own-token")),
            true,
        ));
        let req = req_with_forwarded_bearer("forwarded-should-be-ignored");

        let token = provider
            .resolve_effective_token(&req)
            .await
            .expect("non-anthropic host resolves the provider's own token");
        assert_eq!(
            token, "provider-own-token",
            "non-anthropic host must resolve the provider's own token, not the forwarded one"
        );

        let request = build_wire_request(&provider, &req).await;
        assert!(
            !any_header_value_contains(&request, "forwarded-should-be-ignored"),
            "the forwarded token must never reach the wire on a non-anthropic host"
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer provider-own-token"),
            "the provider's own resolved token is what gets stamped on a non-anthropic host"
        );
    }

    /// A sibling-domain look-alike base (`api.anthropic.com.evil.example`)
    /// is NOT the anthropic host: the forwarded full-scope token must NOT
    /// be sent there. Defends the exact-host pin end-to-end through the
    /// resolver (guards against a substring host check leaking the token to
    /// a takeover domain).
    #[tokio::test]
    async fn forwarded_bearer_ignored_on_lookalike_anthropic_host() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com.evil.example",
            Arc::new(StaticToken::new("provider-own-token")),
            true,
        ));
        let req = req_with_forwarded_bearer("forwarded-full-scope-token");

        let token = provider
            .resolve_effective_token(&req)
            .await
            .expect("look-alike host resolves the provider's own token");
        assert_eq!(
            token, "provider-own-token",
            "a look-alike host must not receive the forwarded token"
        );

        let request = build_wire_request(&provider, &req).await;
        assert!(
            !any_header_value_contains(&request, "forwarded-full-scope-token"),
            "the forwarded token must never reach a look-alike anthropic host"
        );
    }

    /// The coexistence-bug regression: an OWN-creds Anthropic provider
    /// (`use_forwarded_bearer` false) on the exact anthropic host with a
    /// floating captured bearer present (e.g. captured for a sibling
    /// forwarded provider on the same router) must NOT consume it. The
    /// resolver returns the provider's own token, and no outbound header
    /// carries the floating bearer.
    #[tokio::test]
    async fn own_provider_ignores_floating_forwarded_bearer_on_anthropic_host() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(StaticToken::new("provider-own-token")),
            false,
        ));
        let req = req_with_forwarded_bearer("floating-forwarded-bearer");

        let token = provider
            .resolve_effective_token(&req)
            .await
            .expect("own-mode provider resolves its own token");
        assert_eq!(
            token, "provider-own-token",
            "an own-mode provider must resolve its own token even with a floating bearer present"
        );

        let request = build_wire_request(&provider, &req).await;
        assert!(
            !any_header_value_contains(&request, "floating-forwarded-bearer"),
            "the floating bearer must never reach the wire for an own-mode provider"
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer provider-own-token"),
            "the own-mode provider's own token is what gets stamped, not the floating bearer"
        );
    }

    /// The pure `should_use_forwarded_bearer` predicate shared by
    /// `resolve_effective_token` and the `build_headers` forwarded leg.
    /// Baseline: TRUE only when all three legs hold (configured forwarded +
    /// bearer present + exact anthropic host). Each case below flips
    /// exactly one leg off the baseline and must land on false --
    /// including the two coexistence cases: a forwarded provider on a
    /// non-anthropic host, and an own-mode provider with a bearer present
    /// on the exact anthropic host. Host-pinned egress cannot be driven
    /// through wiremock, so this matrix is the full end-to-end proof of the
    /// gate's logic.
    #[test]
    fn should_use_forwarded_bearer_gate_matrix() {
        let cases: &[(bool, bool, &str, bool)] = &[
            // (use_forwarded_bearer, has_bearer, base_url, expected)
            (true, true, "https://api.anthropic.com", true),
            (false, true, "https://api.anthropic.com", false),
            (true, false, "https://api.anthropic.com", false),
            (true, true, "https://proxy.example.com", false),
            (false, false, "https://api.anthropic.com", false),
            (false, true, "https://proxy.example.com", false),
            (true, false, "https://proxy.example.com", false),
            (false, false, "https://proxy.example.com", false),
            (true, true, "https://api.anthropic.com.evil.example", false),
        ];

        for (use_forwarded_bearer, has_bearer, base_url, expected) in cases {
            assert_eq!(
                should_use_forwarded_bearer(*use_forwarded_bearer, *has_bearer, base_url),
                *expected,
                "use_forwarded_bearer={use_forwarded_bearer} has_bearer={has_bearer} \
                 base_url={base_url} expected={expected}"
            );
        }
    }

    /// forwarded_bearer None on the anthropic host: identical to today --
    /// the resolver returns the provider's own token via cfg.auth.token().
    #[tokio::test]
    async fn forwarded_bearer_none_resolves_provider_token() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(StaticToken::new("provider-token")),
            true,
        ));
        // ChatRequest::default() leaves forwarded_bearer None.
        let req = ChatRequest::default();

        let token = provider
            .resolve_effective_token(&req)
            .await
            .expect("token resolves");
        assert_eq!(
            token, "provider-token",
            "the None path must resolve the provider's own token"
        );
    }

    /// forwarded_bearer None on the anthropic host STILL calls
    /// cfg.auth.token() -- the None path is behaviorally identical to the
    /// pre-passthrough egress. Proof: with a failing auth source and no
    /// forwarded token, resolution errors (it can only error by calling
    /// cfg.auth.token()).
    #[tokio::test]
    async fn forwarded_bearer_none_still_calls_cfg_auth() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(FailingTokenSource),
            true,
        ));
        let req = ChatRequest::default();

        let result = provider.resolve_effective_token(&req).await;
        assert!(
            result.is_err(),
            "the None path must resolve through cfg.auth.token(), which errors here"
        );
    }

    /// The resolver must never log the forwarded token. Drive the forwarded
    /// path under a log capture and assert the token string is absent from
    /// every emitted event -- a regression guard against a future debug log
    /// in the resolver. Uses a current-thread runtime so the test stays on
    /// the crate's established `#[traced_test] #[test]` shape.
    #[traced_test]
    #[test]
    fn resolve_forwarded_bearer_does_not_log_token() {
        let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
            "https://api.anthropic.com",
            Arc::new(FailingTokenSource),
            true,
        ));
        let secret = "forwarded-full-scope-SECRET-abc123";
        let req = req_with_forwarded_bearer(secret);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current-thread runtime");
        let token = rt
            .block_on(provider.resolve_effective_token(&req))
            .expect("forwarded token resolves");
        assert_eq!(token, secret);

        assert!(
            !logs_contain(secret),
            "the forwarded token must never be logged by the resolver"
        );
    }

    // -- beta-decision observability ----------------------------------------

    /// A genuine Claude Code request (a captured `x-claude-code-session-id`
    /// header) classifies as NOT non-CC, but the mandatory OAuth gate still
    /// fires independent of that classification. The non-CC-only floor
    /// (and therefore `context-1m-2025-08-07`) must NOT widen the beta set
    /// for a genuine CC client.
    #[test]
    fn beta_decision_reflects_genuine_cc_request() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        let (_rb, decision) = provider.build_headers(rb, &req, "test-token");

        assert!(
            !decision.is_non_cc,
            "a captured session-id header must classify as genuine-CC"
        );
        assert!(
            decision.oauth_added,
            "the mandatory oauth gate must fire independent of is_non_cc"
        );
        assert!(
            !decision.has_context_1m_beta,
            "a genuine-CC request must not be floor-widened with context-1m"
        );
    }

    /// The mirror case: no captured session-id header classifies as
    /// non-CC, and the pinned Claude Code beta floor (including
    /// `context-1m-2025-08-07`) widens the outgoing beta set.
    #[test]
    fn beta_decision_reflects_non_cc_request() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let req = req_with_claude_code_headers(Vec::new());
        let client = reqwest::Client::new();
        let rb = client.post("http://127.0.0.1/test");
        let (_rb, decision) = provider.build_headers(rb, &req, "test-token");

        assert!(
            decision.is_non_cc,
            "no captured session-id header must classify as non-CC"
        );
        assert!(
            decision.oauth_added,
            "the mandatory oauth gate must fire for non-CC too"
        );
        assert!(
            decision.has_context_1m_beta,
            "a non-CC request must be floor-widened with context-1m"
        );
    }

    /// Drive `log_beta_decision_on_4xx` directly (bypassing a full HTTP
    /// round-trip) and assert the beta-context fields land on the emitted
    /// event, so a beta-caused 400 recurrence is diagnosable without
    /// enabling header tracing.
    #[traced_test]
    #[test]
    fn log_beta_decision_on_4xx_emits_beta_context_fields() {
        let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        let decision = BetaDecision {
            is_non_cc: true,
            forwarded_leg: false,
            cloak_mode: CloakMode::Auto,
            oauth_added: true,
            has_context_1m_beta: true,
            has_context_management_beta: false,
        };

        provider.log_beta_decision_on_4xx(400, &decision, "invalid_request_error: bad beta");

        assert!(logs_contain(
            "anthropic-api oauth 4xx beta decision context"
        ));
        assert!(logs_contain("status=400"));
        assert!(logs_contain("is_non_cc=true"));
        assert!(logs_contain("oauth_added=true"));
        assert!(logs_contain("has_context_1m_beta=true"));
        assert!(logs_contain("has_context_management_beta=false"));
    }

    /// `should_log_beta_4xx` is the single gate shared by `complete()`,
    /// `stream()`, and `count_tokens()` -- exercise the full matrix here
    /// instead of trusting three copy-pasted conditions to stay in sync.
    /// Baseline: TRUE only for a 4xx, OauthBearer, api.anthropic.com,
    /// non-forwarded request. Each deviation below flips exactly one
    /// dimension of that baseline and must land on false.
    #[test]
    fn should_log_beta_4xx_gate_matrix() {
        let oauth_provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
        assert!(
            oauth_provider.should_log_beta_4xx(400, false),
            "baseline: 4xx + OauthBearer + api.anthropic.com + own leg must fire"
        );

        for status in [500, 502] {
            assert!(
                !oauth_provider.should_log_beta_4xx(status, false),
                "5xx status {status} must not fire (beta gating cannot cause a 5xx)"
            );
        }

        let api_key_provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
        assert!(
            !api_key_provider.should_log_beta_4xx(400, false),
            "ApiKey auth must not fire even on api.anthropic.com"
        );

        let non_anthropic_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
            "https://example.invalid",
            None,
            Vec::new(),
            false,
        ));
        assert!(
            !non_anthropic_provider.should_log_beta_4xx(400, false),
            "a non-anthropic base_url must not fire even with OauthBearer"
        );

        assert!(
            !oauth_provider.should_log_beta_4xx(400, true),
            "forwarded_leg must suppress the log (own-token lane only)"
        );

        for status in [200, 204, 301] {
            assert!(
                !oauth_provider.should_log_beta_4xx(status, false),
                "2xx/3xx status {status} must not fire"
            );
        }
    }
}
