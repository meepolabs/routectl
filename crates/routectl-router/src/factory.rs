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

/// Convenience wrapper that builds a provider with `BuildOptions::default()`.
///
/// Note for Bedrock providers: the default `BuildOptions` carries empty
/// `bedrock_allowed_betas` / `bedrock_allowed_body_fields` lists, which
/// the per-provider `validate_bedrock_allowlists` guard rejects. Use
/// `build_provider_with_options` and populate the lists from
/// `[bedrock] allowed_betas` / `[bedrock] allowed_body_fields` -- see
/// `examples/bedrock.toml` for the empirical baseline. routectl-cli
/// callers (`server`, `commands::test`) already do this; only library
/// consumers reaching for `build_provider` directly need to switch.
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
    /// Bedrock-accepted `anthropic_beta` flags. Sourced from
    /// `[bedrock] allowed_betas` TOML and applied to every Bedrock
    /// provider. routectl ships no const default; AWS schema drift is
    /// operator-tracked. Empty when no Bedrock provider is configured;
    /// see `validate_bedrock_global_config` for the startup check that
    /// rejects an empty list when one is.
    pub bedrock_allowed_betas: Vec<String>,
    /// Bedrock-accepted top-level body fields / Converse extras keys.
    /// Sourced from `[bedrock] allowed_body_fields` TOML. Same
    /// per-deployment shape as `bedrock_allowed_betas`.
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
            let resolved_base_url = base_url
                .clone()
                .unwrap_or_else(|| default_responses_base(*auth_kind));
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
            validate_bedrock_allowlists(
                !anthropic_beta.is_empty(),
                &opts.bedrock_allowed_betas,
                &opts.bedrock_allowed_body_fields,
            )?;
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
                allowed_betas: opts.bedrock_allowed_betas.clone(),
                allowed_body_fields: opts.bedrock_allowed_body_fields.clone(),
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
/// Bedrock-Invoke body. If `[bedrock] allowed_body_fields` is missing
/// any of these, the egress drops them on send and the upstream 400s
/// the malformed body. Surfaces here as a clean startup error instead.
///
/// Keep in sync with `is_bedrock_invoke_managed_key` in
/// `routectl-providers/src/bedrock/invoke.rs` -- that is the writer
/// side; this is the validator side.
#[cfg(feature = "bedrock")]
const BEDROCK_REQUIRED_BODY_FIELDS: &[&str] = &["anthropic_version", "max_tokens", "messages"];

#[cfg(feature = "bedrock")]
fn validate_bedrock_allowlists(
    has_provider_beta_floor: bool,
    allowed_betas: &[String],
    allowed_body_fields: &[String],
) -> Result<()> {
    use routectl_core::Error;

    if allowed_betas.is_empty() {
        return Err(Error::Config(
            "[bedrock] allowed_betas is empty but at least one provider has \
             kind = \"bedrock\". routectl ships no default; populate the \
             list with the AWS-gated betas for your account. See \
             examples/bedrock.toml for the empirical 2026-05-12 baseline."
                .into(),
        ));
    }

    if allowed_body_fields.is_empty() {
        return Err(Error::Config(
            "[bedrock] allowed_body_fields is empty but at least one \
             provider has kind = \"bedrock\". routectl ships no default; \
             populate the list with the AWS-accepted body fields for your \
             account. See examples/bedrock.toml for the empirical 2026-05-12 \
             baseline."
                .into(),
        ));
    }

    let missing: Vec<&str> = BEDROCK_REQUIRED_BODY_FIELDS
        .iter()
        .copied()
        .filter(|required| !allowed_body_fields.iter().any(|s| s == required))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Config(format!(
            "[bedrock] allowed_body_fields is missing routectl-mandatory keys \
             {missing:?}. Without these, every Bedrock request 400s on the \
             egress. See examples/bedrock.toml for the full baseline."
        )));
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

/// Reject configurations that wire a `kind = "bedrock"` provider
/// without populating the operator-supplied `[bedrock] allowed_betas`
/// and `[bedrock] allowed_body_fields` lists. routectl ships no
/// const defaults for either surface (AWS schema drift is operator-
/// tracked, not release-bound), so an empty list at startup means
/// every flag/field drops on the egress and the upstream 400s every
/// request -- a confusing failure mode that this guard converts into
/// a clean `Error::Config` with a copy-paste hint to the empirical
/// baseline in `examples/bedrock.toml`.
///
/// Call once per process startup BEFORE building any providers.
/// No-op when no Bedrock provider is configured.
#[cfg(feature = "bedrock")]
pub fn validate_bedrock_global_config(config: &crate::config::Config) -> Result<()> {
    let mut bedrock_in_use = false;
    let mut has_provider_beta_floor = false;
    for entry in config.providers.values() {
        if let crate::config::ProviderEntry::Bedrock { anthropic_beta, .. } = entry {
            bedrock_in_use = true;
            has_provider_beta_floor |= !anthropic_beta.is_empty();
        }
    }
    if !bedrock_in_use {
        return Ok(());
    }

    validate_bedrock_allowlists(
        has_provider_beta_floor,
        &config.bedrock.allowed_betas,
        &config.bedrock.allowed_body_fields,
    )
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
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShapeConfig::Invoke,
            creds: BedrockCredsConfig::DefaultChain,
            user_agent: None,
            extra_headers: BTreeMap::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
            runtime: Default::default(),
        }
    }

    fn bedrock_provider_entry_with_floor_beta() -> ProviderEntry {
        let ProviderEntry::Bedrock {
            region,
            model_id,
            api_shape,
            creds,
            user_agent,
            extra_headers,
            additional_model_request_fields,
            adaptive_thinking,
            runtime,
            ..
        } = bedrock_provider_entry()
        else {
            unreachable!();
        };
        ProviderEntry::Bedrock {
            region,
            model_id,
            api_shape,
            creds,
            user_agent,
            extra_headers,
            anthropic_beta: vec!["future-flag-2026-12-31".into()],
            additional_model_request_fields,
            adaptive_thinking,
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
    fn bedrock_provider_with_empty_allowed_betas_errors() {
        // Arrange: provider needs Bedrock; allowed_betas missing.
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: Vec::new(),
                allowed_body_fields: baseline_fields(),
            },
        );

        // Act
        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("allowed_betas"), "msg: {msg}");
        assert!(msg.contains("examples/bedrock.toml"), "msg: {msg}");
    }

    #[test]
    fn bedrock_provider_with_empty_allowed_body_fields_errors() {
        // Arrange
        let cfg = config_with(
            true,
            BedrockGlobalConfig {
                allowed_betas: baseline_betas(),
                allowed_body_fields: Vec::new(),
            },
        );

        // Act
        let err = validate_bedrock_global_config(&cfg).unwrap_err();

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("allowed_body_fields"), "msg: {msg}");
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
