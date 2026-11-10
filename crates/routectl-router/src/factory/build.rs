//! Per-kind provider construction from config rows.

#[cfg(feature = "openai-responses")]
use super::validate::validate_openai_responses_account_id;
use super::validate::{
    ModelProviderTarget, resolve_model_provider, validate_base_url_scheme,
    validate_managed_oauth_credentials,
};
use super::warnings::warn_context_management_needs_preserve;
#[cfg(feature = "bedrock")]
use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
use crate::config::{Config, CredentialSource, PoolEntry, ProviderEntry};
use crate::pool_build::{PoolOutcome, PoolPlaceholder, PoolReport};
use crate::resolved::ResolvedModel;
use crate::runtime_state::UnusableCause;
use crate::seat_pool::SeatTarget;
#[cfg(feature = "gemini")]
use routectl_auth::OAuthStoreProjectCache;
use routectl_auth::{CredentialProbeFailure, SecretRef, SecretScheme, SecretStore};
#[cfg(feature = "gemini")]
use routectl_core::CloudProjectCache;
use routectl_core::{Provider, Result};
#[cfg(feature = "bedrock")]
use routectl_providers::anthropic_api::MantleAuth;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
#[cfg(feature = "bedrock")]
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};
#[cfg(feature = "gemini")]
use routectl_providers::gemini::{GeminiAuthMode, GeminiConfig, GeminiProvider};
use routectl_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::{
    AuthKind as OpenaiResponsesAuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Convenience wrapper that builds a provider with `BuildOptions::default()`.
pub async fn build_provider(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
) -> Result<Arc<dyn Provider>> {
    build_provider_with_options(name, entry, secrets, BuildOptions::default()).await
}

/// Server-wide options that influence per-provider construction.
/// Defaults are equivalent to the legacy `build_provider` behavior.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BuildOptions {
    /// When `true`, providers reject requests carrying canonical-only
    /// fields they cannot represent on the wire (e.g. an OpenAI-compat
    /// egress receiving an Anthropic `cache_control` block). Default
    /// `false` -- warn-and-drop. Set from `[server] strict_translation`.
    pub strict_translation: bool,
    /// Anthropic-cloak tool-array canonicalization switch, sourced from
    /// `[cache] normalize_tools`. Threaded into each anthropic provider's
    /// `CloakConfig` at build time (the same global-to-provider channel
    /// `strict_translation` uses). Default `true`.
    pub normalize_tools: bool,
    /// Whether a ChatGPT-OAuth openai-responses build may create (or
    /// replace a corrupt) `<config-dir>/installation_id`. `false` adopts an
    /// existing valid id and otherwise builds without one, writing nothing:
    /// the posture of a read-only command. Default `true`.
    pub mint_installation_id: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            strict_translation: false,
            normalize_tools: true,
            mint_installation_id: true,
        }
    }
}

impl BuildOptions {
    /// Build options with every field at its default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the strict-translation posture.
    pub const fn with_strict_translation(mut self, strict: bool) -> Self {
        self.strict_translation = strict;
        self
    }

    /// Set the anthropic-cloak tool-array canonicalization switch.
    pub const fn with_normalize_tools(mut self, normalize: bool) -> Self {
        self.normalize_tools = normalize;
        self
    }

    /// Set whether the build may mint a missing installation id.
    pub const fn with_mint_installation_id(mut self, mint: bool) -> Self {
        self.mint_installation_id = mint;
        self
    }
}

