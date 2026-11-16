//! Per-kind provider construction from config rows.

use super::plan::{Destination, ProviderPlan, plan_provider};
#[cfg(feature = "openai-responses")]
use super::validate::validate_openai_responses_account_id;
use super::validate::{ModelProviderTarget, resolve_model_provider, validate_base_url_scheme};
use super::warnings::warn_context_management_needs_preserve;
use crate::config::provider_descriptor::{Auth, Format};
use crate::config::{Config, PoolEntry, ProviderEntry};
use crate::pool_build::{PoolOutcome, PoolPlaceholder, PoolReport};
use crate::resolved::ResolvedModel;
use crate::runtime_state::UnusableCause;
use crate::seat_pool::SeatTarget;
#[cfg(feature = "gemini")]
use routectl_auth::OAuthStoreProjectCache;
use routectl_auth::{CredentialProbeFailure, CredentialRef, SecretRef, SecretScheme, SecretStore};
#[cfg(feature = "gemini")]
use routectl_core::CloudProjectCache;
use routectl_core::wire_check::EgressKind;
use routectl_core::{Provider, Result};
#[cfg(feature = "bedrock")]
use routectl_providers::anthropic_api::MantleAuth;
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
#[cfg(feature = "bedrock")]
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};
#[cfg(feature = "gemini")]
use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
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
    /// Anthropic-cloak tool-array canonicalization switch, sourced from
    /// `[cache] normalize_tools`. Threaded into each anthropic provider's
    /// `CloakConfig` at build time. Default `true`.
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
/// `secrets` and applying `opts`. Plans the entry first, before any
/// credential is read.
#[tracing::instrument(skip_all, fields(provider = %routectl_core::sanitize_for_log(name)))]
pub async fn build_provider_with_options(
    name: &str,
    entry: &ProviderEntry,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<Arc<dyn Provider>> {
    let plan = plan_provider(name, entry)?;
    build_planned_provider(name, entry, &plan, secrets, opts).await
}

/// Build one provider whose plan the caller already computed. The codex
/// identity installed below is process-global and set-once, which is why
/// only a planned entry may reach it.
async fn build_planned_provider(
    name: &str,
    entry: &ProviderEntry,
    plan: &ProviderPlan,
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
        install_entry_codex_version(name, version);
    }

    let ctx = BuildContext {
        name,
        entry,
        plan,
        secrets,
        opts,
    };
    #[cfg(feature = "bedrock")]
    {
        build_provider_inner(ctx, None, None).await
    }
    #[cfg(not(feature = "bedrock"))]
    {
        build_provider_inner(ctx).await
    }
}

#[cfg(feature = "openai-responses")]
fn install_entry_codex_version(name: &str, version: &str) {
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

/// Everything one provider construction reads: the entry, the plan taken
/// over it, the store and the server-wide options.
struct BuildContext<'a> {
    name: &'a str,
    entry: &'a ProviderEntry,
    plan: &'a ProviderPlan,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
}

impl BuildContext<'_> {
    /// The planned base URL, checked for scheme and target before any
    /// credential is attached to it. `None` for the cloud carrier.
    fn checked_base_url(&self) -> Result<Option<&str>> {
        match &self.plan.destination {
            Destination::BaseUrl(base) => {
                validate_base_url_scheme(self.name, base)?;
                Ok(Some(base))
            }
            Destination::Aws { .. } => Ok(None),
        }
    }

    /// The planned AWS region, or `None` for the base-URL carrier.
    #[cfg(feature = "bedrock")]
    fn aws_region(&self) -> Option<&str> {
        match &self.plan.destination {
            Destination::Aws { region } => Some(region),
            Destination::BaseUrl(_) => None,
        }
    }

    fn header_extras(&self) -> Vec<(String, String)> {
        self.entry
            .header_extras
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The store-backed token source for the planned credential.
    async fn token_source(&self) -> Result<Arc<dyn routectl_core::TokenSource>> {
        let secret_ref = self.plan.secret_ref().ok_or_else(|| {
            routectl_core::Error::Config(format!(
                "provider `{}`: this lane needs an env://, file:// or oauth:// credential",
                self.name
            ))
        })?;
        resolve_token_source(&self.secrets, secret_ref.clone()).await
    }
}

