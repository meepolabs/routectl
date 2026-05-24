//! Build concrete `Provider` instances from `ProviderEntry` config rows.
//! Resolves secret references via a `SecretStore` at build time so the
//! provider can hold the plaintext API key it needs.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Provider, Result};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

#[cfg(feature = "bedrock")]
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};

#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::{
    AuthKind as OpenaiResponsesAuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};

#[cfg(feature = "bedrock")]
use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
use crate::config::{Config, ProviderEntry};
use crate::resolved::ResolvedModel;

/// Convenience wrapper that builds a provider with `BuildOptions::default()`.
///
/// Note for Bedrock providers: `BuildOptions::default()` carries empty
/// `bedrock_allowed_betas` / `bedrock_allowed_body_fields` lists, which
/// puts both filters in pass-through mode -- routectl forwards every
/// flag/field to AWS as-is. That is the correct discovery default; if
/// you want filtering, populate the lists from
/// `[bedrock] allowed_betas` / `[bedrock] allowed_body_fields` (see
/// `examples/bedrock.toml`) and pass them via
/// `build_provider_with_options`. routectl-cli callers (`server`,
/// `commands::test`) do this automatically.
pub async fn build_provider(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
) -> Result<Arc<dyn Provider>> {
    build_provider_with_options(name, entry, secrets, BuildOptions::default()).await
}

/// Server-wide options that influence per-provider construction.
/// Defaults are equivalent to the legacy `build_provider` behavior.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BuildOptions {
    /// When `true`, providers reject requests carrying canonical-only
    /// fields they cannot represent on the wire (e.g. an OpenAI-compat
    /// egress receiving an Anthropic `cache_control` block). Default
    /// `false` -- warn-and-drop. Set from `[server] strict_translation`.
    pub strict_translation: bool,
    /// Bedrock-accepted `anthropic_beta` flags. Sourced from
    /// `[bedrock] allowed_betas` TOML and applied to every Bedrock
    /// provider. routectl ships no const default; AWS schema drift is
    /// operator-tracked. Empty list = pass-through (no filter applied).
    pub bedrock_allowed_betas: Vec<String>,
    /// Bedrock-accepted top-level body fields / Converse extras keys.
    /// Sourced from `[bedrock] allowed_body_fields` TOML. Empty list =
    /// pass-through (no filter applied) -- the same discovery-mode
    /// default as `bedrock_allowed_betas`.
    pub bedrock_allowed_body_fields: Vec<String>,
}

impl BuildOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_strict_translation(mut self, strict: bool) -> Self {
        self.strict_translation = strict;
        self
    }

    pub fn with_bedrock_allowed_betas(mut self, list: Vec<String>) -> Self {
        self.bedrock_allowed_betas = list;
        self
    }

    pub fn with_bedrock_allowed_body_fields(mut self, list: Vec<String>) -> Self {
        self.bedrock_allowed_body_fields = list;
        self
    }
}