/// Build one provider from its config entry, resolving secrets through
/// `secrets` and applying `opts`.
#[tracing::instrument(skip_all, fields(provider = %routectl_core::sanitize_for_log(name)))]
pub async fn build_provider_with_options(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<Arc<dyn Provider>> {
    // Defensive: a caller building a single provider directly (e.g.
    // `provider probe`) bypasses `build_resolved_models`, which is where the
    // codex identity is normally installed. Resolve it from THIS entry's
    // own codex_version so a probed responses provider still egresses the
    // configured fingerprint. No-op on the build_resolved_models path (the
    // identity is already installed there) since the OnceLock is set-once.
    //
    // `provider probe` / `doctor` load config through the unvalidated path,
    // which never runs `validate_codex_version`, so an illegal value can
    // reach here. Re-run the syntax check and reject-to-default (never
    // sanitize) rather than let a header-illegal byte panic the derived UA
    // downstream.
    #[cfg(feature = "openai-responses")]
    if let Some(version) = entry.codex_version() {
        use routectl_core::identity::codex::{CodexIdentity, PINNED_CODEX_VERSION, set_resolved};
        match super::validate::validate_codex_version_syntax(name, version) {
            Ok(()) => {
                set_resolved(CodexIdentity::new(version));
            }
            Err(reason) => {
                tracing::warn!(
                    provider = %routectl_core::sanitize_for_log(name),
                    reason = %reason,
                    codex_version = %PINNED_CODEX_VERSION,
                    "invalid codex_version; falling back to the pinned codex identity",
                );
                set_resolved(CodexIdentity::new(PINNED_CODEX_VERSION));
            }
        }
    }

    #[cfg(feature = "bedrock")]
    {
        build_provider_inner(name, entry, secrets, opts, None, None).await
    }
    #[cfg(not(feature = "bedrock"))]
    {
        build_provider_inner(name, entry, secrets, opts).await
    }
}

/// Per-model overrides applied to a Bedrock provider build. These
/// fields used to live on `[providers.X]` but moved to `[models.X]`
/// in v0.6.0; the factory threads them through here so each Bedrock
/// model gets a `BedrockConfig` with the right values.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Default)]
pub(super) struct BedrockModelOverrides {
    pub model_id: String,
    pub additional_model_request_fields: Option<serde_json::Value>,
}

/// Pre-resolved Bedrock credentials. Cached per `[providers.X]` entry
/// in `build_resolved_models` so multiple models on the same Bedrock
/// provider share one SSO probe / one `aws-config` chain construction.
/// Without this, building 5 Bedrock models on the same provider would
/// hit the credential chain 5 times (5x cold-start latency on SSO).
#[cfg(feature = "bedrock")]
#[derive(Clone)]
pub(super) struct CachedBedrockAuth {
    pub creds: routectl_providers::bedrock::BedrockCreds,
    pub resolved: routectl_providers::bedrock::auth::ResolvedCreds,
}

/// Variant that lets the caller override Bedrock model-specific fields
/// on `BedrockConfig`. v0.6.0 moves the upstream model id, adaptive
/// thinking flag, and additional request fields from
/// `[providers.X]` to `[models.X]`, so each Bedrock-targeting model
/// entry needs its own provider instance with those values. Other
/// provider kinds ignore the override; one cached `Arc<dyn Provider>`
/// per `[providers.X]` is shared across all `[models]` referencing it.
///
/// `cached_auth` lets the caller skip the per-model SSO probe by
/// reusing creds resolved once at the parent provider level. When
/// `None`, the factory resolves creds from the `BedrockCredsConfig`
/// in `entry` (the legacy path; one resolve per call).
#[cfg(feature = "bedrock")]
pub(super) async fn build_provider_with_bedrock_model_override(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    bedrock_overrides: Option<BedrockModelOverrides>,
    cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    build_provider_inner(name, entry, secrets, opts, bedrock_overrides, cached_auth).await
}

#[tracing::instrument(skip_all, fields(provider = %routectl_core::sanitize_for_log(name)))]
async fn build_provider_inner(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    #[cfg(feature = "bedrock")] bedrock_overrides: Option<BedrockModelOverrides>,
    #[cfg(feature = "bedrock")] cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    validate_managed_oauth_credentials(name, entry)?;
    match entry {
        ProviderEntry::OpenaiCompat {
            base_url,
            api_key_ref,
            header_extras,
            payload_extras: _,
            user_agent,
            cache_capability: _,
            auto_emit_top_level_breakpoint: _,
            auto_emit_per_block_breakpoints: _,
            reduction_enabled: _,
            #[cfg(feature = "bedrock")]
            bedrock_mantle,
            runtime: _,
        } => {
            // Bedrock mantle lane. Its presence flips this provider onto
            // region-derived, SigV4/bearer-signed egress under the
            // `bedrock-mantle` scope. Config validation guarantees an empty
            // api_key_ref and an empty base_url, so the region is the single
            // source of truth for the endpoint (never the operator base_url)
            // and no first-party bearer is presented. Credentials resolve
            // here, fail-fast probing Profile/DefaultChain exactly as the
            // Bedrock provider does.
            #[cfg(feature = "bedrock")]
            if let Some(m) = bedrock_mantle {
                debug_assert!(
                    base_url.trim().is_empty(),
                    "mantle lane requires an empty base_url; validation must reject a manual base_url"
                );
                let mantle_base = routectl_providers::mantle::mantle_openai_base(&m.region)?;
                let bedrock_creds = resolve_bedrock_creds(&*secrets, &m.creds).await?;
                let resolved =
                    routectl_providers::bedrock::auth::resolve(&bedrock_creds, &m.region).await?;
                let cfg = OpenAiCompatConfig {
                    id: format!("openai-compat:{name}"),
                    base_url: mantle_base,
                    // Empty on the lane: the mantle credential rides the
                    // signed request and the first-party `Authorization:
                    // Bearer` insert is skipped. Validation already rejects
                    // a non-empty api_key_ref on the lane.
                    auth: Arc::new(routectl_core::StaticToken::new(String::new())),
                    header_extras: header_extras
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    payload_extras: None,
                    reasoning_dialect: Default::default(),
                    history_reasoning: Default::default(),
                    user_agent: user_agent.clone(),
                    strict_translation: opts.strict_translation,
                    mantle: Some(MantleAuth {
                        region: m.region.clone(),
                        creds: resolved,
                    }),
                };
                return Ok(Arc::new(OpenAiCompatProvider::new(cfg)));
            }
            validate_base_url_scheme(name, base_url)?;
            // `oauth://<provider>` yields a refreshing `ManagedToken` read
            // per request; env / file / literal resolve once into a
            // `StaticToken`.
            let auth = resolve_token_source(&secrets, api_key_ref).await?;
            // v0.6.0: reasoning_dialect + history_reasoning moved off
            // [providers.X] to [models.X]; the egress reads them from
            // `req.routectl_internal` at request time. The config-side
            // defaults left here are pure fallback for library
            // consumers constructing an `OpenAiCompatConfig` directly
            // (no router).
            let cfg = OpenAiCompatConfig {
                id: format!("openai-compat:{name}"),
                base_url: base_url.clone(),
                auth,
                header_extras: header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                payload_extras: None,
                reasoning_dialect: Default::default(),
                history_reasoning: Default::default(),
                user_agent: user_agent.clone(),
                strict_translation: opts.strict_translation,
                #[cfg(feature = "bedrock")]
                mantle: None,
            };
            Ok(Arc::new(OpenAiCompatProvider::new(cfg)))
        }
        ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            anthropic_version,
            auth_kind,
            credential_source,
            header_extras,
            payload_extras: _,
            user_agent,
            forward_client_headers,
            context_management,
            max_thinking_entry_bytes,
            cache_capability: _,
            auto_emit_top_level_breakpoint: _,
            auto_emit_per_block_breakpoints: _,
            reduction_enabled: _,
            cloak,
            #[cfg(feature = "bedrock")]
            bedrock_mantle,
            runtime: _,
        } => {
            // Bedrock mantle lane. Its presence flips this provider onto
            // region-derived, SigV4/bearer-signed egress under the
            // `bedrock-mantle` scope. Config validation guarantees
            // auth_kind = api-key, an empty api_key_ref,
            // credential_source = own, and base_url at its default -- so
            // the region is the single source of truth for the endpoint
            // (never the operator base_url) and no first-party token is
            // presented. Credentials resolve here, fail-fast probing
            // Profile/DefaultChain exactly as the Bedrock provider does.
            #[cfg(feature = "bedrock")]
            if let Some(m) = bedrock_mantle {
                debug_assert_eq!(
                    base_url,
                    &crate::config::default_anthropic_base(),
                    "mantle lane requires base_url at its default; validation must reject a manual base_url"
                );
                let mantle_base = routectl_providers::mantle::mantle_anthropic_base(&m.region)?;
                let bedrock_creds = resolve_bedrock_creds(&*secrets, &m.creds).await?;
                let resolved =
                    routectl_providers::bedrock::auth::resolve(&bedrock_creds, &m.region).await?;
                let cfg = AnthropicApiConfig {
                    id: format!("anthropic-api:{name}"),
                    auth: Arc::new(routectl_core::StaticToken::new(String::new())),
                    base_url: mantle_base,
                    anthropic_version: anthropic_version.clone(),
                    // Force api-key on the lane rather than copying the
                    // config value: the mantle credential rides the signed
                    // request, and OauthBearer would re-engage the
                    // first-party `Authorization: Bearer` / Claude-Code
                    // header plumbing this lane must bypass. Validation
                    // already rejects a non-api-key mantle entry; this is
                    // the factory-side belt-and-braces.
                    auth_kind: routectl_providers::anthropic_api::AuthKind::ApiKey,
                    header_extras: header_extras
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    user_agent: user_agent.clone(),
                    forward_client_headers: forward_client_headers.clone(),
                    context_management: *context_management,
                    max_thinking_entry_bytes: resolve_max_thinking_entry_bytes(
                        name,
                        *max_thinking_entry_bytes,
                    ),
                    session_id: None,
                    cloak: cloak_with_normalize(cloak, opts.normalize_tools),
                    use_forwarded_bearer: false,
                    mantle: Some(MantleAuth {
                        region: m.region.clone(),
                        creds: resolved,
                    }),
                };
                return Ok(Arc::new(AnthropicApiProvider::new(cfg)));
            }
            validate_base_url_scheme(name, base_url)?;
            let is_forwarded = *credential_source == CredentialSource::Forwarded;
            // OAuth-aware: for `oauth://<provider>` refs the provider
            // gets a `ManagedToken` that re-enters `SecretStore::get`
            // per request, so token rotation in credentials.json is
            // picked up live without restart. For env / file / literal
            // the value is resolved once and wrapped in `StaticToken`
            // (semantically equivalent to the pre-v0.7 `api_key:
            // String` field).
            //
            // `credential_source = "forwarded"` is the exception:
            // `validate_provider_credential_sources` guarantees
            // `api_key_ref` is empty for a forwarded entry (there is no
            // configured secret to resolve -- the provider authenticates
            // with the client's captured bearer instead), so calling
            // `resolve_token_source` here would always fail with an
            // "unrecognized secret URI scheme" error. Wire a sentinel
            // `StaticToken` instead: it is unreachable by construction --
            // `resolve_effective_token` / `build_headers` never call
            // `cfg.auth.token()` on the forwarded leg (gated by
            // `should_use_forwarded_bearer`, which reads
            // `req.routectl_internal.forwarded_bearer` instead), and the
            // router's `missing_forwarded_bearer_error` guard refuses any
            // request with no captured bearer before this provider is
            // ever dispatched to.
            let auth: Arc<dyn routectl_core::TokenSource> = if is_forwarded {
                Arc::new(routectl_core::StaticToken::new(String::new()))
            } else {
                resolve_token_source(&secrets, api_key_ref).await?
            };
            // Resolve the stable per-credential Claude Code session id
            // for the OauthBearer surface only. `api_key_ref` already
            // carries the seat label (`build_seat_targets` rebuilds each
            // labeled seat with its own `oauth://anthropic#label` ref), so
            // `peek_session_id` resolves THIS seat's session_id with no
            // extra fallback. ApiKey providers (and a non-oauth ref) get
            // None. The ref already parsed cleanly inside
            // `resolve_token_source` above, so a parse error here is
            // unreachable; treat it as "no session id" rather than fail
            // the build. A forwarded entry is excluded up front even if
            // `auth_kind` happens to be `OauthBearer` -- its `api_key_ref`
            // is always empty, so there is no seat to resolve a session
            // id for.
            let session_id = if !is_forwarded
                && *auth_kind == routectl_providers::anthropic_api::AuthKind::OauthBearer
            {
                match SecretRef::parse(api_key_ref) {
                    Ok(sr) => secrets.peek_session_id(&sr).await,
                    Err(_) => None,
                }
            } else {
                None
            };
            let cfg = AnthropicApiConfig {
                id: format!("anthropic-api:{name}"),
                auth,
                base_url: base_url.clone(),
                anthropic_version: anthropic_version.clone(),
                auth_kind: *auth_kind,
                header_extras: header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                user_agent: user_agent.clone(),
                forward_client_headers: forward_client_headers.clone(),
                context_management: *context_management,
                max_thinking_entry_bytes: resolve_max_thinking_entry_bytes(
                    name,
                    *max_thinking_entry_bytes,
                ),
                session_id,
                cloak: cloak_with_normalize(cloak, opts.normalize_tools),
                use_forwarded_bearer: is_forwarded,

                #[cfg(feature = "bedrock")]
                mantle: None,
            };
            Ok(Arc::new(AnthropicApiProvider::new(cfg)))
        }
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses {
            api_key_ref,
            account_id_ref,
            base_url,
            auth_kind,
            header_extras,
            payload_extras: _,
            user_agent,
            // Resolved into the process-global codex identity by the
            // router builder before any provider is constructed, not
            // per-provider here.
            codex_version: _,
            cache_capability: _,
            auto_emit_top_level_breakpoint: _,
            auto_emit_per_block_breakpoints: _,
            reduction_enabled: _,
            #[cfg(feature = "bedrock")]
            bedrock_mantle,
            runtime: _,
        } => {
            // Bedrock mantle lane. Its presence flips this provider onto
            // region-derived, SigV4/bearer-signed egress under the
            // `bedrock-mantle` scope. Config validation guarantees an empty
            // api_key_ref, no account_id_ref, and an unset base_url, so the
            // region is the single source of truth for the endpoint and no
            // first-party bearer is presented. Credentials resolve here,
            // fail-fast probing Profile/DefaultChain exactly as the Bedrock
            // provider does. The runtime marker is set here so the extras
            // guard and the auth dispatch see `BedrockMantle` even when the
            // operator omitted the redundant `auth_kind`.
            #[cfg(feature = "bedrock")]
            if let Some(m) = bedrock_mantle {
                debug_assert!(
                    base_url.as_deref().is_none_or(|s| s.trim().is_empty()),
                    "mantle lane requires an unset base_url; validation must reject a manual base_url"
                );
                let mantle_base = routectl_providers::mantle::mantle_openai_base(&m.region)?;
                let bedrock_creds = resolve_bedrock_creds(&*secrets, &m.creds).await?;
                let resolved =
                    routectl_providers::bedrock::auth::resolve(&bedrock_creds, &m.region).await?;
                let mut cfg = OpenAiResponsesConfig::new_with_auth(
                    format!("openai-responses:{name}"),
                    Arc::new(routectl_core::StaticToken::new(String::new())),
                );
                cfg.base_url = mantle_base;
                cfg.auth_kind = OpenaiResponsesAuthKind::BedrockMantle;
                cfg.header_extras = header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                cfg.user_agent = user_agent.clone();
                cfg.mantle = Some(MantleAuth {
                    region: m.region.clone(),
                    creds: resolved,
                });
                return Ok(Arc::new(OpenAiResponsesProvider::new(cfg)));
            }
            // Close the legacy `auth_kind = "bedrock-mantle"`-alone surface
            // for every caller. The mantle branch above returns whenever a
            // `bedrock_mantle` block selected the lane, so reaching here with
            // the marker means no block was set: the bearer-only surface it
            // would otherwise build is refused. This guard is deliberately
            // NOT `bedrock`-gated -- `validate_provider_openai_mantle` is, and
            // callers that skip validation entirely (e.g. `provider probe`
            // over an unvalidated config) must still fail cleanly rather than
            // hit the `default_responses_base` fallback below.
            if *auth_kind == OpenaiResponsesAuthKind::BedrockMantle {
                return Err(routectl_core::Error::Config(format!(
                    "provider `{name}`: auth_kind = \"bedrock-mantle\" but no bedrock_mantle \
                     block is set -- the legacy bearer-only surface is closed; set \
                     [providers.{name}.bedrock_mantle] with region and creds to select the \
                     mantle lane"
                )));
            }
            let bearer_is_oauth =
                matches!(SecretRef::parse(api_key_ref), Ok(SecretRef::OAuth { .. }));
            validate_openai_responses_account_id(
                name,
                *auth_kind,
                bearer_is_oauth,
                account_id_ref,
            )?;
            // OAuth-aware bearer resolution, mirroring the anthropic-api
            // arm: `oauth://<provider>` yields a refreshing
            // `ManagedToken`; env / file / literal yield a one-shot
            // `StaticToken`. The provider resolves `auth.token()` per
            // request, so OAuth rotation is picked up without restart.
            let auth = resolve_token_source(&secrets, api_key_ref).await?;
            let account_id =
                resolve_responses_account_id(&secrets, api_key_ref, account_id_ref, name).await?;
            // Resolve the stable per-credential codex session id for the
            // ChatgptOauth surface only. `api_key_ref` already carries the
            // seat label, so `peek_session_id` resolves THIS seat's value
            // with no extra fallback. ApiKey / BedrockMantle (and a
            // non-oauth ref) get None. The ref already parsed cleanly
            // inside `resolve_token_source` above, so a parse error here
            // is unreachable; treat it as "no session id" rather than fail
            // the build.
            let session_id = if *auth_kind == OpenaiResponsesAuthKind::ChatgptOauth {
                match SecretRef::parse(api_key_ref) {
                    Ok(sr) => secrets.peek_session_id(&sr).await,
                    Err(_) => None,
                }
            } else {
                None
            };
            // Resolve the persistent per-installation id for the
            // ChatgptOauth surface only (adopt an existing file, mint one
            // otherwise when the options allow it; a read/write failure
            // yields None + a WARN, never a build failure). ApiKey /
            // BedrockMantle get None.
            let installation_id = if *auth_kind == OpenaiResponsesAuthKind::ChatgptOauth {
                super::installation_id::resolve_installation_id(opts.mint_installation_id)
            } else {
                None
            };
            let resolved_base_url = base_url
                .clone()
                .unwrap_or_else(|| crate::config::default_responses_base(*auth_kind));
            validate_base_url_scheme(name, &resolved_base_url)?;
            let mut cfg =
                OpenAiResponsesConfig::new_with_auth(format!("openai-responses:{name}"), auth);
            cfg.account_id = account_id;
            cfg.base_url = resolved_base_url;
            cfg.auth_kind = *auth_kind;
            cfg.header_extras = header_extras
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            cfg.user_agent = user_agent.clone();
            cfg.session_id = session_id;
            cfg.installation_id = installation_id;
            Ok(Arc::new(OpenAiResponsesProvider::new(cfg)))
        }
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            header_extras,
            payload_extras: _,
            anthropic_beta,
            cache_capability: _,
            auto_emit_top_level_breakpoint: _,
            auto_emit_per_block_breakpoints: _,
            reduction_enabled: _,
            runtime: _,
        } => {
            // Reuse already-resolved creds when the caller provided
            // them (per-provider cache in `build_resolved_models`).
            // Otherwise resolve fresh -- the legacy single-provider
            // path still works.
            let (bedrock_creds, resolved) = if let Some(c) = cached_auth {
                (c.creds, c.resolved)
            } else {
                let bedrock_creds = resolve_bedrock_creds(&*secrets, creds).await?;
                let resolved =
                    routectl_providers::bedrock::auth::resolve(&bedrock_creds, region).await?;
                (bedrock_creds, resolved)
            };
            // v0.6.0: model_id and additional_model_request_fields both
            // come from the model
            // entry override. The factory pumps them in via
            // `BedrockModelOverrides` from `build_resolved_models`.
            let overrides = bedrock_overrides.unwrap_or_default();
            let cfg = BedrockConfig {
                id: format!("bedrock:{name}"),
                region: region.clone(),
                model_id: overrides.model_id,
                api_shape: map_bedrock_api_shape(*api_shape),
                creds: bedrock_creds,
                user_agent: user_agent.clone(),
                header_extras: header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                anthropic_beta: anthropic_beta.clone(),
                additional_model_request_fields: overrides.additional_model_request_fields,
            };
            Ok(Arc::new(BedrockProvider::new(cfg, resolved)?))
        }
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini {
            api_key_ref,
            base_url,
            header_extras,
            payload_extras: _,
            user_agent,
            auth_mode,
            cloud_project_id,
            cache_capability: _,
            auto_emit_top_level_breakpoint: _,
            auto_emit_per_block_breakpoints: _,
            reduction_enabled: _,
            runtime: _,
        } => {
            let extras: Vec<(String, String)> = header_extras
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            match auth_mode {
                GeminiAuthMode::ApiKey => {
                    validate_base_url_scheme(name, base_url)?;
                    let auth = resolve_token_source(&secrets, api_key_ref).await?;
                    let mut cfg = GeminiConfig::new_with_auth(format!("gemini:{name}"), auth);
                    cfg.base_url = base_url.clone();
                    cfg.header_extras = extras;
                    cfg.user_agent = user_agent.clone();
                    Ok(Arc::new(GeminiProvider::new(cfg)))
                }
                GeminiAuthMode::CloudCode => {
                    // The Cloud Code surface authenticates with a live OAuth
                    // bearer, so validate the base-URL scheme before any token
                    // can be attached to it (the api-key arm validates too; it
                    // matters more here because the credential is a bearer).
                    validate_base_url_scheme(name, base_url)?;
                    let secret_ref = SecretRef::parse(api_key_ref).map_err(|e| {
                        routectl_core::Error::Config(format!(
                            "gemini provider `{name}`: auth_mode = \"cloud-code\" but api_key_ref (scheme `{}`) is not a valid secret URI: {e}",
                            scheme_of(api_key_ref)
                        ))
                    })?;
                    if !matches!(secret_ref, SecretRef::OAuth { .. }) {
                        return Err(routectl_core::Error::Config(format!(
                            "gemini provider `{name}`: auth_mode = \"cloud-code\" requires an oauth:// api_key_ref (got scheme `{}`); run `routectl login antigravity` and reference oauth://antigravity",
                            scheme_of(api_key_ref)
                        )));
                    }
                    let auth = resolve_token_source(&secrets, api_key_ref).await?;
                    let project_cache: Arc<dyn CloudProjectCache> =
                        Arc::new(OAuthStoreProjectCache::new(secrets.clone(), secret_ref));
                    let mut cfg =
                        GeminiConfig::new_cloud_code(format!("gemini:{name}"), auth, project_cache);
                    if *base_url != crate::config::default_gemini_base() {
                        cfg.base_url = base_url.clone();
                    }
                    if let Some(ua) = user_agent {
                        cfg.user_agent = Some(ua.clone());
                    }
                    // An empty string is an operator leaving the knob blank,
                    // not a project id: it must not shadow discovery.
                    cfg.cloud_project_id = cloud_project_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                    cfg.header_extras = extras;
                    Ok(Arc::new(GeminiProvider::new(cfg)))
                }
            }
        }
    }
}

