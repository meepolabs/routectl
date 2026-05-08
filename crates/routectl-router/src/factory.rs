//! Build concrete `Provider` instances from `ProviderEntry` config rows.
//! Resolves secret references via a `SecretStore` at build time so the
//! provider can hold the plaintext API key it needs.

use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Error, Provider, Result};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_compat::{
    OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect as ProviderDialect,
};

#[cfg(feature = "bedrock")]
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider};

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
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct BuildOptions {
    /// When `true`, providers reject requests carrying canonical-only
    /// fields they cannot represent on the wire (e.g. an OpenAI-compat
    /// egress receiving an Anthropic `cache_control` block). Default
    /// `false` -- warn-and-drop. Set from `[server] strict_translation`.
    pub strict_translation: bool,
}

impl BuildOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_strict_translation(mut self, strict: bool) -> Self {
        self.strict_translation = strict;
        self
    }
}

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
            };
            Ok(Arc::new(AnthropicApiProvider::new(cfg)))
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
                additional_model_request_fields: additional_model_request_fields.clone(),
            };
            Ok(Arc::new(BedrockProvider::new(cfg, resolved)))
        }
        ProviderEntry::ClaudeCookie { .. } => Err(Error::Auth(format!(
            "provider `{name}`: claude-cookie is not enabled in this build (v0.2 feature)"
        ))),
        ProviderEntry::ChatgptCookie { .. } => Err(Error::Auth(format!(
            "provider `{name}`: chatgpt-cookie is not enabled in this build (v0.2 feature)"
        ))),
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
    let secret_ref = SecretRef::parse(uri)?;
    secrets.get(&secret_ref).await
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