#[tracing::instrument(skip_all, fields(provider = %name))]
pub async fn build_provider_with_options(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<Arc<dyn Provider>> {
    #[cfg(feature = "bedrock")]
    {
        build_provider_inner(name, entry, secrets, opts, None, None, None).await
    }
    #[cfg(not(feature = "bedrock"))]
    {
        build_provider_inner(name, entry, secrets, opts, None).await
    }
}

/// Per-model overrides applied to a Bedrock provider build. These
/// fields used to live on `[providers.X]` but moved to `[models.X]`
/// in v0.6.0; the factory threads them through here so each Bedrock
/// model gets a `BedrockConfig` with the right values.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Default)]
pub(crate) struct BedrockModelOverrides {
    pub model_id: String,
    pub adaptive_thinking: Option<bool>,
    pub additional_model_request_fields: Option<serde_json::Value>,
}

/// Pre-resolved Bedrock credentials. Cached per `[providers.X]` entry
/// in `build_resolved_models` so multiple models on the same Bedrock
/// provider share one SSO probe / one `aws-config` chain construction.
/// Without this, building 5 Bedrock models on the same provider would
/// hit the credential chain 5 times (5x cold-start latency on SSO).
#[cfg(feature = "bedrock")]
#[derive(Clone)]
pub(crate) struct CachedBedrockAuth {
    pub creds: routectl_providers::bedrock::BedrockCreds,
    pub resolved: routectl_providers::bedrock::auth::ResolvedCreds,
}

/// Per-model overrides applied to a non-Bedrock provider build that
/// need to fan out one provider instance per model. v0.6.0 moved
/// `adaptive_thinking` from `[providers.X]` to `[models.X]` so the
/// AnthropicApi provider needs a per-model build when the flag is
/// set. OpenaiCompat / OpenaiResponses don't read adaptive_thinking
/// today so they still share one cached `Arc<dyn Provider>` per
/// `[providers.X]`.
#[derive(Debug, Clone, Default)]
pub(crate) struct AnthropicModelOverrides {
    pub adaptive_thinking: Option<bool>,
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
pub(crate) async fn build_provider_with_bedrock_model_override(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    bedrock_overrides: Option<BedrockModelOverrides>,
    cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    build_provider_inner(
        name,
        entry,
        secrets,
        opts,
        bedrock_overrides,
        None,
        cached_auth,
    )
    .await
}

/// Variant that lets the caller override AnthropicApi model-specific
/// fields. v0.6.0 moved `adaptive_thinking` to `[models.X]`, so each
/// AnthropicApi model entry that opts into the adaptive shape needs
/// its own `Arc<AnthropicApiProvider>` with `cfg.adaptive_thinking`
/// set. The Bedrock override path is unaffected.
pub(crate) async fn build_provider_with_anthropic_model_override(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    anthropic_overrides: Option<AnthropicModelOverrides>,
) -> Result<Arc<dyn Provider>> {
    #[cfg(feature = "bedrock")]
    {
        build_provider_inner(name, entry, secrets, opts, None, anthropic_overrides, None).await
    }
    #[cfg(not(feature = "bedrock"))]
    {
        build_provider_inner(name, entry, secrets, opts, anthropic_overrides).await
    }
}

#[tracing::instrument(skip_all, fields(provider = %name))]
async fn build_provider_inner(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    #[cfg(feature = "bedrock")] bedrock_overrides: Option<BedrockModelOverrides>,
    anthropic_overrides: Option<AnthropicModelOverrides>,
    #[cfg(feature = "bedrock")] cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    match entry {
        ProviderEntry::OpenaiCompat {
            base_url,
            api_key_ref,
            header_extras,
            payload_extras: _,
            user_agent,
            runtime: _,
        } => {
            validate_base_url_scheme(name, base_url)?;
            let api_key = resolve(&*secrets, api_key_ref).await?;
            // v0.6.0: reasoning_dialect + history_reasoning moved off
            // [providers.X] to [models.X]; the egress reads them from
            // `req.routectl_internal` at request time. The config-side
            // defaults left here are pure fallback for library
            // consumers constructing an `OpenAiCompatConfig` directly
            // (no router).
            let cfg = OpenAiCompatConfig {
                id: format!("openai-compat:{name}"),
                base_url: base_url.clone(),
                api_key,
                header_extras: header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                payload_extras: None,
                reasoning_dialect: Default::default(),
                history_reasoning: Default::default(),
                user_agent: user_agent.clone(),
                strict_translation: opts.strict_translation,
                disable_stream_include_usage: false,
            };
            Ok(Arc::new(OpenAiCompatProvider::new(cfg)))
        }
        ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            anthropic_version,
            auth_kind,
            header_extras,
            payload_extras: _,
            user_agent,
            allowed_betas,
            forward_client_headers,
            runtime: _,
        } => {
            validate_base_url_scheme(name, base_url)?;
            // OAuth-aware: for `oauth://<provider>` refs the provider
            // gets a `ManagedToken` that re-enters `SecretStore::get`
            // per request, so token rotation in credentials.json is
            // picked up live without restart. For env / file / literal
            // the value is resolved once and wrapped in `StaticToken`
            // (semantically equivalent to the pre-v0.7 `api_key:
            // String` field).
            let auth = resolve_token_source(&secrets, api_key_ref).await?;
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
                adaptive_thinking: anthropic_overrides
                    .as_ref()
                    .and_then(|o| o.adaptive_thinking),
                allowed_betas: allowed_betas.clone(),
                forward_client_headers: forward_client_headers.clone(),
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
            originator,
            runtime: _,
        } => {
            validate_openai_responses_account_id(name, *auth_kind, account_id_ref)?;
            let api_key = resolve(&*secrets, api_key_ref).await?;
            let account_id = match account_id_ref {
                Some(uri) => Some(resolve(&*secrets, uri).await?),
                None => None,
            };
            let resolved_base_url = base_url
                .clone()
                .unwrap_or_else(|| default_responses_base(*auth_kind));
            validate_base_url_scheme(name, &resolved_base_url)?;
            let cfg = OpenAiResponsesConfig {
                id: format!("openai-responses:{name}"),
                api_key,
                account_id,
                base_url: resolved_base_url,
                auth_kind: *auth_kind,
                header_extras: header_extras
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                user_agent: user_agent.clone(),
                originator: originator.clone(),
            };
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
            runtime: _,
        } => {
            validate_bedrock_allowlists(
                matches!(api_shape, BedrockApiShapeConfig::Invoke),
                !anthropic_beta.is_empty(),
                &opts.bedrock_allowed_betas,
                &opts.bedrock_allowed_body_fields,
            )?;
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
            // v0.6.0: model_id, adaptive_thinking, and
            // additional_model_request_fields all come from the model
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
                allowed_betas: opts.bedrock_allowed_betas.clone(),
                allowed_body_fields: opts.bedrock_allowed_body_fields.clone(),
                additional_model_request_fields: overrides.additional_model_request_fields,
                adaptive_thinking: overrides.adaptive_thinking,
            };
            Ok(Arc::new(BedrockProvider::new(cfg, resolved)))
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
fn map_bedrock_api_shape(s: BedrockApiShapeConfig) -> BedrockApiShape {
    match s {
        BedrockApiShapeConfig::Invoke => BedrockApiShape::Invoke,
        BedrockApiShapeConfig::Converse => BedrockApiShape::Converse,
    }
}

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
    match entry {
        ProviderEntry::OpenaiCompat { api_key_ref, .. } => Some(api_key_ref),
        ProviderEntry::AnthropicApi { api_key_ref, .. } => Some(api_key_ref),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { api_key_ref, .. } => Some(api_key_ref),
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock { .. } => None,
    }
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
/// PR2 refresh). The store is `Arc`-shared with the rest of routectl
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
pub async fn build_resolved_models(
    config: &Config,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<(BTreeMap<String, Arc<ResolvedModel>>, Vec<(String, String)>)> {
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut failed: Vec<(String, String)> = Vec::new();
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

    for (nickname, entry) in &config.models {
        if !entry.selectable {
            continue;
        }
        let Some(provider_entry) = config.providers.get(&entry.provider) else {
            failed.push((
                nickname.clone(),
                format!(
                    "model `{nickname}` references unknown provider `{}`",
                    entry.provider
                ),
            ));
            continue;
        };

        // Bedrock: one Arc per model. The factory pumps the model's
        // upstream into BedrockConfig.model_id.
        #[cfg(feature = "bedrock")]
        let is_bedrock = matches!(provider_entry, ProviderEntry::Bedrock { .. });
        #[cfg(not(feature = "bedrock"))]
        let is_bedrock = false;

        // AnthropicApi with `[models.X] thinking = "adaptive"`:
        // one Arc per model so each gets its own
        // `AnthropicApiConfig::adaptive_thinking` value. v0.6.0 moved
        // the flag from `[providers.X]` to `[models.X]`. AnthropicApi
        // models that leave the flag at None still share one cached
        // `Arc<dyn Provider>` per `[providers.X]`.
        let is_anthropic_per_model = matches!(provider_entry, ProviderEntry::AnthropicApi { .. })
            && entry.is_adaptive_thinking();

        let provider = if is_bedrock {
            #[cfg(feature = "bedrock")]
            {
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
                                provider = %entry.provider,
                                model = %nickname,
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
                    adaptive_thinking: if entry.is_adaptive_thinking() {
                        Some(true)
                    } else {
                        None
                    },
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
                            provider = %entry.provider,
                            model = %nickname,
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
        } else if is_anthropic_per_model {
            let overrides = AnthropicModelOverrides {
                adaptive_thinking: Some(true),
            };
            match build_provider_with_anthropic_model_override(
                &entry.provider,
                provider_entry,
                secrets.clone(),
                opts.clone(),
                Some(overrides),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    tracing::warn!(
                        provider = %entry.provider,
                        model = %nickname,
                        error = %msg,
                        "skipping AnthropicApi model (build failed)",
                    );
                    failed.push((nickname.clone(), msg));
                    continue;
                }
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
                        provider = %entry.provider,
                        model = %nickname,
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
        )
        .with_reasoning(entry.reasoning_defaults_view());
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
            resolved = resolved.with_stream_first_byte_timeout_ms(ms);
        }
        if let Some(uri) = primary_api_key_uri(provider_entry) {
            if let Ok(sr) = SecretRef::parse(uri) {
                resolved = resolved.with_auth_secret_ref(sr);
            }
        }
        models.insert(nickname.clone(), Arc::new(resolved));
    }

    Ok((models, failed))
}

/// Reject `http://` (cleartext) base_urls at build time so an
/// operator typo doesn't silently exfiltrate API keys + prompts in
/// the clear. Loopback URLs (127.x, ::1, localhost) are exempt
/// because the local-dev workflow and integration tests rely on
/// `http://127.0.0.1:N` mock servers.
///
/// Also rejects link-local hosts (IPv4 `169.254.0.0/16`, IPv6
/// `fe80::/10`) regardless of scheme. The IPv4 link-local range
/// covers cloud-instance-metadata services (AWS / Azure / GCP all
/// use `169.254.169.254`) -- an operator who accidentally pastes
/// the metadata URL or is socially engineered into doing so would
/// otherwise leak SigV4-signed requests + API keys to a service
/// that exposes IAM credentials. Defense-in-depth: routectl is a
/// gateway, not a privileged client of the metadata service.
fn validate_base_url_scheme(provider_name: &str, base_url: &str) -> Result<()> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        // Operator-asserted "use the provider kind's default" -- the
        // factory will substitute the auth_kind-appropriate default
        // base_url when building the provider. Surface a TRACE so an
        // operator wondering why their request landed on a vendor
        // default can find it in the logs without flipping debug.
        tracing::trace!(
            provider = provider_name,
            "base_url empty; provider will use its kind-default endpoint",
        );
        return Ok(());
    }
    let url = match url::Url::parse(trimmed) {
        Ok(u) => u,
        Err(e) => {
            return Err(routectl_core::Error::Config(format!(
                "provider `{provider_name}`: base_url `{trimmed}` is not a valid URL: {e}"
            )));
        }
    };
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(routectl_core::Error::Config(format!(
            "provider `{provider_name}`: base_url scheme `{scheme}` is not allowed; \
             use https:// (or http:// for loopback only)"
        )));
    }

    // Link-local rejection (regardless of scheme). Covers cloud
    // metadata services. `Ipv4Addr::is_link_local` is stable since
    // 1.0 (covers 169.254.0.0/16). For IPv6 we check the fe80::/10
    // prefix manually since `is_unicast_link_local` was only
    // stabilized recently and we want to keep MSRV low.
    if let Some(host) = url.host() {
        let link_local = match host {
            url::Host::Ipv4(ip) => ip.is_link_local(),
            url::Host::Ipv6(ip) => (ip.segments()[0] & 0xffc0) == 0xfe80,
            url::Host::Domain(_) => false,
        };
        if link_local {
            return Err(routectl_core::Error::Config(format!(
                "provider `{provider_name}`: base_url `{trimmed}` targets a link-local \
                 address; cloud-metadata IPs (169.254.169.254 etc.) and IPv6 fe80::/10 \
                 are blocked at build time to prevent SSRF / credential leak"
            )));
        }
    }

    if scheme == "https" {
        return Ok(());
    }
    // http:// is permitted only for loopback hosts so local-dev and
    // integration tests work.
    let host = url.host_str().unwrap_or("");
    let is_loopback = host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
        || host.starts_with("127.")
        || url
            .host()
            .and_then(|h| match h {
                url::Host::Ipv4(ip) => Some(ip.is_loopback()),
                url::Host::Ipv6(ip) => Some(ip.is_loopback()),
                url::Host::Domain(_) => None,
            })
            .unwrap_or(false);
    if is_loopback {
        return Ok(());
    }
    Err(routectl_core::Error::Config(format!(
        "provider `{provider_name}`: base_url `{trimmed}` uses cleartext http:// for \
         non-loopback host `{host}` -- API keys and prompt content would be sent in \
         the clear. Use https:// (or bind a local proxy on 127.0.0.1)"
    )))
}