#[cfg(feature = "bedrock")]
async fn resolve_bedrock_creds(
    secrets: &dyn SecretStore,
    creds: &BedrockCredsConfig,
) -> Result<BedrockCreds> {
    Ok(match creds {
        BedrockCredsConfig::BearerKey { key_ref } => {
            let key = resolve(secrets, key_ref).await?;
            BedrockCreds::BearerKey { key }
        }
        BedrockCredsConfig::Static {
            access_key_ref,
            secret_key_ref,
            session_token_ref,
        } => {
            let access_key = resolve(secrets, access_key_ref).await?;
            let secret_key = resolve(secrets, secret_key_ref).await?;
            let session_token = match session_token_ref {
                Some(t) => Some(resolve(secrets, t).await?),
                None => None,
            };
            BedrockCreds::Static {
                access_key,
                secret_key,
                session_token,
            }
        }
        BedrockCredsConfig::Profile { name } => BedrockCreds::Profile { name: name.clone() },
        BedrockCredsConfig::DefaultChain => BedrockCreds::DefaultChain,
    })
}

/// Resolve a Bedrock provider's creds + AWS auth handle once. Used by
/// `build_resolved_models` to prime the per-provider auth cache before
/// dispatching N model builds against it. Bypasses credential
/// resolution for non-Bedrock entries (returns an error so the caller
/// only invokes this on Bedrock-confirmed entries).
#[cfg(feature = "bedrock")]
async fn resolve_bedrock_auth_for_entry(
    entry: &ProviderEntry,
    secrets: &dyn SecretStore,
) -> Result<CachedBedrockAuth> {
    let ProviderEntry::Bedrock { creds, region, .. } = entry else {
        return Err(routectl_core::Error::Config(
            "resolve_bedrock_auth_for_entry called on non-Bedrock provider entry".into(),
        ));
    };
    let bedrock_creds = resolve_bedrock_creds(secrets, creds).await?;
    let resolved = routectl_providers::bedrock::auth::resolve(&bedrock_creds, region).await?;
    Ok(CachedBedrockAuth {
        creds: bedrock_creds,
        resolved,
    })
}

