//! Anthropic provider construction, auth-kind resolution, and header plumbing.

use std::sync::Arc;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use routectl_core::identity::anthropic::is_anthropic_api_host;
use routectl_core::{ChatRequest, Result, StaticToken, TokenSource};

use super::cloak::{self, CloakConfig};
use super::{context_management, ratelimit_unified, request};
#[cfg(feature = "bedrock")]
use crate::mantle::MantleAuth;

/// How the provider authenticates to the Anthropic Messages API.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
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

/// Configuration for an anthropic-api egress provider.
#[derive(Clone)]
pub struct AnthropicApiConfig {
    /// Provider identifier used in tracing and log fields.
    pub id: String,
    /// Source of the bearer/API-key token. For env/file/literal
    /// secret refs, this is a `StaticToken` resolved once at
    /// construction. For `oauth://<provider>` refs, the factory
    /// passes a `ManagedToken` impl that re-resolves through
    /// `SecretStore::get` per request -- so token rotation in
    /// `~/.config/routectl/credentials.json` is picked up live
    /// without restarting routectl.
    pub auth: Arc<dyn TokenSource>,
    /// Base URL of the upstream Messages API.
    pub base_url: String,
    /// Value sent in the `anthropic-version` header.
    pub anthropic_version: String,
    /// Selects the authentication scheme used to present the token.
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
    ///
    /// CARVE-OUT: the structured-outputs beta
    /// (`STRUCTURED_OUTPUTS_BETA`) is force-added whenever the assembled
    /// body carries `output_config.format`, regardless of this list -- it
    /// is a routectl-derived server requirement implied by the in-use
    /// feature, not a client-opted beta. To deny structured outputs, deny
    /// the feature. See `docs/CONFIGURATION.md`.
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
    /// Bedrock mantle authentication. `Some` selects the mantle lane
    /// (SigV4/bearer signing under the `bedrock-mantle` scope, no
    /// first-party `x-api-key`/Claude-Code plumbing, a no-redirect
    /// client). `None` (default) keeps the first-party api.anthropic.com
    /// behavior byte-for-byte. Resolved at the factory from a
    /// `bedrock_mantle` sub-config.
    #[cfg(feature = "bedrock")]
    pub mantle: Option<MantleAuth>,
}