/// Returns the URI scheme of a SecretRef literal (`env://`, `file://`,
/// `literal:`, or `unknown`) without leaking the value. Used for
/// structured tracing fields so log lines indicate WHICH source resolved
/// (env var vs. file vs. inline) without revealing the secret name or
/// its content.
fn scheme_of(uri: &str) -> &'static str {
    if uri.starts_with("env://") {
        "env://"
    } else if uri.starts_with("file://") {
        "file://"
    } else if uri.starts_with("literal:") {
        "literal:"
    } else {
        "unknown"
    }
}

// v0.6.0: ReasoningDialect / HistoryReasoning mappings moved to
// `From` impls on `routectl_providers::openai_compat::{ReasoningDialect,
// HistoryReasoning}` so the dispatch-layer carrier
// (`ChatRequest::routectl_internal`) can convert without the factory in
// the middle. The factory no longer reads either field off
// `[providers.X]` (both fields live on `[models.X]` now).

/// Pick the default Responses API base URL for a given auth_kind. The
/// factory uses this whenever the operator left `base_url` unset.
/// Centralizing the mapping here (rather than computing it on TOML
/// deserialize) avoids the chicken-and-egg problem that `auth_kind`
/// isn't known at the same time `base_url`'s serde default would
/// fire.
#[cfg(feature = "openai-responses")]
fn default_responses_base(auth_kind: OpenaiResponsesAuthKind) -> String {
    match auth_kind {
        OpenaiResponsesAuthKind::ChatgptOauth => "https://chatgpt.com/backend-api/codex".into(),
        OpenaiResponsesAuthKind::ApiKey => "https://api.openai.com/v1".into(),
        // BedrockMantle URL is region-specific; CG.D wires in a
        // region-aware default. The placeholder here forces the
        // operator to set `base_url` explicitly today (the
        // provider's NotImplemented auth would fire first anyway).
        OpenaiResponsesAuthKind::BedrockMantle => {
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1".into()
        }
    }
}