#[cfg(feature = "bedrock")]
const fn map_bedrock_api_shape(s: BedrockApiShapeConfig) -> BedrockApiShape {
    match s {
        BedrockApiShapeConfig::Invoke => BedrockApiShape::Invoke,
        BedrockApiShapeConfig::Converse => BedrockApiShape::Converse,
    }
}

/// Bounds for `[providers.X].max_thinking_entry_bytes` (anthropic-api).
const MIN_THINKING_ENTRY_BYTES: u32 = 1024;

const MAX_THINKING_ENTRY_BYTES_CEILING: u32 = 4 * 1024 * 1024;

/// Test-only re-export so other modules' tests can drive the resolver
/// without making the helper itself `pub`.
#[cfg(test)]
pub fn resolve_max_thinking_entry_bytes_for_test(
    provider_name: &str,
    configured: Option<u32>,
) -> usize {
    resolve_max_thinking_entry_bytes(provider_name, configured)
}

/// Resolve the operator-supplied `max_thinking_entry_bytes` knob into
/// the runtime value carried on `AnthropicApiConfig`. None or 0 falls
/// through to the hardcoded default (1 MiB). Out-of-range values are
/// clamped to the documented bounds (1 KiB to 4 MiB) with a startup
/// WARN so the operator sees the override they actually got.
fn resolve_max_thinking_entry_bytes(provider_name: &str, configured: Option<u32>) -> usize {
    // Soft-fail clamp + WARN (rather than the file's usual hard-fail
    // Err(Error::Config(...))) is intentional: routectl targets local
    // single-user installs where a typo on a memory cap should not
    // prevent the daemon from coming up. The default is generous enough
    // that a clamped value is still functional. If the local-only
    // assumption ever changes, switch this to hard-fail to match
    // siblings.
    use routectl_providers::anthropic_api::AnthropicApiConfig;
    let default = AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES;
    let Some(v) = configured else {
        return default;
    };
    // Operator-written `[providers.X]` table key, rendered by all three
    // arms below (field plus message body) on an otherwise healthy build.
    let provider_safe = routectl_core::sanitize_for_log(provider_name);
    if v == 0 {
        // Operator wrote `0`; treat as "unset" rather than letting a
        // zero cap silently disable the cache.
        tracing::warn!(
            provider = %provider_safe,
            "[providers.{provider_safe}] max_thinking_entry_bytes = 0 is not a valid cap (would disable the cache); falling back to the default ({default})"
        );
        return default;
    }
    if v < MIN_THINKING_ENTRY_BYTES {
        tracing::warn!(
            provider = %provider_safe,
            configured = v,
            min = MIN_THINKING_ENTRY_BYTES,
            "max_thinking_entry_bytes below minimum; clamping up"
        );
        return MIN_THINKING_ENTRY_BYTES as usize;
    }
    if v > MAX_THINKING_ENTRY_BYTES_CEILING {
        tracing::warn!(
            provider = %provider_safe,
            configured = v,
            max = MAX_THINKING_ENTRY_BYTES_CEILING,
            "max_thinking_entry_bytes above ceiling; clamping down"
        );
        return MAX_THINKING_ENTRY_BYTES_CEILING as usize;
    }
    v as usize
}

#[cfg(any(feature = "bedrock", feature = "openai-responses"))]
async fn resolve(secrets: &dyn SecretStore, uri: &str) -> Result<String> {
    tracing::debug!(secret_scheme = scheme_of(uri), "resolving secret ref");
    let secret_ref = SecretRef::parse(uri)?;
    secrets.get(&secret_ref).await
}

/// Pick the primary `api_key_ref` URI off a provider entry. Used by
/// `build_resolved_models` to thread the originating `SecretRef` onto
/// the resolved model so the 401 self-heal path can dispatch back
/// through the originating store. Bedrock entries return `None` --
/// their creds shape is multi-field and the caller doesn't need a
/// single canonical SecretRef for the self-heal hook today.
fn primary_api_key_uri(entry: &ProviderEntry) -> Option<&str> {
    entry.api_key_ref()
}

