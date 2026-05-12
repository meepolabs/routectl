//! Build concrete `Provider` instances from `ProviderEntry` config rows.
//! Resolves secret references via a `SecretStore` at build time so the
//! provider can hold the plaintext API key it needs.

use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Provider, Result};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_compat::{
    HistoryReasoning as ProviderHistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider,
    ReasoningDialect as ProviderDialect,
};

#[cfg(feature = "bedrock")]
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};

#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::{
    AuthKind as OpenaiResponsesAuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
};

use crate::config::HistoryReasoning;
#[cfg(feature = "bedrock")]
use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
use crate::config::{ProviderEntry, ReasoningDialect};

pub async fn build_provider(
    name: &str,
    entry: &ProviderEntry,
    secrets: &dyn SecretStore,
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
    /// Optional override for the Bedrock-Invoke `anthropic_beta`
    /// allowlist. Sourced from the top-level `[bedrock] anthropic_beta`
    /// TOML field. When `Some`, replaces the routectl-shipped const
    /// `BEDROCK_INVOKE_ACCEPTED_BETAS`. When `None`, the const wins.
    /// Applies to every Bedrock provider in the config (Bedrock's
    /// allowlist is global, not per-model).
    pub bedrock_anthropic_beta_allowlist: Option<Vec<String>>,
}

impl BuildOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_strict_translation(mut self, strict: bool) -> Self {
        self.strict_translation = strict;
        self
    }

    pub fn with_bedrock_anthropic_beta_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.bedrock_anthropic_beta_allowlist = allowlist;
        self
    }
}

#[tracing::instrument(skip_all, fields(provider = %name))]
pub async fn build_provider_with_options(
    name: &str,
    entry: &ProviderEntry,
    secrets: &dyn SecretStore,
    opts: BuildOptions,
) -> Result<Arc<dyn Provider>> {
    match entry {
        ProviderEntry::OpenaiCompat {
            base_url,
            api_key_ref,
            extra_headers,
            default_extras,
            reasoning_dialect,
            history_reasoning,
            user_agent,
            runtime: _,
        } => {
            let api_key = resolve(secrets, api_key_ref).await?;
            let cfg = OpenAiCompatConfig {
                id: format!("openai-compat:{name}"),
                base_url: base_url.clone(),
                api_key,
                extra_headers: extra_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                default_extras: default_extras.clone(),
                reasoning_dialect: map_dialect(*reasoning_dialect),
                history_reasoning: map_history_reasoning(*history_reasoning),
                user_agent: user_agent.clone(),
                strict_translation: opts.strict_translation,
            };
            Ok(Arc::new(OpenAiCompatProvider::new(cfg)))
        }
        ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            anthropic_version,
            auth_kind,
            extra_headers,
            user_agent,
            adaptive_thinking,
            runtime: _,
        } => {
            let api_key = resolve(secrets, api_key_ref).await?;
            let cfg = AnthropicApiConfig {
                id: format!("anthropic-api:{name}"),
                api_key,
                base_url: base_url.clone(),
                anthropic_version: anthropic_version.clone(),
                auth_kind: *auth_kind,
                extra_headers: extra_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                user_agent: user_agent.clone(),
                adaptive_thinking: *adaptive_thinking,
            };
            Ok(Arc::new(AnthropicApiProvider::new(cfg)))
        }
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses {
            api_key_ref,
            account_id_ref,
            base_url,
            auth_kind,
            extra_headers,
            user_agent,
            originator,
            runtime: _,
        } => {
            validate_openai_responses_account_id(name, *auth_kind, account_id_ref)?;
            let api_key = resolve(secrets, api_key_ref).await?;
            let account_id = match account_id_ref {
                Some(uri) => Some(resolve(secrets, uri).await?),
                None => None,
            };
            let resolved_base_url =
                base_url.clone().unwrap_or_else(|| default_responses_base(*auth_kind));
            let cfg = OpenAiResponsesConfig {
                id: format!("openai-responses:{name}"),
                api_key,
                account_id,
                base_url: resolved_base_url,
                auth_kind: *auth_kind,
                extra_headers: extra_headers
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
            model_id,
            api_shape,
            creds,
            user_agent,
            extra_headers,
            anthropic_beta,
            additional_model_request_fields,
            adaptive_thinking,
            runtime: _,
        } => {
            let bedrock_creds = resolve_bedrock_creds(secrets, creds).await?;
            let resolved =
                routectl_providers::bedrock::auth::resolve(&bedrock_creds, region).await?;
            let cfg = BedrockConfig {
                id: format!("bedrock:{name}"),
                region: region.clone(),
                model_id: model_id.clone(),
                api_shape: map_bedrock_api_shape(*api_shape),
                creds: bedrock_creds,
                user_agent: user_agent.clone(),
                extra_headers: extra_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                anthropic_beta: anthropic_beta.clone(),
                anthropic_beta_allowlist: opts.bedrock_anthropic_beta_allowlist.clone(),
                additional_model_request_fields: additional_model_request_fields.clone(),
                adaptive_thinking: *adaptive_thinking,
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

fn map_dialect(d: ReasoningDialect) -> ProviderDialect {
    match d {
        ReasoningDialect::Openai => ProviderDialect::OpenAi,
        ReasoningDialect::Deepseek => ProviderDialect::DeepSeek,
        ReasoningDialect::Vllm => ProviderDialect::Vllm,
        ReasoningDialect::RawThinkTag => ProviderDialect::RawThinkTag,
        ReasoningDialect::Openrouter => ProviderDialect::OpenRouter,
        ReasoningDialect::Passthrough => ProviderDialect::Passthrough,
    }
}

fn map_history_reasoning(h: HistoryReasoning) -> ProviderHistoryReasoning {
    match h {
        HistoryReasoning::Auto => ProviderHistoryReasoning::Auto,
        HistoryReasoning::Strip => ProviderHistoryReasoning::Strip,
        HistoryReasoning::Preserve => ProviderHistoryReasoning::Preserve,
    }
}

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
        // BedrockMantle URL is region-specific; the relevant stage wires in a
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