/// Variant that sets the Bedrock upstream model id on `BedrockConfig`
/// from the `[models.X]` entry, so each Bedrock-targeting model entry
/// gets its own provider instance. Other formats ignore it; one cached
/// `Arc<dyn Provider>` per `[providers.X]` is shared across all
/// `[models]` referencing it.
///
/// `cached_auth` lets the caller skip the per-model SSO probe by reusing
/// creds resolved once at the parent provider level. When `None`, the
/// factory resolves creds from the plan (one resolve per call).
#[cfg(feature = "bedrock")]
async fn build_provider_with_bedrock_model_override(
    name: &str,
    entry: &ProviderEntry,
    plan: &ProviderPlan,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
    bedrock_model_id: Option<String>,
    cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    let ctx = BuildContext {
        name,
        entry,
        plan,
        secrets,
        opts,
    };
    build_provider_inner(ctx, bedrock_model_id, cached_auth).await
}

#[tracing::instrument(skip_all, fields(provider = %routectl_core::sanitize_for_log(ctx.name)))]
async fn build_provider_inner(
    ctx: BuildContext<'_>,
    #[cfg(feature = "bedrock")] bedrock_model_id: Option<String>,
    #[cfg(feature = "bedrock")] cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    match ctx.entry.format() {
        Format::AnthropicMessages => {
            let cfg = anthropic_api_config(&ctx).await?;
            Ok(Arc::new(AnthropicApiProvider::new(cfg)))
        }
        Format::OpenaiChat => openai_compat_provider(&ctx).await,
        #[cfg(feature = "openai-responses")]
        Format::OpenaiResponses => openai_responses_provider(&ctx).await,
        #[cfg(feature = "bedrock")]
        Format::BedrockInvoke | Format::BedrockConverse => {
            bedrock_provider(&ctx, bedrock_model_id, cached_auth).await
        }
        #[cfg(feature = "gemini")]
        Format::Gemini | Format::GeminiCloudCode => gemini_provider(&ctx).await,
        #[allow(unreachable_patterns)]
        format => Err(routectl_core::Error::Config(format!(
            "provider `{}`: format = \"{}\" is not available in this build",
            ctx.name,
            format.as_str()
        ))),
    }
}

/// The AWS credential for a cloud-carrier entry: `aws://profile/<name>` and
/// `aws://chain` select the SDK's own loaders, and an env:// / file:// key
/// sent as a bearer becomes a Bedrock API key. The plan already refused any
/// other pairing.
#[cfg(feature = "bedrock")]
async fn bedrock_creds_for(plan: &ProviderPlan, secrets: &dyn SecretStore) -> Result<BedrockCreds> {
    match &plan.credential {
        CredentialRef::AwsProfile(name) => Ok(BedrockCreds::Profile { name: name.clone() }),
        CredentialRef::AwsChain => Ok(BedrockCreds::DefaultChain),
        CredentialRef::Secret(secret_ref) => Ok(BedrockCreds::BearerKey {
            key: secrets.get(secret_ref).await?,
        }),
        _ => Err(routectl_core::Error::Config(
            "an AWS cloud entry needs an aws:// credential or a bearer key reference".into(),
        )),
    }
}

/// Resolve a cloud-carrier entry's AWS credential and auth handle once.
/// `build_resolved_models` calls it, after the plan, to prime the
/// per-provider auth cache before dispatching N model builds against it.
#[cfg(feature = "bedrock")]
async fn resolve_bedrock_auth_for_entry(
    plan: &ProviderPlan,
    secrets: &dyn SecretStore,
) -> Result<CachedBedrockAuth> {
    let Destination::Aws { region } = &plan.destination else {
        return Err(routectl_core::Error::Config(
            "resolve_bedrock_auth_for_entry called on a base_url provider entry".into(),
        ));
    };
    let bedrock_creds = bedrock_creds_for(plan, secrets).await?;
    let resolved = routectl_providers::bedrock::auth::resolve(&bedrock_creds, region).await?;
    Ok(CachedBedrockAuth {
        creds: bedrock_creds,
        resolved,
    })
}