/// Validate the `account_id_ref` invariant: required for ChatgptOauth,
/// forbidden for the other variants. A misconfigured TOML surfaces
/// here as a clean `Error::Config` rather than a confusing upstream
/// 401/403 at first request time.
#[cfg(feature = "openai-responses")]
fn validate_openai_responses_account_id(
    name: &str,
    auth_kind: OpenaiResponsesAuthKind,
    account_id_ref: &Option<String>,
) -> Result<()> {
    use routectl_core::Error;

    let has_account = account_id_ref.is_some();
    let needs_account = matches!(auth_kind, OpenaiResponsesAuthKind::ChatgptOauth);
    match (needs_account, has_account) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(Error::Config(format!(
            "openai-responses provider `{name}`: auth_kind = \"chatgpt-oauth\" \
             requires `account_id_ref` (the ChatGPT account UUID)"
        ))),
        (false, true) => Err(Error::Config(format!(
            "openai-responses provider `{name}`: `account_id_ref` is only valid \
             when auth_kind = \"chatgpt-oauth\"; remove it for {auth_kind:?}"
        ))),
    }
}

/// Routectl-mandatory body fields: keys routectl writes into every
/// Bedrock-Invoke body. If `[bedrock] allowed_body_fields` is non-empty
/// AND missing any of these, the egress drops them on send and the
/// upstream 400s the malformed body. Surfaces here as a clean startup
/// error instead. (Skipped entirely when `allowed_body_fields` is
/// empty -- that puts the filter in pass-through mode.)
///
/// Keep in sync with `is_bedrock_invoke_managed_key` in
/// `routectl-providers/src/bedrock/invoke.rs` -- that is the writer
/// side; this is the validator side.
#[cfg(feature = "bedrock")]
const BEDROCK_REQUIRED_BODY_FIELDS: &[&str] = &["anthropic_version", "max_tokens", "messages"];

/// Validate the per-deployment Bedrock allowlists.
///
/// Empty lists are PASS-THROUGH mode -- no filter applies, so no
/// validation is needed. The operator is in discovery mode (capturing
/// observed traffic via `ROUTECTL_LOG=routectl_providers::bedrock=trace`
/// to build their list) or has explicitly opted out of routectl-side
/// filtering. Either way, validation is only meaningful when the
/// operator has populated a non-empty list and we want to catch
/// configurations that would silently break their requests.
///
/// When `allowed_body_fields` is non-empty, validate:
///   - Routectl-mandatory keys (`messages`, `anthropic_version`,
///     `max_tokens`) are present -- but only when at least one provider
///     uses `api_shape = "invoke"`. Those keys live at the AWS top
///     level on Converse and never appear in
///     `additionalModelRequestFields`, so a Converse-only deployment
///     is unaffected by their absence from the allowlist.
///   - If any provider has a `[providers.X] anthropic_beta` floor,
///     `anthropic_beta` is on the list -- otherwise the filter
///     silently drops the operator-asserted always-send array. Applies
///     to both Invoke (top-level body) and Converse
///     (`additionalModelRequestFields` bag).
#[cfg(feature = "bedrock")]
fn validate_bedrock_allowlists(
    has_invoke_provider: bool,
    has_provider_beta_floor: bool,
    _allowed_betas: &[String],
    allowed_body_fields: &[String],
) -> Result<()> {
    use routectl_core::Error;

    // Pass-through mode: nothing to validate.
    if allowed_body_fields.is_empty() {
        return Ok(());
    }

    if has_invoke_provider {
        let missing: Vec<&str> = BEDROCK_REQUIRED_BODY_FIELDS
            .iter()
            .copied()
            .filter(|required| !allowed_body_fields.iter().any(|s| s == required))
            .collect();
        if !missing.is_empty() {
            return Err(Error::Config(format!(
                "[bedrock] allowed_body_fields is missing routectl-mandatory keys \
                 {missing:?}. Without these, every Bedrock Invoke request 400s on \
                 the egress. See examples/bedrock.toml for the full baseline; or \
                 remove `[bedrock] allowed_body_fields` entirely to disable \
                 filtering and run in discovery mode."
            )));
        }
    }

    if has_provider_beta_floor && !allowed_body_fields.iter().any(|s| s == "anthropic_beta") {
        return Err(Error::Config(
            "[bedrock] allowed_body_fields is missing `anthropic_beta`, but at \
             least one [providers.X] bedrock entry sets anthropic_beta. The \
             per-provider floor is operator-asserted always-send; include \
             `anthropic_beta` in allowed_body_fields or remove the per-provider \
             floor. See examples/bedrock.toml for the baseline."
                .into(),
        ));
    }

    Ok(())
}