/// Compile one `[pools.<name>]` block into the dispatch seat set every
/// model naming that pool shares.
///
/// A SIBLING of the non-pool path rather than a modification of it: the
/// non-pool path builds exactly one provider from the model's own entry,
/// while this walks the pool's declared members and builds one provider per
/// member from that member's OWN `api_key_ref`. No member is ever enumerated
/// through the credential store, so the bare-ref-means-default-seat rule
/// arrives here by construction -- a member whose ref is a bare
/// `oauth://<provider>` gets that provider's default seat and nothing else.
///
/// A member whose credential probe fails is kept as a PLACEHOLDER seat (no
/// provider) and reported with its unusable cause, so its account can be
/// marked and a pool of placeholders alone still builds. A member that cannot
/// be built for any other reason REFUSES the pool: the returned message names
/// the pool, every refused member and the rule each broke, and the caller
/// turns it into a config error. Config validation refuses the unknown,
/// missing-ref, malformed-ref and containment cases first; they are repeated
/// here for a directly-constructed `Config`. A member whose credential reads
/// but whose provider then fails to build is visible only here.
///
/// Members are walked in declaration order, and the resulting seat order is
/// the operator's declared order -- the strategy layer
/// (`seat_order_for_request`) rotates over it per request.
async fn compile_pool(
    pool_name: &str,
    pool: &PoolEntry,
    config: &Config,
    secrets: &Arc<dyn SecretStore>,
    opts: &BuildOptions,
) -> std::result::Result<PoolOutcome, String> {
    let mut seats: Vec<SeatTarget> = Vec::with_capacity(pool.members.len());
    let mut placeholders: Vec<PoolPlaceholder> = Vec::new();
    let mut refusals: Vec<(&str, MemberRefusal)> = Vec::new();

    for member in &pool.members {
        let Some(entry) = config.providers.get(member) else {
            refusals.push((member, MemberRefusal::UnknownMember));
            continue;
        };
        let provider_kind = entry.kind_str();
        let secret_ref = match member_secret_ref(entry) {
            Ok(sr) => sr,
            Err(refusal) => {
                refusals.push((member, refusal));
                continue;
            }
        };
        // The credential probe below reads the store, so the managed
        // credential containment rules must already hold.
        if validate_managed_oauth_credentials(member, entry).is_err() {
            refusals.push((member, MemberRefusal::OutsideContainment));
            continue;
        }
        // Probe the credential BEFORE building. An `oauth://` provider builds
        // with a LAZY token source (so rotation is picked up per request
        // without a restart), which means a logged-out member would otherwise
        // build successfully and only fail at first traffic. A failed probe
        // keeps the member as a placeholder seat whose account is marked
        // unusable at install; a logged-out Codex member could not even
        // build (it has no account id). One read per member per build,
        // collapsed by the store's own cache + single-flight.
        if let Err(failure) = secrets.probe_credential(&secret_ref).await {
            let cause = unusable_cause_for_probe_failure(failure);
            let seat = SeatTarget::placeholder(
                member.clone(),
                Some(secret_ref),
                entry.forwarded_base_url().is_some(),
            );
            placeholders.push(PoolPlaceholder {
                member: member.clone(),
                provider_kind,
                cause,
                account: seat.account().map(str::to_string),
            });
            seats.push(seat);
            // The store's error text never reaches here: the probe returns
            // only a variant. The cause token is the whole diagnostic.
            tracing::debug!(
                pool = %routectl_core::sanitize_for_log(pool_name),
                member = %routectl_core::sanitize_for_log(member),
                provider_kind,
                cause = cause.as_str(),
                "pool member credential unreadable; kept as an unusable seat",
            );
            continue;
        }
        match build_provider_with_options(member, entry, secrets.clone(), opts.clone()).await {
            Ok(provider) => seats.push(SeatTarget::new(
                member.clone(),
                provider,
                Some(secret_ref),
                entry.forwarded_base_url().is_some(),
            )),
            // The error text is deliberately NOT carried into the refusal:
            // it can inline a credential path, an account id, or upstream
            // response bytes, and the refusal reaches operator stdout and
            // the reload WARN. Only the variant SHAPE classifies.
            Err(e) => refusals.push((member, refusal_for_build_error(&e))),
        }
    }

    if !refusals.is_empty() {
        return Err(pool_refusal_message(pool_name, &refusals));
    }
    if seats.is_empty() {
        return Err(format!(
            "pool `{}`: `members` is empty; list at least one provider entry or remove the pool",
            routectl_core::sanitize_for_log(pool_name),
        ));
    }
    Ok(PoolOutcome {
        seats: Arc::from(seats),
        placeholders,
    })
}

/// Why one pool member refuses the config. Each variant is chosen from the
/// SHAPE of the defect (an absent entry or ref, a ref that does not parse, a
/// containment verdict, the variant a build returned), never from the text
/// of an error, so the rule it renders carries no credential material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberRefusal {
    UnknownMember,
    MissingRef,
    MalformedRef,
    OutsideContainment,
    CredentialUnresolvedAtBuild,
    ProviderBuildFailed,
}

impl MemberRefusal {
    /// The rule the member broke, as the tail of a sentence naming it.
    const fn rule(self) -> &'static str {
        match self {
            Self::UnknownMember => "is not a known provider entry in [providers]",
            Self::MissingRef => "has no credential reference",
            Self::MalformedRef => {
                "has a credential reference that is not a well-formed secret reference"
            }
            Self::OutsideContainment => {
                "carries a managed credential outside the lane that credential is accepted on"
            }
            Self::CredentialUnresolvedAtBuild => {
                "has a stored credential that could not be resolved when its provider was built"
            }
            Self::ProviderBuildFailed => {
                "has a readable credential but its provider could not be built from the entry"
            }
        }
    }
}

/// One line naming the pool and each refused member with its rule. Both
/// keys are operator-written and the line is re-logged verbatim by the
/// reload path, so each is rendered through the shared log sanitizer.
fn pool_refusal_message(pool_name: &str, refusals: &[(&str, MemberRefusal)]) -> String {
    let members: Vec<String> = refusals
        .iter()
        .map(|(member, refusal)| {
            format!(
                "member `{}` {}",
                routectl_core::sanitize_for_log(member),
                refusal.rule()
            )
        })
        .collect();
    format!(
        "pool `{}`: {}; fix each named provider entry or remove it from the pool",
        routectl_core::sanitize_for_log(pool_name),
        members.join("; "),
    )
}

/// The unusable cause a failed credential probe earns, by the failure's
/// variant: only a missing login is `not_logged_in`.
const fn unusable_cause_for_probe_failure(failure: CredentialProbeFailure) -> UnusableCause {
    match failure {
        CredentialProbeFailure::NotLoggedIn => UnusableCause::NotLoggedIn,
        _ => UnusableCause::Unreadable,
    }
}

/// The parsed credential reference of one pool member, or the refusal its
/// absence / malformedness earns.
///
/// A member with no credential ref at all cannot authenticate an account, and
/// a member whose ref does not parse cannot be resolved -- both are
/// pre-build refusals, distinguished so the operator knows whether to add a
/// ref or fix one.
fn member_secret_ref(entry: &ProviderEntry) -> std::result::Result<SecretRef, MemberRefusal> {
    let uri = primary_api_key_uri(entry).ok_or(MemberRefusal::MissingRef)?;
    if uri.is_empty() {
        return Err(MemberRefusal::MissingRef);
    }
    SecretRef::parse(uri).map_err(|_| MemberRefusal::MalformedRef)
}

/// Classify a member build failure by the error VARIANT, never its text.
///
/// `Error::Auth` is the credential lane: the store could not produce a usable
/// credential for the ref after its probe passed. Everything else is a
/// provider-construction fault (a rejected base URL, an incoherent provider
/// block, a Codex login with no stored account id).
const fn refusal_for_build_error(e: &routectl_core::Error) -> MemberRefusal {
    match e {
        routectl_core::Error::Auth(_) | routectl_core::Error::Credential { .. } => {
            MemberRefusal::CredentialUnresolvedAtBuild
        }
        _ => MemberRefusal::ProviderBuildFailed,
    }
}

