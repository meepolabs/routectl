//! OpenAI Responses provider construction, config, and auth wiring.

use std::sync::Arc;

use reqwest::Client;
use routectl_core::{ChatRequest, Result, StaticToken, TokenSource};

use super::{AuthKind, auth, cookies};
#[cfg(feature = "bedrock")]
use crate::mantle::MantleAuth;

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
    /// Persistent per-installation id (UUIDv4), stamped as the
    /// `x-codex-installation-id` header on the ChatgptOauth surface.
    /// `Some` when the factory resolved (adopted or minted) the id from
    /// the config-dir file; `None` for ApiKey / BedrockMantle providers
    /// or when the file could not be read/created -- in every such case
    /// `build_headers` stamps no `x-codex-installation-id` header. Value
    /// never logged.
    pub installation_id: Option<String>,
    /// Bedrock mantle authentication. `Some` selects the mantle lane
    /// (SigV4/bearer signing under the `bedrock-mantle` scope, no bearer
    /// `Authorization` from the provider, a no-redirect client, and no
    /// Cloudflare cookie jar). `None` (default) keeps the chatgpt-oauth /
    /// api-key behavior byte-for-byte. Resolved at the factory from a
    /// `bedrock_mantle` sub-config.
    #[cfg(feature = "bedrock")]
    pub mantle: Option<MantleAuth>,
}

impl std::fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-rolled Debug elides the auth source so a derived
        // `{:?}` on the config (or any struct embedding it) can never
        // print the bearer/JWT. `StaticToken`'s own Debug already
        // redacts; this is the second line of defense mirroring
        // `AnthropicApiConfig`.
        let mut d = f.debug_struct("OpenAiResponsesConfig");
        d.field("id", &self.id)
            .field("auth", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .field("base_url", &self.base_url)
            .field("auth_kind", &self.auth_kind)
            .field("header_extras_len", &self.header_extras.len())
            .field("user_agent", &self.user_agent)
            // Presence only: the session_id ties requests to one logical
            // session; treat it as sensitive so its value never enters logs.
            .field("session_id", &self.session_id.is_some())
            // Presence only: the installation-id is a stable machine
            // fingerprint; keep its value out of logs.
            .field("installation_id", &self.installation_id.is_some());
        // Mantle lane presence + its own redacting Debug (region + auth
        // shape only, never credential material).
        #[cfg(feature = "bedrock")]
        d.field("mantle", &self.mantle);
        d.finish()
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
            installation_id: None,
            #[cfg(feature = "bedrock")]
            mantle: None,
        }
    }
}

/// OpenAI Responses egress provider.
pub struct OpenAiResponsesProvider {
    pub(super) cfg: OpenAiResponsesConfig,
    pub(super) client: Client,
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
    /// Build a provider from its configuration.
    pub fn new(cfg: OpenAiResponsesConfig) -> Self {
        // Always pass an explicit UA string so the client-level default
        // header carries the codex-derived value. Operator-supplied
        // `cfg.user_agent` wins; otherwise fall back to the codex CLI
        // UA shape from auth::default_user_agent.
        let ua = cfg
            .user_agent
            .clone()
            .unwrap_or_else(auth::default_user_agent);

        // Mantle lane: a signed POST must never be auto-followed across a
        // 3xx -- replaying the SigV4 signature against a redirected host
        // always fails, and a redirect on this lane is an upstream fault to
        // surface, not to chase. The Cloudflare cookie jar is a chatgpt.com
        // concern with no meaning here, so this lane carries neither the jar
        // nor a persistence path. The first-party lanes keep the
        // cookie-backed (redirect-following) client below.
        #[cfg(feature = "bedrock")]
        if cfg.mantle.is_some() {
            let client = crate::http_client::build_no_redirect(Some(&ua))
                .expect("reqwest no-redirect client build failed (TLS init?); fatal at startup");
            return Self {
                cfg,
                client,
                window_id: uuid::Uuid::new_v4().to_string(),
                cookie_jar: None,
                cookie_path: None,
            };
        }

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
    pub(super) fn responses_url(&self) -> String {
        format!("{}/responses", self.cfg.base_url.trim_end_matches('/'))
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

    pub(super) fn build_headers(
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
            for (k, v) in routectl_core::identity::codex::default_identity_headers() {
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
            // Persistent per-installation id, resolved once by the factory.
            // Inserted in the defaults phase (before the header_extras loop)
            // so an operator `header_extras` entry for
            // `x-codex-installation-id` still wins, and omitted when the
            // factory could not resolve one. Value never logged.
            if let Some(iid) = &self.cfg.installation_id {
                crate::http_client::insert_header(
                    &mut header_map,
                    &self.cfg.id,
                    "x-codex-installation-id",
                    iid,
                );
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