impl std::fmt::Debug for AnthropicApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-rolled Debug elides the auth source (its own Debug
        // already redacts, but this saves one round-trip if a
        // future TokenSource impl ever leaks).
        let mut d = f.debug_struct("AnthropicApiConfig");
        d.field("id", &self.id)
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
            .field("use_forwarded_bearer", &self.use_forwarded_bearer);
        // Mantle lane presence + its own redacting Debug (region + auth
        // shape only, never credential material).
        #[cfg(feature = "bedrock")]
        d.field("mantle", &self.mantle);
        d.finish()
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
            #[cfg(feature = "bedrock")]
            mantle: None,
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
pub(super) struct BetaDecision {
    pub(super) is_non_cc: bool,
    pub(super) forwarded_leg: bool,
    pub(super) cloak_mode: cloak::CloakMode,
    pub(super) oauth_added: bool,
    pub(super) has_context_1m_beta: bool,
    pub(super) has_context_management_beta: bool,
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
pub(super) fn should_use_forwarded_bearer(
    use_forwarded_bearer: bool,
    has_bearer: bool,
    base_url: &str,
) -> bool {
    use_forwarded_bearer && has_bearer && is_anthropic_api_host(base_url)
}

/// anthropic-api Messages egress provider.
pub struct AnthropicApiProvider {
    pub(super) cfg: AnthropicApiConfig,
    pub(super) client: Client,
    pub(super) thinking_cache: std::sync::Arc<std::sync::RwLock<context_management::ThinkingCache>>,
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

/// FORWARDING TRANSPARENCY CONTRACT
///
/// On the forwarded (pure-proxy) leg -- `forwarded_leg(req) == true`, the
/// three-way pin of `use_forwarded_bearer` + a captured bearer + the
/// api.anthropic.com host -- the egress is a BYTE-TRANSPARENT forwarder of the
/// client's request. The following body/header mutations MUST NOT run on that
/// leg, so the client's real bytes and fingerprint reach Anthropic verbatim:
///
///   1. the client-beta `allowed_betas` filter (the anthropic-beta HEADER),
///   2. `cloak_body` (billing strip, identity stamp, tool-name normalization),
///   3. `resign_cch_in_place` (the billing-checksum re-sign),
///   4. the minted OAuth/Claude-Code beta-floor injection.
///
/// The predicate is always derived via the single `forwarded_leg` helper --
/// self-gating inside `cloak_body`, and a local computed at the top of each
/// dispatch method -- and every new body-mutation site added here MUST carry
/// the `!forwarded_leg` gate. Own mode (`forwarded_leg(req) == false`) behavior
/// is byte-for-byte unchanged by this contract.
impl AnthropicApiProvider {
    /// Build a provider from its configuration.
    pub fn new(cfg: AnthropicApiConfig) -> Self {
        let ua = resolve_user_agent(cfg.user_agent.as_deref(), cfg.auth_kind);
        // The mantle lane uses a no-redirect client: a signed POST must
        // never be auto-followed across a 3xx, since replaying the SigV4
        // signature against a different host always fails and a redirect
        // on this lane is an upstream fault to surface, not to chase. The
        // first-party lane keeps the stock (redirect-following) client.
        #[cfg(feature = "bedrock")]
        let client = if cfg.mantle.is_some() {
            crate::http_client::build_no_redirect(ua.as_deref())
                .expect("reqwest no-redirect client build failed (TLS init?); fatal at startup")
        } else {
            crate::http_client::build(ua.as_deref())
        };
        #[cfg(not(feature = "bedrock"))]
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

    pub(super) fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    pub(super) fn count_tokens_url(&self) -> String {
        format!(
            "{}/v1/messages/count_tokens",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    /// True when this provider egresses on the Bedrock mantle lane. On
    /// this lane the signer owns auth (no `x-api-key`), the body is
    /// serialized to signable bytes, and a no-redirect client is used.
    /// Always `false` in a build without the `bedrock` feature.
    pub(super) const fn is_mantle(&self) -> bool {
        #[cfg(feature = "bedrock")]
        {
            self.cfg.mantle.is_some()
        }
        #[cfg(not(feature = "bedrock"))]
        {
            false
        }
    }

    /// SigV4/bearer-sign a built request in place on the mantle lane; a
    /// no-op on the first-party lane. Signing runs AFTER the request is
    /// fully built (method, URL, headers, body bytes) and BEFORE any
    /// header trace or execute, so the trace shows the real auth header
    /// and the signed input matches the transmitted bytes.
    #[cfg(feature = "bedrock")]
    pub(super) async fn sign_mantle(&self, request: &mut reqwest::Request) -> Result<()> {
        if let Some(mantle) = self.cfg.mantle.as_ref() {
            crate::mantle::sign(request, &mantle.creds, &mantle.region).await?;
        }
        Ok(())
    }

    /// Record the mantle lane context (`lane`, `auth_mode`, `region`) on
    /// the current tracing span so every event within it -- including the
    /// shared upstream-failure WARN -- carries the lane fields. A no-op on
    /// the first-party lane, where the span's `Empty` fields stay unset
    /// and never render.
    #[cfg(feature = "bedrock")]
    pub(super) fn record_mantle_span_fields(&self) {
        if let Some(mantle) = self.cfg.mantle.as_ref() {
            let span = tracing::Span::current();
            span.record("lane", crate::mantle::MANTLE_SERVICE);
            span.record("auth_mode", mantle.auth_mode());
            span.record("region", mantle.region.as_str());
        }
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
    pub(super) async fn resolve_effective_token(&self, req: &ChatRequest) -> Result<String> {
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

    /// The forwarded (pure-proxy) leg predicate: the single gate consulted by
    /// every body/header mutation site (see the FORWARDING TRANSPARENCY
    /// CONTRACT on the impl). Delegates to `should_use_forwarded_bearer` so
    /// build_headers and the dispatch methods can never disagree.
    pub(super) fn forwarded_leg(&self, req: &ChatRequest) -> bool {
        should_use_forwarded_bearer(
            self.cfg.use_forwarded_bearer,
            req.routectl_internal.forwarded_bearer.is_some(),
            &self.cfg.base_url,
        )
    }

    /// Compose the outbound headers, including the three-source
    /// `anthropic-beta` union.
    ///
    /// `wire_body` is the ASSEMBLED body this request will ship, when the
    /// caller has one. It is read ONLY to detect capability-driven beta
    /// requirements that depend on what actually lands on the wire (today:
    /// `output_config.format` -> `STRUCTURED_OUTPUTS_BETA`). The canonical
    /// `req` cannot answer that question -- the `provider_extras` merge and
    /// `reconcile_output_config_effort` add and remove `output_config` after
    /// translation, so `req.response_format` is not the shipped shape. `None`
    /// means "no body-derived betas", used by callers that compose headers
    /// without an assembled body.
    pub(super) fn build_headers(
        &self,
        rb: reqwest::RequestBuilder,
        req: &ChatRequest,
        token: &str,
        wire_body: Option<&Value>,
    ) -> (reqwest::RequestBuilder, BetaDecision) {
        let mut rb = rb.header("anthropic-version", &self.cfg.anthropic_version);
        rb = match self.cfg.auth_kind {
            // Mantle lane: the SigV4/bearer signer owns auth and attaches
            // it post-build, so no `x-api-key` here (the token is empty by
            // config validation anyway). anthropic-version still stamps
            // above, and the OauthBearer-gated Claude-Code identity headers
            // / UA never fire on this ApiKey lane.
            AuthKind::ApiKey if self.is_mantle() => rb,
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
        let forwarded_leg = self.forwarded_leg(req);

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
        //
        // SUPPRESSED on the forwarded leg: there the client's real beta set
        // must reach Anthropic VERBATIM (per the FORWARDING TRANSPARENCY
        // CONTRACT), so the allowlist filter is skipped and the client betas
        // pass through unfiltered.
        let filtered_req_betas: std::borrow::Cow<'_, [String]> = if forwarded_leg {
            std::borrow::Cow::Borrowed(req.anthropic_beta.as_slice())
        } else {
            request::filter_anthropic_betas(
                &self.cfg.id,
                &req.anthropic_beta,
                &self.cfg.allowed_betas,
            )
        };
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

        // Capability-driven union, LAST: a body carrying
        // `output_config.format` is rejected upstream unless the
        // structured-outputs beta rides along, on EVERY auth kind. Placed
        // after `filter_anthropic_betas` (and after the operator/floor
        // unions) because this is a server requirement implied by the
        // shipped body, not a client-opted beta subject to `allowed_betas`
        // -- the same standing the operator-pinned floor has. Idempotent, so
        // an OauthBearer request whose floor already carries the flag keeps
        // its beta list byte-identical.
        //
        // NOT gated on `is_non_cc`, unlike the pinned floor above -- this
        // union intentionally fires on the genuine-CC (`is_non_cc == false`)
        // path too. The floor's gate exists because the floor is
        // SPECULATIVE: it force-widens a real client's beta set with
        // capability flags CC never asked for, which Anthropic 400s. This
        // union is the opposite -- it fires ONLY when the assembled body
        // actually carries `output_config.format`, so a genuine-CC request
        // reaching here demonstrably uses the gated feature and needs its
        // flag, exactly as a cloaked one does.
        //
        // SUPPRESSED on the forwarded leg, like every other minted-beta site
        // here: there the client's own beta set must reach Anthropic verbatim
        // per the FORWARDING TRANSPARENCY CONTRACT, and the client owns the
        // forwarded body too -- so widening its fingerprint with a flag it
        // did not send is never routectl's call.
        if !forwarded_leg && let Some(body) = wire_body {
            super::extras::union_structured_outputs_beta(body, &mut merged_betas);
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
    pub(super) fn should_log_beta_4xx(&self, status: u16, forwarded_leg: bool) -> bool {
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
    pub(super) fn log_beta_decision_on_4xx(&self, status: u16, dec: &BetaDecision, excerpt: &str) {
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
    pub(super) fn is_non_cc(&self, req: &ChatRequest) -> bool {
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
    pub(super) fn cloak_body(
        &self,
        body: &mut Value,
        req: &ChatRequest,
    ) -> Option<cloak::CloakResult> {
        // Forwarded (pure-proxy) leg: the egress is a byte-transparent
        // forwarder (see the FORWARDING TRANSPARENCY CONTRACT), so no cloak
        // transform runs and the client's body reaches Anthropic verbatim.
        if self.forwarded_leg(req) {
            return None;
        }
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
    pub(super) fn observe_unified_quota(
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
pub(super) fn resolve_user_agent(user_agent: Option<&str>, auth_kind: AuthKind) -> Option<String> {
    match (user_agent, auth_kind) {
        (Some(ua), _) => Some(ua.to_string()),
        (None, AuthKind::OauthBearer) => {
            Some(routectl_core::identity::anthropic::default_claude_code_user_agent().to_string())
        }
        (None, AuthKind::ApiKey) => None,
    }
}