/// Build every member of every pool a selectable model routes at, exactly as
/// [`build_resolved_models_reported`] does, and return each pool's refusal.
///
/// This is how `config check` refuses the configs serve and reload refuse at
/// build: same member walk, same credential probe, same provider
/// construction. Only pools are built; no non-pool provider is constructed.
/// Whether the build touches the network is the store's business: a caller
/// that must stay offline passes a store whose reads never refresh.
pub async fn pool_member_refusals(
    config: &Config,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Vec<String> {
    install_resolved_codex_identity(config);
    let mut refusals = Vec::new();
    for (pool_name, pool) in routed_pools(config) {
        if let Err(refusal) = compile_pool(pool_name, pool, config, &secrets, &opts).await {
            refusals.push(refusal);
        }
    }
    refusals
}

/// The pools a selectable, buildable model names: the set
/// [`build_resolved_models_reported`] compiles.
fn routed_pools(config: &Config) -> BTreeMap<&str, &PoolEntry> {
    config
        .models
        .iter()
        .filter(|(nickname, entry)| {
            entry.selectable
                && crate::seat_pool::check_state_key_name("model nickname", nickname).is_ok()
        })
        .filter_map(|(_, entry)| config.pools.get_key_value(&entry.provider))
        .map(|(name, pool)| (name.as_str(), pool))
        .collect()
}

/// Build a `TokenSource` for a provider that needs per-request token
/// resolution. For `oauth://<provider>` refs this returns a
/// `ManagedToken` that re-enters `SecretStore::get` per request -- so
/// rotation in `~/.config/routectl/credentials.json` is picked up live
/// without restart. For static refs (env / file / literal) the value
/// is resolved once and cached as a `StaticToken`, semantically
/// equivalent to the pre-v0.7 in-memory `api_key: String`.
async fn resolve_token_source(
    secrets: &Arc<dyn SecretStore>,
    uri: &str,
) -> Result<Arc<dyn routectl_core::TokenSource>> {
    tracing::debug!(secret_scheme = scheme_of(uri), "resolving token source");
    let secret_ref = SecretRef::parse(uri)?;
    match secret_ref {
        SecretRef::OAuth { .. } => {
            let mt: Arc<dyn routectl_core::TokenSource> = Arc::new(ManagedToken {
                secret_ref,
                store: secrets.clone(),
            });
            Ok(mt)
        }
        // Static refs: resolve once and cache. Same hot-path cost as
        // the pre-v0.7 baked-in `api_key: String`.
        _ => {
            let v = secrets.get(&secret_ref).await?;
            let st: Arc<dyn routectl_core::TokenSource> =
                Arc::new(routectl_core::StaticToken::new(v));
            Ok(st)
        }
    }
}

/// `TokenSource` impl backed by a routectl-managed credentials store.
/// Holds the original `SecretRef` and an `Arc` to the store so each
/// `token()` call dispatches back through `SecretStore::get` -- which
/// for `oauth://` refs lands in `OAuthStore` (in-memory cache + future
/// refresh). The store is `Arc`-shared with the rest of routectl
/// so we never duplicate the credentials file in memory.
struct ManagedToken {
    secret_ref: SecretRef,
    store: Arc<dyn SecretStore>,
}

impl std::fmt::Debug for ManagedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedToken")
            .field("secret_ref", &self.secret_ref)
            .finish()
    }
}

#[async_trait::async_trait]
impl routectl_core::TokenSource for ManagedToken {
    async fn token(&self) -> Result<String> {
        self.store.get(&self.secret_ref).await
    }

    /// Forward upstream-401 notifications to the underlying store.
    /// For `oauth://` refs this lands in `OAuthStore::on_auth_failure`,
    /// which force-refreshes the token through the same per-provider
    /// single-flight gate that `near_expiry` refreshes use. Failures
    /// (invalid_grant, network error to token endpoint) propagate so
    /// the router surfaces an actionable auth error rather than
    /// masking dead credentials by walking the fallback chain.
    async fn on_auth_failure(&self) -> Result<()> {
        self.store.on_auth_failure(&self.secret_ref).await
    }
}

/// Resolve the ChatGPT account id for an openai-responses provider.
///
/// Precedence:
///   1. An operator-supplied `account_id_ref` always wins -- resolved
///      one-shot through the `SecretStore` (env / file / literal /
///      oauth). This is the explicit override escape hatch.
///   2. Otherwise, for an `oauth://<provider>` bearer, derive the id
///      from the logged-in session via `SecretStore::account_id` (the
///      `chatgpt_account_id` recorded at `routectl login`, stable
///      across token rotations). A missing session yields a clean
///      `Error::Config` pointing the operator at `routectl login
///      <provider>` rather than a confusing upstream 403 later.
///   3. Otherwise `None`. The validator has already guaranteed this
///      case only arises for non-ChatgptOauth surfaces (where the id
///      must be absent), so `None` is correct.
#[cfg(feature = "openai-responses")]
async fn resolve_responses_account_id(
    secrets: &Arc<dyn SecretStore>,
    api_key_ref: &str,
    account_id_ref: &Option<String>,
    name: &str,
) -> Result<Option<String>> {
    if let Some(uri) = account_id_ref {
        return Ok(Some(resolve(&**secrets, uri).await?));
    }
    let parsed = SecretRef::parse(api_key_ref)?;
    let SecretRef::OAuth { provider, .. } = &parsed else {
        return Ok(None);
    };
    match secrets.account_id(&parsed).await? {
        Some(id) => Ok(Some(id)),
        None => Err(routectl_core::Error::Config(format!(
            "openai-responses provider `{name}`: no ChatGPT account id found for \
             `oauth://{provider}`. Run `routectl login {provider}` first, or set \
             `account_id_ref` explicitly."
        ))),
    }
}

/// Install the process-global codex identity from the config's resolved
/// `codex_version` (or the pinned default when none is set), unless it is
/// already installed. Emits a structured INFO per chatgpt-oauth
/// openai-responses provider naming the effective version and its source.
/// Idempotent: the underlying `OnceLock` is set-once, so a hot reload
/// re-running the factory does not re-install (codex_version is
/// restart-required).
fn install_resolved_codex_identity(config: &Config) {
    use routectl_core::identity::codex::{CodexIdentity, PINNED_CODEX_VERSION, set_resolved};

    let configured = super::resolved_codex_version(config);
    let effective = configured
        .clone()
        .unwrap_or_else(|| PINNED_CODEX_VERSION.to_string());
    let installed = set_resolved(CodexIdentity::new(&effective));

    #[cfg(feature = "openai-responses")]
    {
        use routectl_core::identity::codex::resolved_identity;

        let source = if configured.is_some() {
            "configured"
        } else {
            "pinned"
        };
        if installed {
            for (name, entry) in &config.providers {
                if entry.is_chatgpt_oauth_responses() {
                    tracing::info!(
                        provider = %routectl_core::sanitize_for_log(name),
                        codex_version = %effective,
                        source,
                        "codex identity resolved"
                    );
                }
            }
        } else if effective != resolved_identity().version() {
            // A hot reload changed codex_version but the set-once identity
            // still serves the boot value: report the pending change instead
            // of falsely logging the new version as active.
            tracing::warn!(
                configured = %effective,
                active = %resolved_identity().version(),
                "codex_version changed but requires a daemon restart to take effect",
            );
        }
    }
    #[cfg(not(feature = "openai-responses"))]
    let _ = installed;
}