/// Validate that `[bedrock]` allowlists are coherent with the
/// configured providers. Returns Ok in two cases:
///
///   - No provider has `kind = "bedrock"` (no-op).
///   - `[bedrock] allowed_body_fields` is empty (pass-through mode --
///     routectl forwards the assembled body verbatim, so there is
///     nothing to validate). This is the discovery-mode default:
///     bring up routectl, observe traffic via
///     `ROUTECTL_LOG=routectl_providers::bedrock=trace`, then build
///     `allowed_betas` / `allowed_body_fields` from what you see.
///
/// Returns Err only when the operator has populated a non-empty
/// `allowed_body_fields` that:
///
///   - Is missing routectl-mandatory keys (`messages`,
///     `anthropic_version`, `max_tokens`), which would silently break
///     every Bedrock request.
///   - Is missing `anthropic_beta` while a `[providers.X]` entry sets
///     a `anthropic_beta` floor that the filter would then drop.
///
/// `allowed_betas` is independent: empty there just disables betas
/// filtering, allowed there just gates which betas survive. No
/// validation of `allowed_betas` shape is needed.
///
/// Call once per process startup BEFORE building any providers.
#[cfg(feature = "bedrock")]
pub fn validate_bedrock_global_config(config: &crate::config::Config) -> Result<()> {
    let mut bedrock_in_use = false;
    let mut has_invoke_provider = false;
    let mut has_provider_beta_floor = false;
    for entry in config.providers.values() {
        if let crate::config::ProviderEntry::Bedrock {
            api_shape,
            anthropic_beta,
            ..
        } = entry
        {
            bedrock_in_use = true;
            has_invoke_provider |=
                matches!(api_shape, crate::config::BedrockApiShapeConfig::Invoke);
            has_provider_beta_floor |= !anthropic_beta.is_empty();
        }
    }
    if !bedrock_in_use {
        return Ok(());
    }

    validate_bedrock_allowlists(
        has_invoke_provider,
        has_provider_beta_floor,
        &config.bedrock.allowed_betas,
        &config.bedrock.allowed_body_fields,
    )
}

/// Validate the `[models.X] thinking` / `effort` knobs across every
/// configured model.
///
/// v0.6.0 collapsed the old free-form `thinking: String` field into
/// two typed enums: `thinking: ThinkingChoice` (closed `Bool | Adaptive`)
/// and `effort: EffortLevel` (closed lowercase enum). Both reject bad
/// values at TOML parse time, so the heavy string validation that
/// lived here pre-v0.6 is no longer needed. The function is kept as
/// a stable startup-validation hook (called from `routectl-cli`
/// `commands::config`, `commands::test`, and `server::start`) so the
/// CLI surface keeps a single place to add semantic invariants if
/// future shapes need them.
///
/// Call once per process startup BEFORE building any providers.
pub fn validate_reasoning_defaults(_config: &crate::config::Config) -> Result<()> {
    Ok(())
}

