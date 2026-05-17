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
            validate_base_url_scheme(name, base_url)?;
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
            allowed_betas,
            runtime: _,
        } => {
            validate_base_url_scheme(name, base_url)?;
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
                allowed_betas: allowed_betas.clone(),
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
            validate_base_url_scheme(name, &resolved_base_url)?;
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
///     `max_tokens`) are present -- otherwise the egress drops a key
///     routectl just wrote and the upstream 400s.
///   - If any provider has a `[providers.X] anthropic_beta` floor,
///     `anthropic_beta` is on the list -- otherwise the filter
///     silently drops the operator-asserted always-send array.
#[cfg(feature = "bedrock")]
fn validate_bedrock_allowlists(
    has_provider_beta_floor: bool,
    _allowed_betas: &[String],
    allowed_body_fields: &[String],
) -> Result<()> {
    use routectl_core::Error;

    // Pass-through mode: nothing to validate.
    if allowed_body_fields.is_empty() {
        return Ok(());
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
             egress. See examples/bedrock.toml for the full baseline; or \
             remove `[bedrock] allowed_body_fields` entirely to disable \
             filtering and run in discovery mode."
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
    /// rejected even with https. Round-7 security review HIGH:
    /// link-local egress would leak SigV4 + API keys to whatever
    /// service the operator was tricked into pointing at.
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