/// Build the per-nickname `ResolvedModel` table from a `Config`. Walks
/// `[models]` once, building one `Arc<dyn Provider>` per non-Bedrock
/// `[providers.X]` (cached across models referencing the same provider)
/// and one `Arc<dyn Provider>` per Bedrock-targeting model (each carries
/// its own `BedrockConfig.model_id`).
///
/// Returns a `BTreeMap<nickname, Arc<ResolvedModel>>` plus a list of
/// `(provider_name, error)` for providers that failed to build. The
/// caller is responsible for failing loudly when a failed-to-build
/// provider is referenced by an alias chain (mirrors the existing
/// `serve` / `test` "fail loudly when route is broken" guard).
///
/// `[models.X].selectable = false` entries are skipped; the returned
/// map does not contain them.
///
/// The tuple-returning shape for callers that do not consume the per-pool
/// build reports. [`build_resolved_models_reported`] is the same build with
/// the reports retained; this wrapper discards them.
pub async fn build_resolved_models(
    config: &Config,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<(BTreeMap<String, Arc<ResolvedModel>>, Vec<(String, String)>)> {
    let built = build_resolved_models_reported(config, secrets, opts).await?;
    Ok((built.models, built.failed))
}

/// Everything one `build_resolved_models_reported` pass observed.
///
/// The per-pool reports are the reason this shape exists: a pool's degraded
/// state is only visible AT BUILD TIME (config alone cannot see a credential
/// failure), so any surface that wants to report it must receive what the
/// build observed rather than re-derive it. `#[non_exhaustive]` so a later
/// observation can join without breaking construction.
#[non_exhaustive]
#[derive(Debug)]
pub struct ResolvedModelBuild {
    /// The per-nickname resolved-model table.
    pub models: BTreeMap<String, Arc<ResolvedModel>>,
    /// `(nickname, error)` for each model whose provider failed to build.
    pub failed: Vec<(String, String)>,
    /// One sanitized report per pool that at least one selectable model
    /// named, in pool-name order.
    pub pool_reports: Vec<PoolReport>,
}

/// [`build_resolved_models`] with the per-pool build reports retained.
///
/// Fails with `Error::Config` when any pool that a selectable model names has
/// a member that refuses the config, or no member at all (see
/// `compile_pool`). A pool whose members are all placeholders builds: it
/// answers a local refusal until a credential recovers. On a hot reload the
/// candidate build is rejected and the previous router stays live, which is
/// the existing behavior for any build error.
pub async fn build_resolved_models_reported(
    config: &Config,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<ResolvedModelBuild> {
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    // Install the process-global codex identity ONCE, before constructing
    // any provider (the openai-responses egress and the OAuth refresh
    // client read it via `resolved_identity()`). This is the shared factory
    // provider-loop boundary every construction path routes through, so the
    // configured version reaches the wire regardless of caller (serve,
    // reload, `routectl test`, doctor). Validation has already rejected
    // divergent values; codex_version is restart-required, so the set-once
    // contract holds across reloads. A structured INFO per chatgpt-oauth
    // provider names the effective version and its source.
    install_resolved_codex_identity(config);

    // Cache for non-Bedrock providers: name -> Arc.
    let mut provider_cache: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
    // Per-provider failed flag so we don't try to rebuild on every
    // model that references it.
    let mut provider_failed: BTreeMap<String, String> = BTreeMap::new();
    // Cache resolved Bedrock creds per provider name. The first model
    // referencing a Bedrock provider triggers `auth::resolve` (which
    // probes the credential chain / SSO); subsequent models on the
    // same provider reuse the resolved handle, sparing the SSO probe
    // round-trip. Each per-model BedrockProvider still gets its own
    // Arc (because `BedrockConfig.model_id` and other overrides
    // differ), but the credential layer is shared.
    #[cfg(feature = "bedrock")]
    let mut bedrock_auth_cache: BTreeMap<String, CachedBedrockAuth> = BTreeMap::new();
    // One compiled outcome per pool, and the models that named it. The cache
    // is what makes the seat set SHARED rather than rebuilt: the second model
    // on a pool clones the first's `Arc<[SeatTarget]>` instead of
    // reconstructing its members' providers.
    let mut pool_cache: BTreeMap<String, PoolOutcome> = BTreeMap::new();
    let mut pool_models: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (nickname, entry) in &config.models {
        if !entry.selectable {
            continue;
        }
        // A `#` in a model nickname would collide with a labeled seat's
        // runtime-state key (`{nickname}#{label}`), letting two distinct
        // dispatch identities share one circuit breaker. Config validation
        // already refuses it; this keeps an unvalidated config from building
        // one, dropping the model with the reason.
        if let Err(reason) = crate::seat_pool::check_state_key_name("model nickname", nickname) {
            failed.push((nickname.clone(), reason));
            continue;
        }
        let provider_entry = match resolve_model_provider(config, &entry.provider) {
            Some(ModelProviderTarget::Pool(pool)) => {
                let outcome = match pool_cache.get(&entry.provider) {
                    Some(cached) => cached.clone(),
                    None => {
                        // Computed ONCE per pool: every model naming this pool
                        // shares the same seat set by Arc, so the members'
                        // providers are built exactly once regardless of how many
                        // models route at the pool.
                        let built = compile_pool(&entry.provider, pool, config, &secrets, &opts)
                            .await
                            .map_err(routectl_core::Error::Config)?;
                        pool_cache.insert(entry.provider.clone(), built.clone());
                        built
                    }
                };
                pool_models
                    .entry(entry.provider.clone())
                    .or_default()
                    .push(nickname.clone());
                let resolved = resolved_pool_model(nickname, entry, outcome.seats);
                models.insert(nickname.clone(), Arc::new(resolved));
                continue;
            }
            Some(ModelProviderTarget::Provider(provider_entry)) => provider_entry,
            None => {
                failed.push((
                    nickname.clone(),
                    format!(
                        "model `{nickname}` references unknown provider or pool `{}`",
                        entry.provider
                    ),
                ));
                continue;
            }
        };

        // Bedrock: one Arc per model. The factory pumps the model's
        // upstream into BedrockConfig.model_id.
        #[cfg(feature = "bedrock")]
        let is_bedrock = matches!(provider_entry, ProviderEntry::Bedrock { .. });
        #[cfg(not(feature = "bedrock"))]
        let is_bedrock = false;

        let provider = if is_bedrock {
            #[cfg(feature = "bedrock")]
            {
                // A sibling model already failed cred resolution for
                // this provider: reuse the recorded error and skip
                // without re-probing SSO. Mirrors the non-Bedrock
                // `provider_failed` guard below so the SSO probe /
                // aws-config chain build is attempted at most once per
                // [providers.X] entry on the failure path, matching the
                // success-path dedup carried by `bedrock_auth_cache`.
                if let Some(prior_err) = provider_failed.get(&entry.provider) {
                    failed.push((nickname.clone(), prior_err.clone()));
                    continue;
                }
                // Credential resolution below reads the store ahead of the
                // per-provider build, so the managed credential containment
                // rules are enforced here first.
                if let Err(e) = validate_managed_oauth_credentials(&entry.provider, provider_entry)
                {
                    let msg = e.to_string();
                    tracing::warn!(
                        provider = %routectl_core::sanitize_for_log(&entry.provider),
                        model = %routectl_core::sanitize_for_log(nickname),
                        error = %msg,
                        "skipping Bedrock model (credential rejected)",
                    );
                    provider_failed.insert(entry.provider.clone(), msg.clone());
                    failed.push((nickname.clone(), msg));
                    continue;
                }
                // Cache the resolved creds per provider name so the
                // SSO probe / aws-config chain build only fires once
                // per [providers.X] entry, regardless of how many
                // [models.X] reference it.
                let cached = match bedrock_auth_cache.get(&entry.provider) {
                    Some(c) => Some(c.clone()),
                    None => match resolve_bedrock_auth_for_entry(provider_entry, &*secrets).await {
                        Ok(c) => {
                            bedrock_auth_cache.insert(entry.provider.clone(), c.clone());
                            Some(c)
                        }
                        Err(e) => {
                            // Bedrock cred resolution failed: the
                            // provider is unusable, so flag this
                            // model as failed (and skip every other
                            // model on the same provider via
                            // `provider_failed`).
                            let msg = e.to_string();
                            tracing::warn!(
                                provider = %routectl_core::sanitize_for_log(&entry.provider),
                                model = %routectl_core::sanitize_for_log(nickname),
                                error = %msg,
                                "skipping Bedrock model (creds resolution failed)",
                            );
                            provider_failed.insert(entry.provider.clone(), msg.clone());
                            failed.push((nickname.clone(), msg));
                            continue;
                        }
                    },
                };

                let overrides = BedrockModelOverrides {
                    model_id: entry.upstream.clone(),
                    additional_model_request_fields: entry.additional_request_fields.clone(),
                };
                match build_provider_with_bedrock_model_override(
                    &entry.provider,
                    provider_entry,
                    secrets.clone(),
                    opts.clone(),
                    Some(overrides),
                    cached,
                )
                .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = e.to_string();
                        tracing::warn!(
                            provider = %routectl_core::sanitize_for_log(&entry.provider),
                            model = %routectl_core::sanitize_for_log(nickname),
                            error = %msg,
                            "skipping Bedrock model (build failed)",
                        );
                        failed.push((nickname.clone(), msg));
                        continue;
                    }
                }
            }
            #[cfg(not(feature = "bedrock"))]
            {
                unreachable!("is_bedrock cannot be true without the bedrock feature");
            }
        } else if let Some(cached) = provider_cache.get(&entry.provider) {
            cached.clone()
        } else if let Some(prior_err) = provider_failed.get(&entry.provider) {
            // Provider already failed on a sibling model; reuse the
            // error and keep walking.
            failed.push((nickname.clone(), prior_err.clone()));
            continue;
        } else {
            match build_provider_with_options(
                &entry.provider,
                provider_entry,
                secrets.clone(),
                opts.clone(),
            )
            .await
            {
                Ok(p) => {
                    provider_cache.insert(entry.provider.clone(), p.clone());
                    p
                }
                Err(e) => {
                    let msg = e.to_string();
                    tracing::warn!(
                        provider = %routectl_core::sanitize_for_log(&entry.provider),
                        model = %routectl_core::sanitize_for_log(nickname),
                        error = %msg,
                        "skipping provider (build failed)",
                    );
                    provider_failed.insert(entry.provider.clone(), msg.clone());
                    failed.push((nickname.clone(), msg));
                    continue;
                }
            }
        };

        let mut resolved = ResolvedModel::new(
            nickname.clone(),
            entry.provider.clone(),
            provider,
            entry.upstream.clone(),
        );
        resolved = apply_model_knobs(resolved, nickname, entry);
        warn_context_management_needs_preserve(
            &entry.provider,
            nickname,
            provider_entry,
            entry.history_reasoning,
        );
        if let Some(uri) = primary_api_key_uri(provider_entry)
            && let Ok(sr) = SecretRef::parse(uri)
        {
            resolved = resolved.with_auth_secret_ref(sr);
        }
        models.insert(nickname.clone(), Arc::new(resolved));
    }

    let reports = pool_reports(&pool_cache, &pool_models);

    Ok(ResolvedModelBuild {
        models,
        failed,
        pool_reports: reports,
    })
}

