//! Build concrete `Provider` instances from `ProviderEntry` config rows.
//! Resolves secret references via a `SecretStore` at build time so the
//! provider can hold the plaintext API key it needs.

use std::sync::Arc;

use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{Error, Provider, Result};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect as ProviderDialect};

use crate::config::{ProviderEntry, ReasoningDialect};

pub async fn build_provider(
    name: &str,
    entry: &ProviderEntry,
    secrets: &dyn SecretStore,
) -> Result<Arc<dyn Provider>> {
    match entry {
        ProviderEntry::OpenaiCompat {
            base_url,
            api_key_ref,
            extra_headers,
            default_extras,
            reasoning_dialect,
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
            };
            Ok(Arc::new(OpenAiCompatProvider::new(cfg)))
        }
        ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            anthropic_version,
            runtime: _,
        } => {
            let api_key = resolve(secrets, api_key_ref).await?;
            let cfg = AnthropicApiConfig {
                id: format!("anthropic-api:{name}"),
                api_key,
                base_url: base_url.clone(),
                anthropic_version: anthropic_version.clone(),
            };
            Ok(Arc::new(AnthropicApiProvider::new(cfg)))
        }
        ProviderEntry::ClaudeCookie { .. } => Err(Error::Auth(format!(
            "provider `{name}`: claude-cookie is not enabled in this build (v0.2 feature)"
        ))),
        ProviderEntry::ChatgptCookie { .. } => Err(Error::Auth(format!(
            "provider `{name}`: chatgpt-cookie is not enabled in this build (v0.2 feature)"
        ))),
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