/// Validate that every entry in `[aliases]` resolves to a known and
/// selectable `[models.X]` nickname. Walks both `AliasValue::Single`
/// and `AliasValue::Chain`; accumulates every offending nickname into
/// one consolidated startup error so the operator gets the full list
/// in one shot.
///
/// Three failure modes:
///
///   - alias references a nickname that doesn't exist in `[models]`.
///     Common cause: typo, or the operator deleted a model row but
///     forgot to update the alias.
///
///   - alias references a `selectable = false` nickname. The model
///     parses but the router refuses to dispatch to it; passing it
///     through as a route silently breaks at request time.
///
///   - empty `AliasValue::Chain([])`. An alias with no targets
///     resolves to `UnknownAlias` at request time, which is identical
///     to the alias not being declared at all -- surface the
///     misconfiguration at startup.
///
/// Call once per process startup AFTER `validate_reasoning_defaults`
/// and BEFORE `build_resolved_models`. Glob keys (`claude-*` etc.)
/// are validated identically to exact keys -- the chain target must
/// still be a known nickname even though the alias key matches a
/// pattern.
pub fn validate_alias_chain_targets(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    let mut errors: Vec<String> = Vec::new();
    for (alias, value) in &config.aliases {
        if value.is_empty() {
            errors.push(format!(
                "alias `{alias}`: chain is empty -- an alias with no targets \
                 resolves to UnknownAlias at request time, which is the same \
                 as not declaring the alias at all"
            ));
            continue;
        }
        for nickname in value.nicknames() {
            match config.models.get(nickname) {
                None => {
                    errors.push(format!(
                        "alias `{alias}`: target `{nickname}` is not a known \
                         model nickname in [models]"
                    ));
                }
                Some(model) if !model.selectable => {
                    errors.push(format!(
                        "alias `{alias}`: target `{nickname}` is declared but \
                         `selectable = false`; alias chains must reference \
                         selectable models"
                    ));
                }
                Some(_) => {}
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

#[cfg(test)]
mod base_url_validation_tests {
    use super::validate_base_url_scheme;

    #[test]
    fn https_passes() {
        assert!(validate_base_url_scheme("p", "https://api.openai.com").is_ok());
        assert!(validate_base_url_scheme("p", "https://api.anthropic.com").is_ok());
    }

    #[test]
    fn http_loopback_passes() {
        assert!(validate_base_url_scheme("p", "http://127.0.0.1:8080").is_ok());
        assert!(validate_base_url_scheme("p", "http://localhost:8080").is_ok());
        assert!(validate_base_url_scheme("p", "http://[::1]:8080").is_ok());
        // 127.x range covers any IPv4 loopback alias.
        assert!(validate_base_url_scheme("p", "http://127.0.0.5:8080").is_ok());
    }

    #[test]
    fn http_public_host_rejected() {
        let err = validate_base_url_scheme("acme", "http://api.openai.com").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("acme"), "got: {msg}");
        assert!(msg.contains("cleartext"), "got: {msg}");
        assert!(msg.contains("api.openai.com"), "got: {msg}");
    }

    /// Pin: AWS / Azure / GCP cloud-instance metadata IP must be
    /// rejected even with https. Link-local egress would leak SigV4
    /// and API keys to whatever service the operator was tricked
    /// into pointing at.
    #[test]
    fn https_aws_imds_rejected() {
        let err =
            validate_base_url_scheme("p", "https://169.254.169.254/latest/meta-data/").unwrap_err();
        assert!(err.to_string().contains("link-local"));
    }

    /// Pin: 169.254/16 link-local range rejected wholesale (the IMDS
    /// IP is the obvious target but the whole prefix is unsafe).
    #[test]
    fn https_link_local_ipv4_range_rejected() {
        for host in ["169.254.0.1", "169.254.42.42", "169.254.255.255"] {
            let url = format!("https://{host}/");
            let err = validate_base_url_scheme("p", &url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {host}; got: {err}"
            );
        }
    }

    /// Pin: IPv6 fe80::/10 unicast link-local rejected.
    #[test]
    fn https_link_local_ipv6_rejected() {
        for url in [
            "https://[fe80::1]/",
            "https://[febf::1]/",
            "https://[fea0:abcd::1]/",
        ] {
            let err = validate_base_url_scheme("p", url).unwrap_err();
            assert!(
                err.to_string().contains("link-local"),
                "expected link-local rejection for {url}; got: {err}"
            );
        }
    }

    /// Pin: IPv6 addresses just outside the fe80::/10 prefix still pass.
    /// fec0:: is site-local (deprecated but not link-local).
    #[test]
    fn https_non_link_local_ipv6_passes() {
        assert!(validate_base_url_scheme("p", "https://[fec0::1]/").is_ok());
        assert!(validate_base_url_scheme("p", "https://[2001:db8::1]/").is_ok());
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = validate_base_url_scheme("p", "ftp://example.com").unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn empty_passes() {
        // Some providers have an unset base_url that the factory fills
        // in later (e.g. openai-responses computes a default per
        // auth_kind). The validator MUST NOT reject empty strings.
        assert!(validate_base_url_scheme("p", "").is_ok());
        assert!(validate_base_url_scheme("p", "   ").is_ok());
    }

    #[test]
    fn unparseable_url_rejected() {
        let err = validate_base_url_scheme("p", "not a url at all").unwrap_err();
        assert!(err.to_string().contains("not a valid URL"));
    }
}

#[cfg(test)]
#[cfg(feature = "bedrock")]
mod bedrock_validation_tests {
    use super::*;
    use crate::config::{
        BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig, Config, ProviderEntry,
    };
    use std::collections::BTreeMap;

    fn baseline_betas() -> Vec<String> {
        vec!["context-1m-2025-08-07".into()]
    }

    fn baseline_fields() -> Vec<String> {
        BEDROCK_REQUIRED_BODY_FIELDS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn bedrock_provider_entry() -> ProviderEntry {
        ProviderEntry::Bedrock {
            region: "us-west-2".into(),
            api_shape: BedrockApiShapeConfig::Invoke,
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            runtime: Default::default(),
        }
    }

    fn bedrock_provider_entry_with_floor_beta() -> ProviderEntry {
        let ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            header_extras,
            payload_extras,
            runtime,
            ..
        } = bedrock_provider_entry()
        else {
            unreachable!();
        };
        ProviderEntry::Bedrock {
            region,
            api_shape,
            creds,
            user_agent,
            header_extras,
            payload_extras,
            anthropic_beta: vec!["future-flag-2026-12-31".into()],
            runtime,
        }
    }

    fn config_with(bedrock_provider: bool, global: BedrockGlobalConfig) -> Config {
        let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
        if bedrock_provider {
            providers.insert("primary".into(), bedrock_provider_entry());
        }
        Config {
            providers,
            bedrock: global,
            ..Config::default()
        }
    }

    fn config_with_entry(entry: ProviderEntry, global: BedrockGlobalConfig) -> Config {
        let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
        providers.insert("primary".into(), entry);
        Config {
            providers,
            bedrock: global,
            ..Config::default()
        }
    }

    #[test]
    fn no_bedrock_provider_short_circuits_ok() {
        // Arrange: no providers reference Bedrock; the [bedrock] section
        // is empty (default).
        let cfg = config_with(false, BedrockGlobalConfig::default());

        // Act
        let result = validate_bedrock_global_config(&cfg);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn bedrock_provider_with_empty_allowlists_is_pass_through() {
        // Discovery mode: operator omits the [bedrock] section entirely.
        // Validation passes; the filters run in pass-through mode so the
        // operator can observe traffic via trace logs and build their
        // list from what they see.
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: Vec::new(),
                allowed_body_fields: Vec::new(),
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(result.is_ok(), "expected pass-through Ok, got {result:?}");
    }

    #[test]
    fn bedrock_provider_with_only_allowed_betas_set_is_ok() {
        // Operator chose to gate betas only; body-fields remain in
        // pass-through mode. Validation should accept this -- the
        // empty body-fields list short-circuits the required-keys
        // check.
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: Vec::new(),
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn bedrock_provider_missing_required_body_field_errors() {
        // Arrange: the operator omitted `messages` from their list.
        let mut fields = baseline_fields();
        fields.retain(|s| s != "messages");
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: fields,
            },
        );

        // Act
        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("messages"), "msg: {msg}");
        assert!(msg.contains("routectl-mandatory"), "msg: {msg}");
        assert!(msg.contains("Invoke"), "msg: {msg}");
    }

    #[test]
    fn converse_only_deployment_skips_required_body_field_check() {
        // Arrange: a Converse-only deployment with `allowed_body_fields`
        // that omits `messages`/`anthropic_version`/`max_tokens`. Those
        // keys live at the AWS top level on Converse and never reach
        // `additionalModelRequestFields`, so the missing-required check
        // must NOT fire.
        let cfg = config_with_entry(
            ProviderEntry::Bedrock {
                region: "us-west-2".into(),
                api_shape: BedrockApiShapeConfig::Converse,
                creds: BedrockCredsConfig::DefaultChain,
                user_agent: None,
                header_extras: BTreeMap::new(),
                payload_extras: None,
                anthropic_beta: Vec::new(),
                runtime: Default::default(),
            },
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: vec!["thinking".into(), "anthropic_beta".into()],
            },
        );

        let result = validate_bedrock_global_config(&cfg);

        assert!(
            result.is_ok(),
            "Converse-only deployment should not require Invoke-specific body keys; got {result:?}"
        );
    }

    #[test]
    fn bedrock_provider_floor_beta_requires_anthropic_beta_body_field() {
        let cfg = config_with_entry(
            bedrock_provider_entry_with_floor_beta(),
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: baseline_fields(),
            },
        );

        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("anthropic_beta"), "msg: {msg}");
        assert!(msg.contains("always-send"), "msg: {msg}");
    }

    #[test]
    fn fully_populated_config_is_ok() {
        // Arrange
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: baseline_fields(),
            },
        );

        // Act
        let result = validate_bedrock_global_config(&cfg);

        // Assert
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}