/// The managed-surface signing material for a cloud-carrier entry.
#[cfg(feature = "bedrock")]
async fn mantle_auth(ctx: &BuildContext<'_>, region: &str) -> Result<MantleAuth> {
    let creds = bedrock_creds_for(ctx.plan, &*ctx.secrets).await?;
    let resolved = routectl_providers::bedrock::auth::resolve(&creds, region).await?;
    Ok(MantleAuth {
        region: region.to_string(),
        creds: resolved,
    })
}

/// An `openai-chat` provider: the configured base URL with the credential
/// as a bearer, or Bedrock's managed surface derived from `cloud.region`
/// with the AWS credential signing the request.
async fn openai_compat_provider(ctx: &BuildContext<'_>) -> Result<Arc<dyn Provider>> {
    let mut cfg = OpenAiCompatConfig {
        id: format!("openai-compat:{}", ctx.name),
        base_url: String::new(),
        auth: Arc::new(routectl_core::StaticToken::new(String::new())),
        header_extras: ctx.header_extras(),
        payload_extras: None,
        reasoning_dialect: Default::default(),
        history_reasoning: Default::default(),
        user_agent: ctx.entry.user_agent.clone(),
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    if let Some(base) = ctx.checked_base_url()? {
        cfg.base_url = base.to_string();
        cfg.auth = ctx.token_source().await?;
        return Ok(Arc::new(OpenAiCompatProvider::new(cfg)));
    }
    #[cfg(feature = "bedrock")]
    if let Some(region) = ctx.aws_region() {
        cfg.base_url = routectl_providers::mantle::mantle_openai_base(region)?;
        cfg.mantle = Some(mantle_auth(ctx, region).await?);
        return Ok(Arc::new(OpenAiCompatProvider::new(cfg)));
    }
    Err(cloud_unavailable(ctx.name))
}

fn cloud_unavailable(name: &str) -> routectl_core::Error {
    routectl_core::Error::Config(format!(
        "provider `{name}`: the `cloud` carrier is not available in this build"
    ))
}

/// The Responses wire dialect an `openai-responses` entry speaks: the ChatGPT
/// Codex dialect on the Codex host, the Bedrock managed dialect on the AWS
/// cloud, the OpenAI API dialect anywhere else.
#[cfg(feature = "openai-responses")]
fn responses_dialect(ctx: &BuildContext<'_>) -> OpenaiResponsesAuthKind {
    match &ctx.plan.destination {
        Destination::Aws { .. } => OpenaiResponsesAuthKind::BedrockMantle,
        Destination::BaseUrl(_) => super::validate::responses_base_url_dialect(ctx.entry),
    }
}

/// An `openai-responses` provider.
#[cfg(feature = "openai-responses")]
async fn openai_responses_provider(ctx: &BuildContext<'_>) -> Result<Arc<dyn Provider>> {
    let dialect = responses_dialect(ctx);
    let Some(base) = ctx.checked_base_url()? else {
        return openai_responses_cloud_provider(ctx).await;
    };
    let bearer_is_oauth = matches!(ctx.plan.secret_ref(), Some(SecretRef::OAuth { .. }));
    validate_openai_responses_account_id(
        ctx.name,
        dialect,
        bearer_is_oauth,
        &ctx.entry.account_id_ref,
    )?;
    let auth = ctx.token_source().await?;
    let account_id = resolve_responses_account_id(ctx).await?;
    // The codex session and installation ids are part of the codex client
    // identity, so only a provider the plan grants that identity reads (or
    // mints) them. The credential already carries the seat label, so
    // `peek_session_id` resolves THIS seat's value. An installation-id
    // read/write failure yields None + a WARN, never a build failure.
    let client_identity = ctx.plan.client_identity;
    let session_id = match (client_identity, ctx.plan.secret_ref()) {
        (true, Some(sr)) => ctx.secrets.peek_session_id(sr).await,
        _ => None,
    };
    let installation_id = if client_identity {
        super::installation_id::resolve_installation_id(ctx.opts.mint_installation_id)
    } else {
        None
    };
    let mut cfg =
        OpenAiResponsesConfig::new_with_auth(format!("openai-responses:{}", ctx.name), auth);
    cfg.account_id = account_id;
    cfg.base_url = base.to_string();
    cfg.auth_kind = dialect;
    cfg.header_extras = ctx.header_extras();
    cfg.user_agent = ctx.entry.user_agent.clone();
    cfg.session_id = session_id;
    cfg.installation_id = installation_id;
    cfg.client_identity = client_identity;
    Ok(Arc::new(OpenAiResponsesProvider::new(cfg)))
}

/// An `openai-responses` provider on Bedrock's managed surface: the endpoint
/// derives from `cloud.region` and the AWS credential signs the request; no
/// first-party bearer is presented.
#[cfg(feature = "openai-responses")]
#[cfg_attr(not(feature = "bedrock"), allow(clippy::unused_async))]
async fn openai_responses_cloud_provider(ctx: &BuildContext<'_>) -> Result<Arc<dyn Provider>> {
    #[cfg(feature = "bedrock")]
    if let Some(region) = ctx.aws_region() {
        let mut cfg = OpenAiResponsesConfig::new_with_auth(
            format!("openai-responses:{}", ctx.name),
            Arc::new(routectl_core::StaticToken::new(String::new())),
        );
        cfg.base_url = routectl_providers::mantle::mantle_openai_base(region)?;
        cfg.auth_kind = OpenaiResponsesAuthKind::BedrockMantle;
        cfg.header_extras = ctx.header_extras();
        cfg.user_agent = ctx.entry.user_agent.clone();
        cfg.mantle = Some(mantle_auth(ctx, region).await?);
        return Ok(Arc::new(OpenAiResponsesProvider::new(cfg)));
    }
    Err(cloud_unavailable(ctx.name))
}

/// A native Bedrock provider (`bedrock-invoke` / `bedrock-converse`), which
/// speaks SigV4 or a Bedrock API key to the runtime endpoint of
/// `cloud.region`.
#[cfg(feature = "bedrock")]
async fn bedrock_provider(
    ctx: &BuildContext<'_>,
    bedrock_model_id: Option<String>,
    cached_auth: Option<CachedBedrockAuth>,
) -> Result<Arc<dyn Provider>> {
    let region = ctx.aws_region().ok_or_else(|| {
        routectl_core::Error::Config(format!(
            "provider `{}`: the bedrock formats need `cloud`",
            ctx.name
        ))
    })?;
    let cached = match cached_auth {
        Some(cached) => cached,
        None => resolve_bedrock_auth_for_entry(ctx.plan, &*ctx.secrets).await?,
    };
    let api_shape = match ctx.entry.format() {
        Format::BedrockConverse => BedrockApiShape::Converse,
        _ => BedrockApiShape::Invoke,
    };
    let cfg = BedrockConfig {
        id: format!("bedrock:{}", ctx.name),
        region: region.to_string(),
        model_id: bedrock_model_id.unwrap_or_default(),
        api_shape,
        creds: cached.creds,
        user_agent: ctx.entry.user_agent.clone(),
        header_extras: ctx.header_extras(),
    };
    Ok(Arc::new(BedrockProvider::new(cfg, cached.resolved)?))
}

/// A Gemini provider: the public REST surface with an API key, or the Cloud
/// Code surface with the managed Antigravity login the plan required.
#[cfg(feature = "gemini")]
async fn gemini_provider(ctx: &BuildContext<'_>) -> Result<Arc<dyn Provider>> {
    let base = ctx
        .checked_base_url()?
        .ok_or_else(|| cloud_unavailable(ctx.name))?
        .to_string();
    let auth = ctx.token_source().await?;
    let mut cfg = if ctx.entry.format() == Format::GeminiCloudCode {
        let secret_ref = ctx.plan.secret_ref().cloned().ok_or_else(|| {
            routectl_core::Error::Config(format!(
                "provider `{}`: gemini-cloud-code needs an oauth://antigravity credential",
                ctx.name
            ))
        })?;
        let project_cache: Arc<dyn CloudProjectCache> =
            Arc::new(OAuthStoreProjectCache::new(ctx.secrets.clone(), secret_ref));
        let mut cfg =
            GeminiConfig::new_cloud_code(format!("gemini:{}", ctx.name), auth, project_cache);
        // An empty string is an operator leaving the knob blank, not a
        // project id: it must not shadow discovery.
        cfg.cloud_project_id = ctx
            .entry
            .cloud_project_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        cfg
    } else {
        GeminiConfig::new_with_auth(format!("gemini:{}", ctx.name), auth)
    };
    cfg.base_url = base;
    cfg.header_extras = ctx.header_extras();
    if let Some(ua) = &ctx.entry.user_agent {
        cfg.user_agent = Some(ua.clone());
    }
    Ok(Arc::new(GeminiProvider::new(cfg)))
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
/// the operator's declared order -- the order `fill_first_order` walks and
/// the sticky selection falls back to.
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
        // The credential probe below reads the store, so the entry's plan
        // must hold first; the same plan is passed to the build.
        let plan = match plan_provider(member, entry) {
            Ok(plan) => plan,
            Err(_) => {
                refusals.push((member, plan_refusal(entry)));
                continue;
            }
        };
        let provider_kind = entry.kind_str();
        let secret_ref = match member_secret_ref(&plan) {
            Ok(sr) => sr,
            Err(refusal) => {
                refusals.push((member, refusal));
                continue;
            }
        };
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
        match build_planned_provider(member, entry, &plan, secrets.clone(), opts.clone()).await {
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
                "is refused by its provider plan (an incoherent entry, or a managed \
                 credential outside the lane that credential is accepted on)"
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

/// The pool rule a refused plan maps to, by the shape of the entry's
/// credential: absent, unparseable, or otherwise refused by the plan.
fn plan_refusal(entry: &ProviderEntry) -> MemberRefusal {
    if entry.credential.trim().is_empty() {
        MemberRefusal::MissingRef
    } else if routectl_auth::CredentialRef::parse(&entry.credential).is_err() {
        MemberRefusal::MalformedRef
    } else {
        MemberRefusal::OutsideContainment
    }
}

/// The store reference of one pool member's planned credential, or the
/// refusal its absence earns. A member whose credential no store resolves
/// (an AWS form, `forwarded`) cannot authenticate a pool account.
fn member_secret_ref(plan: &ProviderPlan) -> std::result::Result<SecretRef, MemberRefusal> {
    plan.secret_ref().cloned().ok_or(MemberRefusal::MissingRef)
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

/// Build every member of every pool a model routes at, exactly as
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

/// The pools a buildable model names: the set
/// [`build_resolved_models_reported`] compiles.
fn routed_pools(config: &Config) -> BTreeMap<&str, &PoolEntry> {
    config
        .models
        .iter()
        .filter(|(nickname, _)| {
            crate::seat_pool::check_state_key_name("model nickname", nickname).is_ok()
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
    secret_ref: SecretRef,
) -> Result<Arc<dyn routectl_core::TokenSource>> {
    tracing::debug!(
        secret_scheme = SecretScheme::from(&secret_ref).as_prefix(),
        "resolving token source"
    );
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
async fn resolve_responses_account_id(ctx: &BuildContext<'_>) -> Result<Option<String>> {
    if let Some(uri) = &ctx.entry.account_id_ref {
        tracing::debug!(secret_scheme = scheme_of(uri), "resolving account id ref");
        let secret_ref = SecretRef::parse(uri)?;
        return Ok(Some(ctx.secrets.get(&secret_ref).await?));
    }
    let Some(parsed @ SecretRef::OAuth { provider, .. }) = ctx.plan.secret_ref() else {
        return Ok(None);
    };
    match ctx.secrets.account_id(parsed).await? {
        Some(id) => Ok(Some(id)),
        None => Err(routectl_core::Error::Config(format!(
            "openai-responses provider `{}`: no ChatGPT account id found for \
             `oauth://{provider}`. Run `routectl login {provider}` first, or set \
             `account_id_ref` explicitly.",
            ctx.name
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
    /// One sanitized report per pool that at least one model named, in pool-name order.
    pub pool_reports: Vec<PoolReport>,
}

/// [`build_resolved_models`] with the per-pool build reports retained.
///
/// Fails with `Error::Config` when any pool that a model names has
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

        if let Some(prior_err) = provider_failed.get(&entry.provider) {
            // Provider already failed on a sibling model; reuse the error
            // and keep walking, so neither the plan nor any credential
            // resolution runs twice for one [providers.X] entry.
            failed.push((nickname.clone(), prior_err.clone()));
            continue;
        }
        // Every store read and AWS credential resolution below sits behind
        // the entry's plan, computed once per provider and passed down.
        let plan = match plan_provider(&entry.provider, provider_entry) {
            Ok(plan) => plan,
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    provider = %routectl_core::sanitize_for_log(&entry.provider),
                    model = %routectl_core::sanitize_for_log(nickname),
                    error = %msg,
                    "skipping provider (refused by its plan)",
                );
                provider_failed.insert(entry.provider.clone(), msg.clone());
                failed.push((nickname.clone(), msg));
                continue;
            }
        };

        // Native Bedrock: one Arc per model. The factory pumps the model's
        // upstream into BedrockConfig.model_id.
        let is_bedrock = matches!(
            plan.egress,
            EgressKind::BedrockInvoke | EgressKind::BedrockConverse
        );

        let provider = if is_bedrock {
            #[cfg(feature = "bedrock")]
            {
                // Cache the resolved creds per provider name so the SSO
                // probe / aws-config chain build only fires once per
                // [providers.X] entry, regardless of how many [models.X]
                // reference it.
                let cached = match bedrock_auth_cache.get(&entry.provider) {
                    Some(c) => c.clone(),
                    None => match resolve_bedrock_auth_for_entry(&plan, &*secrets).await {
                        Ok(c) => {
                            bedrock_auth_cache.insert(entry.provider.clone(), c.clone());
                            c
                        }
                        Err(e) => {
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

                match build_provider_with_bedrock_model_override(
                    &entry.provider,
                    provider_entry,
                    &plan,
                    secrets.clone(),
                    opts.clone(),
                    Some(entry.upstream.clone()),
                    Some(cached),
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
                unreachable!("the plan has no Bedrock route without the bedrock feature");
            }
        } else if let Some(cached) = provider_cache.get(&entry.provider) {
            cached.clone()
        } else {
            match build_planned_provider(
                &entry.provider,
                provider_entry,
                &plan,
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
        if let Some(sr) = plan.secret_ref() {
            resolved = resolved.with_auth_secret_ref(sr.clone());
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
    if let Some(tokens) = entry.max_output_tokens {
        resolved = resolved.with_max_output_tokens(tokens);
    }
    if let Some(tokens) = entry.context_window {
        resolved = resolved.with_context_window(tokens);
    }
    resolved
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
/// model named. A pool no model routes at is not reported: it was
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

/// The provider config for an `anthropic-messages` entry. The plan's
/// client-identity verdict is passed through unchanged, and the login
/// session id is read only when that verdict holds.
async fn anthropic_api_config(ctx: &BuildContext<'_>) -> Result<AnthropicApiConfig> {
    let entry = ctx.entry;
    let mut cfg = AnthropicApiConfig {
        id: format!("anthropic-api:{}", ctx.name),
        auth: Arc::new(routectl_core::StaticToken::new(String::new())),
        base_url: String::new(),
        anthropic_version: entry
            .anthropic_version
            .clone()
            .unwrap_or_else(|| crate::config::DEFAULT_ANTHROPIC_VERSION.to_string()),
        auth_kind: AuthKind::ApiKey,
        header_extras: ctx.header_extras(),
        user_agent: entry.user_agent.clone(),
        forward_client_headers: entry.forward_client_headers.clone().unwrap_or_default(),
        context_management: entry.context_management.unwrap_or(false),
        session_id: None,
        cloak: entry_cloak(entry, ctx.opts.normalize_tools),
        use_forwarded_bearer: false,
        client_identity: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let Some(base) = ctx.checked_base_url()? else {
        #[cfg(feature = "bedrock")]
        return anthropic_cloud_config(ctx, cfg).await;
        #[cfg(not(feature = "bedrock"))]
        return Err(cloud_unavailable(ctx.name));
    };
    cfg.base_url = base.to_string();
    cfg.auth_kind = match entry.auth {
        Auth::Bearer => AuthKind::OauthBearer,
        _ => AuthKind::ApiKey,
    };
    // `forwarded` relays the client's captured bearer; the plan pinned it to
    // the Anthropic host with the bearer form. No configured secret exists to
    // resolve, so the token source is an empty sentinel that the forwarded
    // leg never reads: `resolve_effective_token` takes the captured bearer,
    // and the router refuses a forwarded target with no captured bearer
    // before this provider is dispatched to.
    if matches!(ctx.plan.credential, CredentialRef::Forwarded) {
        cfg.use_forwarded_bearer = true;
        return Ok(cfg);
    }
    cfg.auth = ctx.token_source().await?;
    // The stable per-credential Claude Code session id is read only for a
    // provider that presents the Claude Code identity. The credential
    // already carries the seat label, so `peek_session_id` resolves THIS
    // seat's session id.
    if ctx.plan.client_identity
        && let Some(sr) = ctx.plan.secret_ref()
    {
        cfg.session_id = ctx.secrets.peek_session_id(sr).await;
    }
    cfg.client_identity = ctx.plan.client_identity;
    Ok(cfg)
}

/// [`anthropic_api_config`] over a context built from its parts.
#[cfg(test)]
pub(super) async fn anthropic_api_config_for(
    name: &str,
    entry: &ProviderEntry,
    plan: &ProviderPlan,
    secrets: Arc<dyn SecretStore>,
    opts: BuildOptions,
) -> Result<AnthropicApiConfig> {
    anthropic_api_config(&BuildContext {
        name,
        entry,
        plan,
        secrets,
        opts,
    })
    .await
}

/// An `anthropic-messages` config on Bedrock's managed surface: the endpoint
/// derives from `cloud.region`, the AWS credential signs the request, and no
/// first-party token or Claude Code identity is presented.
#[cfg(feature = "bedrock")]
async fn anthropic_cloud_config(
    ctx: &BuildContext<'_>,
    mut cfg: AnthropicApiConfig,
) -> Result<AnthropicApiConfig> {
    let region = ctx
        .aws_region()
        .ok_or_else(|| cloud_unavailable(ctx.name))?;
    cfg.base_url = routectl_providers::mantle::mantle_anthropic_base(region)?;
    cfg.mantle = Some(mantle_auth(ctx, region).await?);
    Ok(cfg)
}

/// The entry's cloak settings, with the global `[cache] normalize_tools`
/// switch stamped on. `normalize_tools` is not a provider key; it reaches
/// the cloak only through this build-time stamp, so the single operator
/// control stays under `[cache]`.
fn entry_cloak(entry: &ProviderEntry, normalize_tools: bool) -> CloakConfig {
    let mut cloak = entry.effective_cloak();
    cloak.normalize_tools = normalize_tools;
    cloak
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
#[cfg(feature = "openai-responses")]
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
