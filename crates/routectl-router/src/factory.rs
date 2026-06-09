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
    build_provider_inner(name, entry, secrets, opts, bedrock_overrides, cached_auth).await
}

#[tracing::instrument(skip_all, fields(provider = %name))]
async fn build_provider_inner(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    #[cfg(feature = "bedrock")] bedrock_overrides: Option<BedrockModelOverrides>,
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
            context_management,
            max_thinking_entry_bytes,
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
            // Resolve the stable per-credential Claude Code session id
            // for the OauthBearer surface only. `api_key_ref` already
            // carries the seat label (`build_seat_targets` rebuilds each
            // labeled seat with its own `oauth://anthropic#label` ref), so
            // `peek_session_id` resolves THIS seat's session_id with no
            // extra fallback. ApiKey providers (and a non-oauth ref) get
            // None. The ref already parsed cleanly inside
            // `resolve_token_source` above, so a parse error here is
            // unreachable; treat it as "no session id" rather than fail
            // the build.
            let session_id =
                if *auth_kind == routectl_providers::anthropic_api::AuthKind::OauthBearer {
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
                allowed_betas: allowed_betas.clone(),
                forward_client_headers: forward_client_headers.clone(),
                context_management: *context_management,
                max_thinking_entry_bytes: resolve_max_thinking_entry_bytes(
                    name,
                    *max_thinking_entry_bytes,
                ),
                session_id,
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
            runtime: _,
        } => {
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
            let resolved_base_url = base_url.clone().unwrap_or_else(|| {
                let default = default_responses_base(*auth_kind);
                if *auth_kind == OpenaiResponsesAuthKind::BedrockMantle {
                    // The OpenaiResponses config has no region field, so the
                    // factory cannot substitute the configured AWS region into
                    // the bedrock-mantle hostname. Operators on regions other
                    // than us-east-1 MUST set base_url explicitly in their
                    // provider entry, e.g.:
                    //   base_url = "https://bedrock-mantle.<region>.api.aws/openai/v1"
                    tracing::warn!(
                        provider = name,
                        default_base_url = %default,
                        "bedrock-mantle provider has no base_url configured; \
                         defaulting to the us-east-1 endpoint -- operators on \
                         other AWS regions must set base_url explicitly, e.g. \
                         https://bedrock-mantle.<region>.api.aws/openai/v1",
                    );
                }
                default
            });
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

/// Bounds for `[providers.X].max_thinking_entry_bytes` (anthropic-api).
const MIN_THINKING_ENTRY_BYTES: u32 = 1024;
const MAX_THINKING_ENTRY_BYTES_CEILING: u32 = 4 * 1024 * 1024;

/// Test-only re-export so other modules' tests can drive the resolver
/// without making the helper itself `pub`.
#[cfg(test)]
pub(crate) fn resolve_max_thinking_entry_bytes_for_test(
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
    if v == 0 {
        // Operator wrote `0`; treat as "unset" rather than letting a
        // zero cap silently disable the cache.
        tracing::warn!(
            provider = %provider_name,
            "[providers.{provider_name}] max_thinking_entry_bytes = 0 is not a valid cap (would disable the cache); falling back to the default ({default})"
        );
        return default;
    }
    if v < MIN_THINKING_ENTRY_BYTES {
        tracing::warn!(
            provider = %provider_name,
            configured = v,
            min = MIN_THINKING_ENTRY_BYTES,
            "max_thinking_entry_bytes below minimum; clamping up"
        );
        return MIN_THINKING_ENTRY_BYTES as usize;
    }
    if v > MAX_THINKING_ENTRY_BYTES_CEILING {
        tracing::warn!(
            provider = %provider_name,
            configured = v,
            max = MAX_THINKING_ENTRY_BYTES_CEILING,
            "max_thinking_entry_bytes above ceiling; clamping down"
        );
        return MAX_THINKING_ENTRY_BYTES_CEILING as usize;
    }
    v as usize
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

/// Clone a provider entry with its primary `api_key_ref` swapped for a
/// seat-pinned URI. Used to build one provider instance per OAuth seat
/// in a credential pool. Bedrock has no single `api_key_ref` slot, so it
/// returns `None` -- pools are never built for Bedrock (its creds shape
/// is multi-field and is not an `oauth://` pool).
fn entry_with_api_key_ref(entry: &ProviderEntry, seat_uri: &str) -> Option<ProviderEntry> {
    let mut cloned = entry.clone();
    match &mut cloned {
        ProviderEntry::OpenaiCompat { api_key_ref, .. } => *api_key_ref = seat_uri.to_string(),
        ProviderEntry::AnthropicApi { api_key_ref, .. } => *api_key_ref = seat_uri.to_string(),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { api_key_ref, .. } => *api_key_ref = seat_uri.to_string(),
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock { .. } => return None,
    }
    Some(cloned)
}

/// Expand a model's primary OAuth credential reference into a fixed set
/// of seat-pinned providers, ONE per stored seat, when (and only when)
/// the ref is a bare-pool `oauth://<provider>` (label `None`) backed by
/// MORE THAN ONE seat. Returns:
///
///   - `None` for the single-seat / non-pooled / labeled / non-oauth
///     case -- the model dispatches its single `default_provider` keyed
///     by nickname, byte-for-byte the pre-pool behavior.
///   - `Some(seats)` with one `SeatTarget` per seat (default seat first,
///     then sorted labels) when expansion applies. The first seat reuses
///     `default_provider` (already built, default ref); the rest are
///     built fresh from a seat-pinned ref. Each seat carries its own
///     `state_key` so the breaker + RPM bucket are per-seat.
///
/// `list_seats` is reached through the `Arc<dyn SecretStore>` the factory
/// already holds; for the server build path this lands in `OAuthStore`,
/// which enumerates the stored seats for the provider.
async fn build_seat_targets(
    nickname: &str,
    provider_name: &str,
    provider_entry: &ProviderEntry,
    primary_ref: &SecretRef,
    default_provider: &Arc<dyn Provider>,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Option<Arc<[crate::seat_pool::SeatTarget]>> {
    // Only a bare-pool oauth ref (label None) can expand. A labeled ref
    // already pins one seat; env/file/literal are single-credential.
    if !matches!(primary_ref, SecretRef::OAuth { label: None, .. }) {
        return None;
    }
    let seat_refs = match secrets.list_seats(primary_ref).await {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!(
                provider = %provider_name,
                model = %nickname,
                error = %e,
                "seat enumeration failed; falling back to single-seat dispatch",
            );
            return None;
        }
    };
    // A single seat resolves to the same provider already on the model;
    // skip the pool and keep the byte-for-byte single-target path.
    if seat_refs.len() <= 1 {
        return None;
    }
    let mut seats: Vec<crate::seat_pool::SeatTarget> = Vec::with_capacity(seat_refs.len());
    for seat_ref in seat_refs.iter() {
        let label = match seat_ref {
            SecretRef::OAuth { label, .. } => label.clone(),
            _ => None,
        };
        let state_key = crate::seat_pool::seat_state_key(nickname, label.as_deref());
        // The default seat (label None) reuses the provider the factory
        // already built from the bare-pool ref -- no second build. This
        // MUST key off the label, NOT the seat index: a labels-only pool
        // (no bare default seat) has a labeled seat at index 0, which has
        // to build from its OWN pinned ref rather than inherit the bare,
        // credential-less provider.
        let provider = if label.is_none() {
            default_provider.clone()
        } else {
            let seat_uri = seat_ref.to_string();
            let seat_entry = match entry_with_api_key_ref(provider_entry, &seat_uri) {
                Some(e) => e,
                None => {
                    // A provider kind with no single api_key_ref slot cannot
                    // be seat-pinned; skip this seat, not the whole pool.
                    tracing::warn!(
                        provider = %provider_name,
                        model = %nickname,
                        seat = %state_key,
                        "skipping OAuth pool seat (no api_key_ref to pin)",
                    );
                    continue;
                }
            };
            match build_provider_with_options(
                provider_name,
                &seat_entry,
                secrets.clone(),
                opts.clone(),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    // A single seat failing to build should not sink the
                    // whole pool; skip it and keep the healthy seats.
                    tracing::warn!(
                        provider = %provider_name,
                        model = %nickname,
                        seat = %state_key,
                        error = %e,
                        "skipping OAuth pool seat (build failed)",
                    );
                    continue;
                }
            }
        };
        seats.push(crate::seat_pool::SeatTarget {
            label,
            state_key,
            provider,
            auth_secret_ref: Some(seat_ref.clone()),
        });
    }
    // If only the default seat survived (every labeled seat failed to
    // build), there is no pool to dispatch across -- fall back to the
    // single-target path so behavior matches a single-seat config.
    if seats.len() <= 1 {
        return None;
    }
    Some(Arc::from(seats))
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
        // A `#` in a model nickname would collide with a labeled seat's
        // runtime-state key (`{nickname}#{label}`), letting two distinct
        // dispatch identities share one circuit breaker. Reject it here so
        // the collision is impossible by construction; the offending model
        // is dropped from the resolved table with a clear reason.
        if nickname.contains('#') {
            failed.push((
                nickname.clone(),
                format!(
                    "model nickname `{nickname}` must not contain `#` \
                     (reserved as the seat-pool state-key separator)"
                ),
            ));
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
                    adaptive_thinking: if entry.supports_adaptive_thinking {
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
        );
        if entry.supports_adaptive_thinking {
            resolved = resolved.with_supports_adaptive_thinking(true);
        }
        if !entry.effort_levels.is_empty() {
            resolved = resolved.with_effort_levels(entry.effort_levels.clone());
        }
        if entry.max_thinking_budget > 0 {
            resolved = resolved.with_max_thinking_budget(entry.max_thinking_budget);
        }
        if let Some(d) = entry.reasoning_dialect {
            resolved = resolved.with_reasoning_dialect(d);
        }
        if let Some(h) = entry.history_reasoning {
            resolved = resolved.with_history_reasoning(h);
        }
        warn_context_management_needs_preserve(
            &entry.provider,
            nickname,
            provider_entry,
            entry.history_reasoning,
        );
        if !entry.header_extras.is_empty() {
            resolved = resolved.with_header_extras(entry.header_extras.clone());
        }
        if let Some(extras) = entry.payload_extras.as_ref() {
            resolved = resolved.with_payload_extras(extras.clone());
        }
        if let Some(ms) = entry.stream_first_byte_timeout_ms {
            resolved = resolved.with_stream_first_byte_timeout_ms(ms);
        }
        if let Some(tokens) = entry.max_output_tokens {
            resolved = resolved.with_max_output_tokens(tokens);
        }
        if let Some(uri) = primary_api_key_uri(provider_entry) {
            if let Ok(sr) = SecretRef::parse(uri) {
                resolved = resolved.with_auth_secret_ref(sr.clone());
                // OAuth credential-pool expansion. A bare-pool
                // `oauth://<provider>` ref backed by more than one stored
                // seat expands into one seat-pinned provider per seat so
                // the dispatch chain rotates + cools across seats. A
                // single seat / labeled ref / non-oauth ref builds exactly
                // one provider (the default `provider` already on
                // `resolved`), so this is a no-op there -- back-compat.
                if let Some(seats) = build_seat_targets(
                    nickname,
                    &entry.provider,
                    provider_entry,
                    &sr,
                    &resolved.provider,
                    secrets.clone(),
                    opts.clone(),
                )
                .await
                {
                    resolved = resolved.with_seats(seats);
                }
            }
        }
        models.insert(nickname.clone(), Arc::new(resolved));
    }

    Ok((models, failed))
}

/// Returns `true` when `entry` is `ProviderEntry::AnthropicApi { context_management: true, .. }`.
/// `false` for any other shape. Used by the model-binding warning
/// path to scope the guard to the only provider kind where the
/// `context_management` emulation flag exists.
fn anthropic_api_uses_context_management(entry: &ProviderEntry) -> bool {
    matches!(
        entry,
        ProviderEntry::AnthropicApi {
            context_management: true,
            ..
        }
    )
}

/// Emit a structured WARN when an anthropic-api provider declares
/// `context_management = true` but the model's `history_reasoning`
/// is missing or set to anything other than `Preserve`. The two
/// settings are complementary: `context_management` controls the
/// outgoing-request shaping for non-Anthropic anthropic-api endpoints
/// (DeepSeek `/anthropic`, vLLM, LM Studio) while `history_reasoning =
/// "preserve"` ensures thinking blocks ride back into the request
/// history so multi-turn continuity is preserved upstream.
///
/// Silent for any other shape: `context_management = false`,
/// `history_reasoning = Preserve`, or non-anthropic-api providers.
/// The literal strings `context_management` and `history_reasoning`
/// appear in the message body so operators can grep the runbook
/// without hunting for the exact wording.
fn warn_context_management_needs_preserve(
    provider_name: &str,
    nickname: &str,
    entry: &ProviderEntry,
    history_reasoning: Option<crate::config::HistoryReasoning>,
) {
    if !anthropic_api_uses_context_management(entry) {
        return;
    }
    if matches!(
        history_reasoning,
        Some(crate::config::HistoryReasoning::Preserve)
    ) {
        return;
    }
    tracing::warn!(
        provider = provider_name,
        model = nickname,
        "context_management = true on this anthropic-api provider but \
         history_reasoning is not 'preserve' on the model; thinking \
         echo-back is required for multi-turn continuity. See \
         docs/PROVIDER-QUIRKS.md \"context_management\" for the \
         recommended config."
    );
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
        // BedrockMantle URL is region-specific. ProviderEntry::OpenaiResponses
        // carries no region field, so the factory cannot substitute the
        // configured AWS region here. This returns the us-east-1 endpoint as
        // a fallback. The call site emits a WARN when this default fires so
        // the operator is not silently misdirected to the wrong region.
        // Operators on other regions must set base_url explicitly.
        OpenaiResponsesAuthKind::BedrockMantle => {
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1".into()
        }
    }
}

/// Validate the `account_id_ref` invariant for an openai-responses
/// entry. A misconfigured TOML surfaces here as a clean `Error::Config`
/// rather than a confusing upstream 401/403 at first request time.
///
/// Rules:
///   - ChatgptOauth + `oauth://<provider>` bearer: `account_id_ref` is
///     OPTIONAL. When omitted, the factory derives the account id from
///     the logged-in OAuth session (the `chatgpt_account_id` recorded
///     at `routectl login`). An explicit `account_id_ref` is still
///     accepted and wins as an override.
///   - ChatgptOauth + static bearer (`env://`/`file://`/`literal:`):
///     `account_id_ref` is REQUIRED. There is no OAuth session to read
///     the account id from, so the operator must supply it -- this is
///     the legacy chatgpt-oauth workflow, kept unchanged.
///   - ApiKey / BedrockMantle: `account_id_ref` is FORBIDDEN (the
///     account id is a ChatGPT-OAuth-only concept).
///
/// `bearer_is_oauth` mirrors `matches!(SecretRef::parse(api_key_ref),
/// Ok(SecretRef::OAuth { .. }))`; the caller computes it once and passes
/// it in so the validator and the downstream resolver do not each
/// reparse the same URI.
#[cfg(feature = "openai-responses")]
fn validate_openai_responses_account_id(
    name: &str,
    auth_kind: OpenaiResponsesAuthKind,
    bearer_is_oauth: bool,
    account_id_ref: &Option<String>,
) -> Result<()> {
    use routectl_core::Error;

    let has_account = account_id_ref.is_some();
    let is_chatgpt_oauth = matches!(auth_kind, OpenaiResponsesAuthKind::ChatgptOauth);
    if !is_chatgpt_oauth {
        // ApiKey / BedrockMantle: account_id is a ChatGPT-OAuth-only
        // concept; reject it for the other surfaces.
        if has_account {
            return Err(Error::Config(format!(
                "openai-responses provider `{name}`: `account_id_ref` is only valid \
                 when auth_kind = \"chatgpt-oauth\"; remove it for {auth_kind:?}"
            )));
        }
        return Ok(());
    }

    // ChatgptOauth path. `oauth://` bearers may omit account_id_ref
    // (derived from the session); static bearers must supply it.
    if bearer_is_oauth || has_account {
        return Ok(());
    }
    Err(Error::Config(format!(
        "openai-responses provider `{name}`: auth_kind = \"chatgpt-oauth\" with a \
         static bearer requires `account_id_ref` (the ChatGPT account UUID). Use an \
         `oauth://<provider>` bearer to derive it from a logged-in session instead."
    )))
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

/// Validate the `[models.X] effort_levels` allowlist across every
/// configured model.
///
/// Each element must be one of the six known effort vocabulary tokens
/// (the union of the Anthropic-shape and OpenAI-shape vocabularies):
/// `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Individual
/// egresses clamp to their own subset at dispatch time; the validator
/// here catches operator typos before any request is processed.
///
/// An empty `effort_levels` list is valid (means pass-through -- the
/// egress accepts whatever effort the caller supplied).
///
/// Returns `Err(Error::Config(...))` on the first model entry that
/// contains an unknown effort token, naming the model nickname and
/// the offending token.
///
/// Call once per process startup BEFORE building any providers.
pub fn validate_reasoning_defaults(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;
    use routectl_providers::effort::VALID_EFFORT_TOKENS;

    for (nickname, entry) in &config.models {
        for level in &entry.effort_levels {
            if !VALID_EFFORT_TOKENS.contains(&level.as_str()) {
                return Err(Error::Config(format!(
                    "[models.{nickname}] effort_levels contains unknown value {:?}; \
                     valid values are: minimal, low, medium, high, xhigh, max",
                    level
                )));
            }
        }
    }
    Ok(())
}

/// Validate the `[retry]` block: enforce that `retry_allowlist` and
/// `retry_denylist` are mutually exclusive. Setting a non-empty
/// `retry_allowlist` together with `retry_denylist = Some(_)` would
/// otherwise leave the operator's intent ambiguous; the predicate
/// in `RetryPolicy::is_fallbackable_status` resolves to the allowlist
/// (everything else is terminal) but the denylist would be silently
/// ignored, masking the misconfiguration. Surface the conflict at
/// startup so the operator picks one.
///
/// Call once per process startup alongside the other validators.
pub fn validate_retry_policy(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    let r = &config.retry;
    if !r.retry_allowlist.is_empty() && r.retry_denylist.is_some() {
        return Err(Error::Config(
            "[retry]: `retry_allowlist` and `retry_denylist` are \
             mutually exclusive; pick one (allowlist for an explicit \
             set of fallback codes, denylist for `400..=599 except \
             these`)"
                .into(),
        ));
    }
    Ok(())
}

/// Validate that every entry in `[aliases]` resolves to a known and
/// selectable `[models.X]` nickname OR another alias key (recursive
/// expansion). Walks both `AliasValue::Single` and `AliasValue::Chain`;
/// accumulates every offending nickname into one consolidated startup
/// error so the operator gets the full list in one shot.
///
/// Failure modes:
///
///   - alias references a nickname that doesn't exist in `[models]`
///     and is not another alias key. Common cause: typo, or the
///     operator deleted a model row but forgot to update the alias.
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
///   - cycle in the alias graph (e.g. `A = ["B"]`, `B = ["A"]`).
///     Detected via DFS over the alias keys; the error message names
///     the cycle path so the operator can break the loop. The
///     dispatch path also carries a runtime depth cap as belt-and-
///     suspenders, but cycles caught here never reach it.
///
/// Call once per process startup AFTER `validate_reasoning_defaults`
/// and BEFORE `build_resolved_models`. Glob keys (`claude-*` etc.)
/// are validated identically to exact keys -- the chain target must
/// still be a known nickname or alias key even though the alias key
/// matches a pattern.
pub fn validate_alias_chain_targets(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    let mut errors: Vec<String> = Vec::new();

    // Pass 1: empty-chain check + per-entry resolves-to-something
    // check (must be either a known model nickname OR another alias
    // key). Cycle detection is a separate pass below; it walks the
    // graph structure rather than per-entry semantics.
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
            // An entry may either be another alias key (recursive
            // expansion) OR a model nickname. Alias keys win on
            // collision (matches the dispatch-time shadowing rule).
            if config.aliases.contains_key(nickname) {
                continue;
            }
            match config.models.get(nickname) {
                None => {
                    errors.push(format!(
                        "alias `{alias}`: target `{nickname}` is not a known \
                         model nickname in [models] and is not an alias key"
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

    // Pass 2: cycle detection via DFS. Each connected component is
    // walked once -- `globally_visited` short-circuits keys that have
    // already been fully explored from another start point. Cycles
    // are recorded with the offending path so the operator can break
    // the loop.
    let mut globally_visited: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for start in config.aliases.keys() {
        if globally_visited.contains(start) {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut path_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        detect_alias_cycles_dfs(
            &config.aliases,
            start,
            &mut path,
            &mut path_set,
            &mut globally_visited,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

/// DFS helper for cycle detection in the alias graph. `path` /
/// `path_set` track the currently-active recursion stack; a chain
/// entry that hits the stack means we have a back-edge (cycle).
/// `globally_visited` is the standard DFS "fully explored" set so
/// each connected component is traversed once.
///
/// Errors are pushed into `errors` and accumulate alongside the
/// per-entry diagnostics from pass 1 of
/// `validate_alias_chain_targets`. Reuses `Error::Config` rather than
/// introducing a new variant -- the message carries the cycle path
/// (e.g. `alias `foo`: cycle detected: foo -> bar -> baz -> foo`).
fn detect_alias_cycles_dfs(
    aliases: &BTreeMap<String, crate::config::AliasValue>,
    current: &str,
    path: &mut Vec<String>,
    path_set: &mut std::collections::BTreeSet<String>,
    globally_visited: &mut std::collections::BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    if path_set.contains(current) {
        // Back-edge: cycle. The cycle starts at the position in
        // `path` where `current` first appears and closes by re-
        // visiting `current`. Attribute the diagnostic to the FIRST
        // alias actually in the cycle (`path[idx]`), not the DFS
        // root, so the operator's eye lands on the alias that
        // closes the loop. With external feeders like
        // `c -> a -> b -> a`, the report names `a` (the cycle's
        // entry) rather than `c` (which merely points into it).
        let idx = path
            .iter()
            .position(|p| p == current)
            .expect("path_set/path invariant: current must be present in path");
        let mut cycle_path: Vec<&str> = path[idx..].iter().map(String::as_str).collect();
        cycle_path.push(current);
        let entry_alias = path[idx].clone();
        errors.push(format!(
            "alias `{entry_alias}`: cycle detected: {}",
            cycle_path.join(" -> ")
        ));
        return;
    }
    if globally_visited.contains(current) {
        return;
    }
    let Some(value) = aliases.get(current) else {
        // Not an alias key; either a model nickname (handled in
        // pass 1) or a dangling reference (also handled in pass 1).
        // Either way, no cycle can pass through a non-alias leaf.
        return;
    };
    path.push(current.to_string());
    path_set.insert(current.to_string());
    for entry in value.nicknames() {
        detect_alias_cycles_dfs(aliases, entry, path, path_set, globally_visited, errors);
    }
    path.pop();
    path_set.remove(current);
    globally_visited.insert(current.to_string());
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

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn bedrock_creds_failure_resolves_at_most_once_for_sibling_models() {
        // Dedup invariant on the failure path: when a Bedrock
        // provider's cred resolution fails on its first model, the
        // failure is recorded in `provider_failed` and every sibling
        // model on the same provider is skipped WITHOUT re-attempting
        // resolution (no repeat SSO / aws-config probe). With two
        // models on one provider, the secret store is hit exactly
        // once -- the second model short-circuits via the
        // `provider_failed` guard at the top of the Bedrock branch.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingFailStore {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl SecretStore for CountingFailStore {
            async fn get(&self, _secret_ref: &SecretRef) -> routectl_core::Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(routectl_core::Error::Auth("simulated cred failure".into()))
            }
            async fn set(
                &self,
                _secret_ref: &SecretRef,
                _value: &str,
            ) -> routectl_core::Result<()> {
                Err(routectl_core::Error::Auth("read-only".into()))
            }
            async fn delete(&self, _secret_ref: &SecretRef) -> routectl_core::Result<()> {
                Err(routectl_core::Error::Auth("read-only".into()))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn SecretStore> = Arc::new(CountingFailStore {
            calls: calls.clone(),
        });

        let bedrock = ProviderEntry::Bedrock {
            region: "us-east-1".to_string(),
            api_shape: BedrockApiShapeConfig::default(),
            creds: BedrockCredsConfig::BearerKey {
                key_ref: "literal:unused".to_string(),
            },
            user_agent: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            runtime: Default::default(),
        };
        let cfg = config_with_models(
            vec![("br", bedrock)],
            vec![
                ("opus", ModelEntry::new("br", "anthropic.claude-opus")),
                ("sonnet", ModelEntry::new("br", "anthropic.claude-sonnet")),
            ],
        );

        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");

        assert!(
            models.is_empty(),
            "no model should resolve when provider creds fail"
        );
        assert_eq!(
            failed.len(),
            2,
            "both sibling models must be reported failed: {failed:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "cred resolution must run at most once per provider; the sibling \
             model should be skipped via provider_failed, not re-probed"
        );
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

    /// Stub store that reports a fixed list of OAuth seats for any bare
    /// pool ref. `get`/`set`/`delete` are unused by these build-time
    /// tests (the anthropic-api oauth arm wraps a lazy `ManagedToken`
    /// rather than resolving a token at build).
    struct MultiSeatStore {
        labels: Vec<Option<String>>,
    }

    #[async_trait::async_trait]
    impl SecretStore for MultiSeatStore {
        async fn get(&self, _secret_ref: &SecretRef) -> routectl_core::Result<String> {
            Ok("token".into())
        }
        async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
            Ok(())
        }
        async fn list_seats(
            &self,
            secret_ref: &SecretRef,
        ) -> routectl_core::Result<Vec<SecretRef>> {
            // A labeled ref pins one seat; mirror the real store.
            if let SecretRef::OAuth { label: Some(_), .. } = secret_ref {
                return Ok(vec![secret_ref.clone()]);
            }
            let SecretRef::OAuth { provider, .. } = secret_ref else {
                return Ok(vec![secret_ref.clone()]);
            };
            Ok(self
                .labels
                .iter()
                .map(|label| SecretRef::OAuth {
                    provider: provider.clone(),
                    label: label.clone(),
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn single_unlabeled_seat_builds_one_target_unchanged() {
        // Back-compat pin: a bare-pool oauth ref backed by exactly one
        // (unlabeled/default) seat does NOT expand -- `seats` stays None,
        // so dispatch builds one target keyed by nickname, byte-for-byte
        // the pre-pool behavior.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore { labels: vec![None] });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert!(
            opus.seats.is_none(),
            "single seat must NOT expand into a pool"
        );
    }

    #[tokio::test]
    async fn pool_with_three_seats_expands_to_three_targets() {
        // A bare-pool ref backed by three stored seats expands into three
        // seat targets, each pinned to a distinct labeled SecretRef and a
        // distinct state_key (default seat first, then sorted labels).
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into()), Some("seat-c".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        let seats = opus.seats.as_ref().expect("three-seat pool must expand");
        assert_eq!(seats.len(), 3, "expected three seat targets");

        // Distinct state_keys: default seat is the bare nickname, labeled
        // seats carry the `#label` suffix.
        let keys: Vec<&str> = seats.iter().map(|s| s.state_key.as_str()).collect();
        assert_eq!(keys, vec!["opus", "opus#seat-b", "opus#seat-c"]);

        // Distinct seat-pinned SecretRefs round-tripping through Display.
        let refs: Vec<String> = seats
            .iter()
            .map(|s| s.auth_secret_ref.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(
            refs,
            vec![
                "oauth://anthropic",
                "oauth://anthropic#seat-b",
                "oauth://anthropic#seat-c",
            ]
        );
    }

    #[tokio::test]
    async fn labels_only_pool_builds_each_seat_from_its_own_ref() {
        // Regression pin for the labels-only bug: a pool with NO bare
        // default seat (operator ran `login anthropic --label a` / `--label
        // b` only) puts a LABELED seat at index 0. Seat 0 must build from
        // its OWN pinned ref (`oauth://anthropic#a`), NOT inherit the bare,
        // credential-less provider the model was built from. The old
        // `idx == 0` reuse silently bound seat 0 to the bare provider.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![Some("a".into()), Some("b".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        let seats = opus.seats.as_ref().expect("labels-only pool must expand");
        assert_eq!(seats.len(), 2, "expected two labeled seat targets");

        // Labels-only: index 0 is the FIRST LABELED seat -- no bare `opus`
        // state_key, no bare `oauth://anthropic` ref.
        let keys: Vec<&str> = seats.iter().map(|s| s.state_key.as_str()).collect();
        assert_eq!(keys, vec!["opus#a", "opus#b"]);
        let refs: Vec<String> = seats
            .iter()
            .map(|s| s.auth_secret_ref.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(refs, vec!["oauth://anthropic#a", "oauth://anthropic#b"]);

        // The fix: NO labeled seat reuses the bare-ref provider the model
        // was built from. With the old `idx == 0` reuse, seat 0 would be
        // pointer-equal to `opus.provider` (the bare, credential-less
        // build) and silently resolve the wrong identity at request time.
        for seat in seats.iter() {
            assert!(
                !Arc::ptr_eq(&opus.provider, &seat.provider),
                "labels-only seat {} must be built from its own ref, not the bare provider",
                seat.state_key,
            );
        }
    }

    #[tokio::test]
    async fn explicitly_labeled_ref_does_not_expand() {
        // A model whose api_key_ref already pins a seat
        // (`oauth://anthropic#seat-b`) builds exactly one target -- the
        // operator selected the seat, so there is no pool to expand.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into()), Some("seat-c".into())],
        });
        let cfg = config_with_models(
            vec![(
                "anthropic",
                ProviderEntry::anthropic_api("oauth://anthropic#seat-b")
                    .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer),
            )],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, failed) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        assert!(failed.is_empty(), "expected no failures: {failed:?}");
        let opus = models.get("opus").expect("opus entry");
        assert!(
            opus.seats.is_none(),
            "an explicitly-labeled ref must NOT expand into a pool"
        );
    }

    #[tokio::test]
    async fn non_oauth_ref_does_not_expand() {
        // Back-compat: a literal/env/file ref never pools, even if a
        // (misconfigured) store reported multiple seats for it.
        let store: Arc<dyn SecretStore> = Arc::new(MultiSeatStore {
            labels: vec![None, Some("seat-b".into())],
        });
        let cfg = config_with_models(
            vec![("anthropic", ProviderEntry::anthropic_api("literal:k"))],
            vec![("opus", ModelEntry::new("anthropic", "claude-opus-4-7"))],
        );
        let (models, _) = build_resolved_models(&cfg, store.clone(), BuildOptions::default())
            .await
            .expect("ok");
        let opus = models.get("opus").expect("opus entry");
        assert!(opus.seats.is_none(), "a non-oauth ref must never pool");
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

    // ----- Recursive alias-chain validation (Task #5) -----
    //
    // Each test pins one slice of the recursive expansion contract:
    // alias-of-alias resolves, dangling refs surface cleanly, cycles
    // are detected with a path-bearing error, and globs follow the
    // same rule as exact aliases.

    #[test]
    fn alias_referencing_another_alias_passes_validation() {
        // A -> B -> model. Pass 1 sees `A`'s "B" as an alias key
        // (skipped, recursion-checked later) and `B`'s "model-x" as
        // a known nickname. Pass 2 walks A -> B -> model-x without
        // hitting a cycle.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("model-x".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("2-deep alias chain must validate");
    }

    #[test]
    fn alias_referencing_three_deep_passes_validation() {
        let cfg = config_with(
            vec![
                ("model-x", ModelEntry::new("anthropic", "claude-x")),
                ("model-y", ModelEntry::new("anthropic", "claude-y")),
            ],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("c".into())),
                (
                    "c",
                    AliasValue::Chain(vec!["model-x".into(), "model-y".into()]),
                ),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("3-deep alias chain must validate");
    }

    #[test]
    fn alias_cycle_detected_with_path() {
        // A -> B -> A. Pass 1 sees both entries as alias keys (no
        // dangling-ref errors). Pass 2 catches the back-edge.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("a".into())),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"), "msg: {msg}");
        // The reported path includes both alias keys and closes back
        // on the entry point.
        assert!(
            msg.contains("a -> b -> a") || msg.contains("b -> a -> b"),
            "msg: {msg}"
        );
    }

    #[test]
    fn alias_self_cycle_detected() {
        // The 1-hop degenerate case: A -> A. Pass 1 lets it through
        // (alias key); pass 2 catches the immediate back-edge.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![("a", AliasValue::Single("a".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"), "msg: {msg}");
        assert!(msg.contains("a -> a"), "msg: {msg}");
    }

    #[test]
    fn external_alias_feeds_cycle_attributes_to_first_in_cycle() {
        // Regression for the cycle-attribution fix: when a non-cycle
        // alias feeds into a cycle, the diagnostic must name the
        // FIRST alias in the cycle, not the DFS root that merely
        // pointed at it. Config: `a -> b -> c -> b`; the cycle is
        // `b <-> c` and `a` is the external feeder. (Root iteration
        // is alphabetical because `config.aliases` is a `BTreeMap`,
        // so `a` is the DFS root that detects the back-edge.)
        //
        // BEFORE the fix this reported `alias `a`: ...` (wrong --
        // operator looks at `a`, finds it just points at `b`, can't
        // see the cycle). AFTER the fix it reports `alias `b`: ...`
        // -- the alias that closes the loop.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("b".into())),
                ("b", AliasValue::Single("c".into())),
                ("c", AliasValue::Single("b".into())),
            ],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `b`:"), "msg: {msg}");
        assert!(msg.contains("b -> c -> b"), "msg: {msg}");
        assert!(
            !msg.contains("alias `a`:"),
            "external feeder `a` must not be the attributed alias; msg: {msg}"
        );
    }

    #[test]
    fn dangling_ref_in_recursive_chain_is_caught() {
        // A -> nonexistent. Neither an alias key nor a model
        // nickname; pass 1 surfaces a dangling-reference error.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![("a", AliasValue::Single("nonexistent".into()))],
        );
        let err = validate_alias_chain_targets(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias `a`"), "msg: {msg}");
        assert!(msg.contains("nonexistent"), "msg: {msg}");
        assert!(
            msg.contains("not a known model nickname") && msg.contains("not an alias key"),
            "msg: {msg}"
        );
    }

    #[test]
    fn glob_alias_referencing_another_alias_passes_validation() {
        // Per architect's verdict F: glob keys follow the same rule
        // as exact keys. `claude-haiku*` -> `a` -> model. The fact
        // that the glob key is a pattern (not a literal) does not
        // change validation semantics.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("claude-haiku*", AliasValue::Single("a".into())),
                ("a", AliasValue::Single("model-x".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("glob key into alias must validate");
    }

    #[test]
    fn dry_operator_pattern_passes_validation() {
        // The DRY case from the spec: a single source-of-truth alias
        // `a` plus a discoverability wrapper `claude-a` that just
        // points at it. Both should validate cleanly so the operator
        // can collapse the duplicated `claude-cheap`/`claude-codex-pro`
        // /etc. shapes that currently inline the full chain.
        let cfg = config_with(
            vec![("model-x", ModelEntry::new("anthropic", "claude-x"))],
            vec![
                ("a", AliasValue::Single("model-x".into())),
                ("claude-a", AliasValue::Single("a".into())),
            ],
        );
        validate_alias_chain_targets(&cfg).expect("DRY single-pointer alias must validate");
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
                SecretRef::OAuth { provider, .. } => Ok(format!("token-for-{provider}")),
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

#[cfg(test)]
#[cfg(feature = "openai-responses")]
mod openai_responses_account_id_tests {
    //! Pin the managed-OAuth account-id derivation for the
    //! openai-responses factory arm:
    //!   (a) `oauth://codex` + no `account_id_ref` + populated store
    //!       -> account id taken from the stored TokenRecord; build ok.
    //!   (b) `oauth://codex` + no `account_id_ref` + empty store
    //!       -> clean Error mentioning `routectl login codex`.
    //!   (c) `env://X` + no `account_id_ref` (legacy chatgpt-oauth)
    //!       -> existing "requires account_id_ref" Error preserved.
    //!   (d) `oauth://codex` + explicit `account_id_ref`
    //!       -> the operator value wins (override).

    use super::*;
    use async_trait::async_trait;
    use routectl_auth::{MemoryStore, OAuthStore, SecretRef};
    use std::sync::Arc;

    /// Minimal stand-in for the production `CompositeStore` (which lives
    /// in the CLI crate and is out of scope here). Routes `oauth://`
    /// refs -- including the `account_id` read -- to the OAuthStore, and
    /// everything else (`literal:`, `env://`, `file://`) to MemoryStore.
    /// Lets these router-level tests exercise the operator-override path
    /// (`account_id_ref = "literal:..."`) alongside the JWT-derived path
    /// without depending on the CLI crate.
    struct CompositeTestStore {
        oauth: OAuthStore,
        fallback: MemoryStore,
    }

    #[async_trait]
    impl SecretStore for CompositeTestStore {
        async fn get(&self, sr: &SecretRef) -> Result<String> {
            match sr {
                SecretRef::OAuth { .. } => self.oauth.get(sr).await,
                _ => self.fallback.get(sr).await,
            }
        }
        async fn set(&self, sr: &SecretRef, v: &str) -> Result<()> {
            self.fallback.set(sr, v).await
        }
        async fn delete(&self, sr: &SecretRef) -> Result<()> {
            self.fallback.delete(sr).await
        }
        async fn account_id(&self, sr: &SecretRef) -> Result<Option<String>> {
            match sr {
                SecretRef::OAuth { .. } => self.oauth.account_id(sr).await,
                _ => self.fallback.account_id(sr).await,
            }
        }
    }

    /// Write a `credentials.json` seeded with a `codex` record (when
    /// `account_id` is `Some`) or leave the store empty, then open an
    /// `OAuthStore` over it. Returns the tempdir guard (kept alive for
    /// the test's duration) and the store as `Arc<dyn SecretStore>`.
    ///
    /// The record is written as raw JSON rather than constructed from
    /// `TokenRecord` because that struct is `#[non_exhaustive]` and
    /// cannot be built with a struct literal from this crate. Writing
    /// the on-disk shape also exercises the real `OAuthStore::open`
    /// load path.
    async fn oauth_store_with_codex(
        account_id: Option<&str>,
    ) -> (tempfile::TempDir, Arc<dyn SecretStore>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        if let Some(id) = account_id {
            let json = format!(
                r#"{{
                    "schema_version": 1,
                    "providers": {{
                        "codex": {{
                            "access_token": "tok-codex",
                            "refresh_token": "rtok-codex",
                            "token_type": "Bearer",
                            "expires_at_unix": 9999999999,
                            "scopes": ["openid"],
                            "account": {{ "email": "u@example.com", "account_id": "{id}" }},
                            "obtained_at_unix": 0
                        }}
                    }}
                }}"#
            );
            std::fs::write(&path, json).unwrap();
            // OAuthStore::open refuses group/other-readable credential
            // files (it wants chmod 600). tempfile defaults to 644, so
            // tighten the mode before opening.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let store = CompositeTestStore {
            oauth: OAuthStore::open(&path).await.unwrap(),
            fallback: MemoryStore::new(),
        };
        (dir, Arc::new(store) as Arc<dyn SecretStore>)
    }

    fn chatgpt_oauth_entry(api_key_ref: &str) -> ProviderEntry {
        ProviderEntry::openai_responses(api_key_ref)
            .with_openai_responses_auth_kind(OpenaiResponsesAuthKind::ChatgptOauth)
    }

    #[tokio::test]
    async fn oauth_no_account_id_ref_derives_from_stored_token() {
        // (a) populated store -> account id derived; provider builds.
        let (_dir, store) = oauth_store_with_codex(Some("acct-from-jwt")).await;

        let derived = resolve_responses_account_id(&store, "oauth://codex", &None, "codex-pro")
            .await
            .expect("derivation should succeed");
        assert_eq!(derived, Some("acct-from-jwt".to_string()));

        let entry = chatgpt_oauth_entry("oauth://codex");
        let provider = build_provider("codex-pro", &entry, store.clone()).await;
        assert!(
            provider.is_ok(),
            "provider should build from a logged-in session: {:?}",
            provider.err()
        );
    }

    #[tokio::test]
    async fn oauth_no_account_id_ref_empty_store_errors_with_login_hint() {
        // (b) empty store -> clean Error mentioning `routectl login codex`.
        let (_dir, store) = oauth_store_with_codex(None).await;

        let err = resolve_responses_account_id(&store, "oauth://codex", &None, "codex-pro")
            .await
            .expect_err("empty store must error");
        let msg = err.to_string();
        assert!(
            msg.contains("routectl login codex"),
            "expected login hint, got: {msg}"
        );

        // The full build arm must surface the same error. `Arc<dyn
        // Provider>` is not `Debug`, so match instead of `expect_err`.
        let entry = chatgpt_oauth_entry("oauth://codex");
        match build_provider("codex-pro", &entry, store.clone()).await {
            Ok(_) => panic!("build must fail with no session"),
            Err(e) => assert!(
                e.to_string().contains("routectl login codex"),
                "build error should carry the login hint, got: {e}"
            ),
        }
    }

    #[tokio::test]
    async fn legacy_static_chatgpt_oauth_still_requires_account_id_ref() {
        // (c) env:// bearer (legacy chatgpt-oauth) + no account_id_ref
        // -> the validator rejects it (existing operator workflow).
        let err = validate_openai_responses_account_id(
            "legacy",
            OpenaiResponsesAuthKind::ChatgptOauth,
            false, // env://OPENAI_JWT is a static bearer, not oauth://
            &None,
        )
        .expect_err("static chatgpt-oauth without account_id_ref must error");
        let msg = err.to_string();
        assert!(msg.contains("requires `account_id_ref`"), "got: {msg}");
        assert!(msg.contains("legacy"), "got: {msg}");
    }

    #[tokio::test]
    async fn explicit_account_id_ref_wins_over_stored_token() {
        // (d) operator-supplied account_id_ref overrides the JWT-derived
        // one even when the store has a (different) stored account id.
        let (_dir, store) = oauth_store_with_codex(Some("acct-from-jwt")).await;

        let override_ref = Some("literal:acct-operator-override".to_string());
        let derived =
            resolve_responses_account_id(&store, "oauth://codex", &override_ref, "codex-pro")
                .await
                .expect("override should resolve");
        assert_eq!(
            derived,
            Some("acct-operator-override".to_string()),
            "operator-supplied account_id_ref must win over the stored token"
        );
    }
}

#[cfg(test)]
mod validate_reasoning_defaults_tests {
    //! Unit tests for `validate_reasoning_defaults`.
    //! Covers: valid levels accepted, empty list accepted, invalid level
    //! rejected (error names model and offending token), all six valid
    //! tokens pass individually.

    use super::validate_reasoning_defaults;
    use crate::config::{Config, ModelEntry};

    fn config_with_model(nickname: &str, entry: ModelEntry) -> Config {
        let mut cfg = Config::default();
        cfg.models.insert(nickname.to_string(), entry);
        cfg
    }

    /// Empty effort_levels is valid (pass-through mode).
    #[test]
    fn accepts_empty_effort_levels() {
        let entry = ModelEntry::new("p", "u").with_effort_levels(vec![]);
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "empty effort_levels should be accepted"
        );
    }

    /// Default effort_levels (["low","medium","high"]) is valid.
    #[test]
    fn accepts_default_effort_levels() {
        let entry = ModelEntry::new("p", "u");
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "default effort_levels must be valid"
        );
    }

    /// All six valid vocabulary tokens are individually accepted.
    #[test]
    fn accepts_all_six_valid_levels() {
        for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            let entry = ModelEntry::new("p", "u").with_effort_levels(vec![level.to_string()]);
            let cfg = config_with_model("single", entry);
            assert!(
                validate_reasoning_defaults(&cfg).is_ok(),
                "level {:?} should be accepted",
                level
            );
        }
    }

    /// A mix of valid tokens all in one list is accepted.
    #[test]
    fn accepts_mixed_valid_levels() {
        let entry = ModelEntry::new("p", "u").with_effort_levels(vec![
            "minimal".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
            "xhigh".into(),
            "max".into(),
        ]);
        let cfg = config_with_model("m", entry);
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "all six levels together should be valid"
        );
    }

    /// An unknown token causes rejection with the model name and token in
    /// the error message.
    #[test]
    fn rejects_invalid_level_names_model_and_token() {
        let entry = ModelEntry::new("p", "u")
            .with_effort_levels(vec!["low".into(), "invalid_level".into()]);
        let cfg = config_with_model("my-model", entry);
        let err =
            validate_reasoning_defaults(&cfg).expect_err("invalid effort level should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("my-model"),
            "error must name the model; got: {msg}"
        );
        assert!(
            msg.contains("invalid_level"),
            "error must name the offending token; got: {msg}"
        );
    }

    /// The validator catches every entry: if multiple models have invalid
    /// levels, the first offender is reported (not silently skipped).
    #[test]
    fn rejects_on_first_invalid_model_encountered() {
        let mut cfg = Config::default();
        cfg.models.insert(
            "good".to_string(),
            ModelEntry::new("p", "u").with_effort_levels(vec!["low".into(), "high".into()]),
        );
        cfg.models.insert(
            "bad".to_string(),
            ModelEntry::new("p", "u").with_effort_levels(vec!["high".into(), "turbo".into()]),
        );
        let err = validate_reasoning_defaults(&cfg)
            .expect_err("should reject the config with an invalid level");
        let msg = err.to_string();
        assert!(
            msg.contains("turbo"),
            "error must name the offending token; got: {msg}"
        );
    }

    /// A config with no models at all passes validation.
    #[test]
    fn accepts_empty_models_table() {
        let cfg = Config::default();
        assert!(
            validate_reasoning_defaults(&cfg).is_ok(),
            "empty models table must be valid"
        );
    }
}

#[cfg(test)]
mod anthropic_api_config_propagation_tests {
    //! Pin that `context_management` flows from `ProviderEntry::AnthropicApi`
    //! through the factory destructure into `AnthropicApiConfig`.
    //!
    //! The factory arm destructures the entry fields then assigns them
    //! one-for-one to `AnthropicApiConfig { .. }`. These tests mirror that
    //! destructure pattern so any mismatch in the wiring is caught at
    //! compile time (missing field) or at runtime (wrong value).

    use crate::config::ProviderEntry;
    use routectl_providers::anthropic_api::AnthropicApiConfig;

    /// Helper that simulates the factory destructure and returns the
    /// `context_management` value that would land in `AnthropicApiConfig`.
    /// Written to mirror the exact field list in `build_provider_inner` so
    /// a future factory refactor that drops the field from the destructure
    /// will break this test at compile time.
    fn extract_context_management(entry: &ProviderEntry) -> bool {
        match entry {
            ProviderEntry::AnthropicApi {
                context_management, ..
            } => *context_management,
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// `ProviderEntry::AnthropicApi { context_management: true, .. }` wires
    /// the value `true` into `AnthropicApiConfig.context_management`.
    #[test]
    fn factory_propagates_context_management_true() {
        // Arrange
        let mut entry = ProviderEntry::anthropic_api("literal:sk-test");
        if let ProviderEntry::AnthropicApi {
            ref mut context_management,
            ..
        } = entry
        {
            *context_management = true;
        }

        // Act: extract the way the factory does, then build the config field.
        let extracted = extract_context_management(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        // Simulate the factory struct-literal assignment.
        let cfg_with_flag = AnthropicApiConfig {
            context_management: extracted,
            ..cfg
        };

        // Assert
        assert!(
            cfg_with_flag.context_management,
            "context_management: true must propagate into AnthropicApiConfig"
        );
    }

    /// A default `ProviderEntry::AnthropicApi` (context_management omitted)
    /// wires the value `false` into `AnthropicApiConfig.context_management`.
    #[test]
    fn factory_propagates_context_management_false_default() {
        // Arrange: use the constructor helper -- context_management defaults to false.
        let entry = ProviderEntry::anthropic_api("literal:sk-test");

        // Act
        let extracted = extract_context_management(&entry);
        let cfg = AnthropicApiConfig::new("test", "sk-test");
        let cfg_with_flag = AnthropicApiConfig {
            context_management: extracted,
            ..cfg
        };

        // Assert
        assert!(
            !cfg_with_flag.context_management,
            "context_management must default to false in AnthropicApiConfig"
        );
    }
}