#[cfg(test)]
mod build_resolved_models_tests {
    //! Tests for the v0.6.0 `build_resolved_models` function. Validates
    //! that:
    //!   - Multiple non-Bedrock models referencing the same provider
    //!     share one cached `Arc<dyn Provider>`.
    //!   - Bedrock models each get a distinct `Arc<dyn Provider>` with
    //!     `BedrockConfig.model_id` set from the model's `upstream`.
    //!   - Disabled `[models.X] selectable = false` entries are skipped.
    //!   - Models referencing an unknown provider are reported in the
    //!     `failed` return.

    use super::*;
    use crate::config::{Config, ModelEntry, ProviderEntry};
    use routectl_auth::MemoryStore;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn config_with_models(
        providers: Vec<(&str, ProviderEntry)>,
        models: Vec<(&str, ModelEntry)>,
    ) -> Config {
        let mut p = BTreeMap::new();
        for (name, e) in providers {
            p.insert(name.to_string(), e);
        }
        let mut m = BTreeMap::new();
        for (name, e) in models {
            m.insert(name.to_string(), e);
        }
        Config {
            providers: p,
            models: m,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn non_bedrock_models_share_one_arc_per_provider() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5")),
                ("sonnet", ModelEntry::new("anthropic", "claude-sonnet-4-6")),
            ],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        assert_eq!(models.len(), 2);
        let haiku = models.get("haiku").unwrap();
        let sonnet = models.get("sonnet").unwrap();
        assert!(
            Arc::ptr_eq(&haiku.provider, &sonnet.provider),
            "non-Bedrock models on the same provider must share one Arc"
        );
    }

    #[tokio::test]
    async fn disabled_models_are_skipped() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5")),
                (
                    "shelved",
                    ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
                ),
            ],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty());
        assert!(models.contains_key("haiku"));
        assert!(!models.contains_key("shelved"));
    }

    #[tokio::test]
    async fn unknown_provider_in_model_yields_failed_entry() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(vec![], vec![("orphan", ModelEntry::new("missing", "u"))]);
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(models.is_empty());
        assert_eq!(failed.len(), 1);
        let (nickname, err) = &failed[0];
        assert_eq!(nickname, "orphan");
        assert!(err.contains("unknown provider"), "got: {err}");
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_factory_path_uses_per_model_upstream_for_model_id() {
        // Smoke-level pin: the `build_resolved_models` walk passes
        // each Bedrock model's `upstream` into the BedrockConfig
        // override slot via `build_provider_with_bedrock_model_override`.
        // We can't easily build a BedrockProvider in a unit test
        // (the AWS SDK requires a tokio sleep impl that's awkward
        // to wire up), so this test just sanity-checks that the
        // override-aware factory variant exists and that the wiring
        // compiles. The end-to-end behavior is exercised by the
        // live Bedrock tests in routectl-cli.
        let _f = build_provider_with_bedrock_model_override;
    }

    #[tokio::test]
    async fn header_extras_propagate_from_model_entry_to_resolved() {
        // Pin: v0.6.0 -- per-model `header_extras` lands on
        // ResolvedModel.header_extras after build_resolved_models.
        // Operators now set anthropic-beta via header_extras instead
        // of the dropped Vec<String> field.
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "anthropic-beta".to_string(),
            "context-1m-2025-08-07,prompt-cache-1h".to_string(),
        );
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![(
                "opus",
                ModelEntry::new("anthropic", "claude-opus-4-7").with_header_extras(headers),
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert_eq!(
            opus.header_extras.get("anthropic-beta"),
            Some(&"context-1m-2025-08-07,prompt-cache-1h".to_string())
        );
    }

    #[tokio::test]
    async fn stream_first_byte_timeout_ms_propagates_from_model_entry() {
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![(
                "opus",
                ModelEntry::new("anthropic", "claude-opus-4-7")
                    .with_stream_first_byte_timeout_ms(300_000),
            )],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert_eq!(opus.stream_first_byte_timeout_ms, Some(300_000));
    }

    #[tokio::test]
    async fn empty_header_extras_and_none_timeout_yield_defaults() {
        // Pin: a model entry without the new fields leaves the
        // resolved model with default values (empty maps, None).
        let store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(MemoryStore);
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku-4-5"))],
        );
        let (models, _) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        let haiku = models.get("haiku").expect("haiku entry");
        assert!(haiku.header_extras.is_empty());
        assert!(haiku.payload_extras.is_none());
        assert!(haiku.stream_first_byte_timeout_ms.is_none());
    }
}