/// Apply every `[models.X]` knob that is independent of how the model's
/// credential resolves.
///
/// Shared by the provider-backed and pool-backed resolution paths so the two
/// cannot drift: a knob added to one would otherwise silently not apply to
/// models on a pool, and the omission would be invisible (the knob simply
/// reads as unset).
pub fn apply_model_knobs(
    mut resolved: ResolvedModel,
    nickname: &str,
    entry: &crate::config::ModelEntry,
) -> ResolvedModel {
    // The nickname is an operator-written `[models.X]` table key and the
    // warn below renders it (field plus message body) on SUCCESSFUL builds,
    // so it is bound sanitized once here rather than at each use.
    let model_safe = routectl_core::sanitize_for_log(nickname);
    if let Some(d) = entry.reasoning_dialect {
        resolved = resolved.with_reasoning_dialect(d);
    }
    if let Some(h) = entry.history_reasoning {
        resolved = resolved.with_history_reasoning(h);
    }
    if !entry.header_extras.is_empty() {
        resolved = resolved.with_header_extras(entry.header_extras.clone());
    }
    if let Some(extras) = entry.payload_extras.as_ref() {
        resolved = resolved.with_payload_extras(extras.clone());
    }
    if let Some(ms) = entry.stream_first_byte_timeout_ms {
        if ms == 0 {
            tracing::warn!(
                model = %model_safe,
                "[models.{model_safe}] stream_first_byte_timeout_ms = 0 would abandon every stream before its first content-bearing chunk; ignoring the override"
            );
        } else {
            resolved = resolved.with_stream_first_byte_timeout_ms(ms);
        }
    }
    if let Some(label) = entry.reported_model.as_ref() {
        resolved = resolved.with_reported_model(label.clone());
    }
    if let Some(tokens) = entry.max_output_tokens {
        resolved = resolved.with_max_output_tokens(tokens);
    }
    if let Some(tokens) = entry.context_window {
        resolved = resolved.with_context_window(tokens);
    }
    resolved.with_visible_routectl_provider(entry.visible_routectl_provider)
}

/// Assemble the `ResolvedModel` for a pool-backed model from the pool's
/// shared seat set.
///
/// `provider` and `auth_secret_ref` mirror the FIRST BUILT seat: those two
/// fields are the single-target projection of a model, and dispatch reads
/// them only for a model with no seats. A pool of placeholders alone has no
/// built seat, so its projection is an inert provider that is never dialed
/// (every seat is a placeholder, and dispatch refuses a placeholder before
/// reading any provider). `provider_name` keeps the POOL name -- it is
/// what the operator wrote, what the strategy lookup keys on, and what
/// reload's rotation carry-over matches; every per-provider config lookup on
/// the dispatch path resolves against the seat's own `provider_name` instead.
fn resolved_pool_model(
    nickname: &str,
    entry: &crate::config::ModelEntry,
    seats: Arc<[SeatTarget]>,
) -> ResolvedModel {
    let built = seats.iter().find(|seat| !seat.is_placeholder());
    let provider = built
        .and_then(|seat| seat.provider.clone())
        .unwrap_or_else(super::inert_provider::inert_provider);
    let mut resolved = ResolvedModel::new(
        nickname,
        entry.provider.clone(),
        provider,
        entry.upstream.clone(),
    );
    if let Some(sr) = built.unwrap_or(&seats[0]).auth_secret_ref.clone() {
        resolved = resolved.with_auth_secret_ref(sr);
    }
    resolved = apply_model_knobs(resolved, nickname, entry);
    resolved.with_seats(seats)
}

/// The sanitized per-pool build reports, one per pool that at least one
/// selectable model named. A pool no model routes at is not reported: it was
/// never compiled, so there is nothing observed to report.
fn pool_reports(
    outcomes: &BTreeMap<String, PoolOutcome>,
    models_by_pool: &BTreeMap<String, Vec<String>>,
) -> Vec<PoolReport> {
    models_by_pool
        .iter()
        .filter_map(|(pool, models)| {
            let outcome = outcomes.get(pool)?;
            let placeholders = outcome.placeholders.clone();
            let usable_members = outcome.seats.len() - placeholders.len();
            Some(PoolReport {
                pool: pool.clone(),
                models: models.clone(),
                configured_members: outcome.seats.len(),
                usable_members,
                placeholders,
            })
        })
        .collect()
}

/// Clone a provider entry's `CloakConfig` and stamp the global
/// `[cache] normalize_tools` switch onto it. `normalize_tools` is not an
/// operator-facing `[cloak]` key (it is `#[serde(skip)]` on `CloakConfig`);
/// it reaches the cloak seam only through this build-time stamp, so the
/// single operator control stays under `[cache]`.
fn cloak_with_normalize(
    cloak: &routectl_providers::anthropic_api::CloakConfig,
    normalize_tools: bool,
) -> routectl_providers::anthropic_api::CloakConfig {
    let mut c = cloak.clone();
    c.normalize_tools = normalize_tools;
    c
}

/// Extract the embedded IPv4 of an IPv4-COMPATIBLE IPv6 address
/// (`::a.b.c.d`, the `::/96` prefix -- first six segments all zero),
/// distinct from the IPv4-MAPPED form (`::ffff:a.b.c.d`) that
/// `Ipv6Addr::to_ipv4_mapped` already canonicalizes. Returns `None`
/// for any address whose high six segments are not all zero. The
/// embedded v4 is read from the last two segments. Callers run the
/// link-local / loopback predicates on the result so an SSRF target
/// like `::169.254.169.254` (cloud metadata) cannot slip past in
/// IPv4-compatible disguise.
pub fn ipv4_compatible_embedded(ip: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let seg = ip.segments();
    if seg[0..6].iter().any(|&s| s != 0) {
        return None;
    }
    Some(std::net::Ipv4Addr::from(
        ((seg[6] as u32) << 16) | seg[7] as u32,
    ))
}

/// Returns the URI scheme of a SecretRef literal (`env://`, `file://`,
/// `oauth://`, `literal:`, or `unknown`) without leaking the value. Used
/// for structured tracing fields and config errors so they indicate WHICH
/// source a ref names without revealing the secret name or its content.
fn scheme_of(uri: &str) -> &'static str {
    SecretScheme::classify(uri).map_or("unknown", SecretScheme::as_prefix)
}

// v0.6.0: ReasoningDialect / HistoryReasoning mappings moved to
// `From` impls on `routectl_providers::openai_compat::{ReasoningDialect,
// HistoryReasoning}` so the dispatch-layer carrier
// (`ChatRequest::routectl_internal`) can convert without the factory in
// the middle. The factory no longer reads either field off
// `[providers.X]` (both fields live on `[models.X]` now).

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;

#[cfg(test)]
#[path = "build_placeholder_tests.rs"]
mod build_placeholder_tests;