#[cfg(test)]
mod validate_alias_chain_targets_tests {
    //! Tests for the v0.6.0 alias-chain validator. Each test pins
    //! one validator branch (clean pass, unknown nickname, disabled
    //! nickname, multi-error accumulation) so a regression in any
    //! one branch shows up as a precise test failure.

    use super::validate_alias_chain_targets;
    use crate::config::{AliasValue, Config, ModelEntry};
    use std::collections::BTreeMap;

    fn config_with(models: Vec<(&str, ModelEntry)>, aliases: Vec<(&str, AliasValue)>) -> Config {
        let mut m = BTreeMap::new();
        for (name, e) in models {
            m.insert(name.to_string(), e);
        }
        let mut a = BTreeMap::new();
        for (name, v) in aliases {
            a.insert(name.to_string(), v);
        }
        Config {
            models: m,
            aliases: a,
            ..Config::default()
        }
    }

    #[test]
    fn validate_alias_chain_targets_passes_clean_config() {
        let cfg = config_with(
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku")),
                ("sonnet", ModelEntry::new("anthropic", "claude-sonnet")),
            ],
            vec![
                ("fast", AliasValue::Single("haiku".into())),
                (
                    "heavy",
                    AliasValue::Chain(vec!["sonnet".into(), "haiku".into()]),
                ),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("clean config must validate");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_unknown_nickname() {
        let cfg = config_with(
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku"))],
            vec![("fast", AliasValue::Single("missing".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("missing"), "msg: {msg}");
        assert!(msg.contains("not a known model nickname"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_disabled_nickname() {
        let cfg = config_with(
            vec![(
                "shelved",
                ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
            )],
            vec![("fast", AliasValue::Single("shelved".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("shelved"), "msg: {msg}");
        assert!(msg.contains("selectable = false"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_rejects_empty_chain() {
        let cfg = config_with(
            vec![("haiku", ModelEntry::new("anthropic", "claude-haiku"))],
            vec![("fast", AliasValue::Chain(vec![]))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `fast`"), "msg: {msg}");
        assert!(msg.contains("empty"), "msg: {msg}");
    }

    #[test]
    fn validate_alias_chain_targets_accumulates_multiple_errors() {
        // Two unrelated misconfigurations -- one alias references an
        // unknown nickname, another references a disabled one. The
        // validator must surface BOTH in a single error so the
        // operator doesn't fix one and discover the other on the
        // next run.
        let cfg = config_with(
            vec![
                ("haiku", ModelEntry::new("anthropic", "claude-haiku")),
                (
                    "shelved",
                    ModelEntry::new("anthropic", "claude-shelved").with_selectable(false),
                ),
            ],
            vec![
                ("alpha", AliasValue::Single("missing-1".into())),
                ("beta", AliasValue::Single("shelved".into())),
                (
                    "gamma",
                    AliasValue::Chain(vec!["haiku".into(), "missing-2".into()]),
                ),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing-1"), "msg: {msg}");
        assert!(msg.contains("missing-2"), "msg: {msg}");
        assert!(msg.contains("shelved"), "msg: {msg}");
    }
}

#[cfg(test)]
mod managed_token_tests {
    //! Pin the v0.7 OAuth-aware `resolve_token_source` semantics:
    //!   - `oauth://` refs return a `ManagedToken` that re-enters
    //!     `SecretStore::get` on every `token()` call (so credentials
    //!     rotation in `~/.config/routectl/credentials.json` is picked
    //!     up live without restart).
    //!   - `env://` / `file://` / `literal:` refs return a `StaticToken`
    //!     resolved once at construction; subsequent `token()` calls
    //!     never re-hit the SecretStore.

    use super::*;
    use async_trait::async_trait;
    use routectl_auth::{SecretRef, SecretStore};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingStore {
        calls: AtomicUsize,
    }
    impl CountingStore {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl SecretStore for CountingStore {
        async fn get(&self, sr: &SecretRef) -> routectl_core::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match sr {
                SecretRef::OAuth { provider } => Ok(format!("token-for-{provider}")),
                SecretRef::Env(_) => Ok("static-canned".to_string()),
                _ => Err(routectl_core::Error::Auth(
                    "counting store: oauth/env-only".into(),
                )),
            }
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn managed_token_re_enters_store_per_call() {
        let counting = Arc::new(CountingStore::new());
        let store: Arc<dyn SecretStore> = counting.clone();
        let ts = resolve_token_source(&store, "oauth://anthropic")
            .await
            .unwrap();
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(ts.token().await.unwrap(), "token-for-anthropic");
        assert_eq!(
            counting.calls(),
            3,
            "ManagedToken must hit store once per token() call"
        );
    }

    #[tokio::test]
    async fn static_token_does_not_re_enter_store_per_call() {
        // CountingStore intercepts `SecretRef::Env(_)` directly and
        // returns a canned reply, so `std::env` is never consulted.
        // The point of the test is to prove the StaticToken path
        // caches: only ONE call lands in the store at construction,
        // and subsequent `token()` invocations reuse the cached value.
        let counting = Arc::new(CountingStore::new());
        let store: Arc<dyn SecretStore> = counting.clone();
        let ts = resolve_token_source(&store, "env://ROUTECTL_TEST_STATIC_TOKEN_VAR")
            .await
            .unwrap();
        let _ = ts.token().await.unwrap();
        let _ = ts.token().await.unwrap();
        assert_eq!(
            counting.calls(),
            1,
            "StaticToken caches; store hit only at construction"
        );
    }
}
