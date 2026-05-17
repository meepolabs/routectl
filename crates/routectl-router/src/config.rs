//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Server bind config.
    #[serde(default)]
    pub server: ServerConfig,

    /// Provider definitions, keyed by user-facing name (e.g. "deepseek", "claude-pro").
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,

    /// Aliases mapping a logical model name -> ordered fallback chain.
    #[serde(default)]
    pub aliases: BTreeMap<String, AliasEntry>,

    /// Catch-all destination for requests whose `model` field doesn't
    /// resolve. Applied AFTER `[aliases]` lookup and `provider:model`
    /// literal detection, so configured aliases and explicit literals
    /// always win. Lets clients send model names that haven't been
    /// added to `[aliases]` or `[ingress.<dialect>.aliases]` yet (e.g.
    /// a fresh claude release the operator hasn't mapped yet) without
    /// hard-failing -- they go to this destination instead. Accepts
    /// the same shapes the wire `model` field accepts: an alias name
    /// from `[aliases]` (e.g. `"med"`) or a `provider:model` literal
    /// (e.g. `"bedrock-default:global.anthropic.claude-sonnet-4-6"`).
    /// When unset, unknown models error with `UnknownAlias`.
    #[serde(default)]
    pub default_model: Option<String>,

    /// Default retry policy applied per-provider attempt.
    #[serde(default)]
    pub retry: RetryPolicy,

    /// Schema compatibility mode for the outward shape.
    /// `openrouter` (default): full reasoning_details surface.
    /// `openai`: strip routectl/openrouter extensions for paranoid clients.
    #[serde(default)]
    pub legacy_compat: LegacyCompat,

    /// Per-ingress configuration: model-id -> alias mapping. v0.4.0
    /// adds two ingress dialects (OpenAI Chat Completions, Anthropic
    /// Messages); each can have its own alias map for clients that
    /// can't override the `model` field directly (Claude Code, etc.).
    #[serde(default)]
    pub ingress: IngressConfig,

    /// Bedrock-wide settings that apply to every Bedrock provider.
    /// Carries the operator-supplied `allowed_betas` and
    /// `allowed_body_fields` lists -- routectl ships no defaults so
    /// AWS schema drift does not require a routectl release. See
    /// `examples/bedrock.toml` for the empirical baseline. Future
    /// shared knobs (e.g. region-default, retry-default) land here too.
    #[serde(default)]
    pub bedrock: BedrockGlobalConfig,
}

/// Bedrock-wide configuration shared by every `[providers.X]` entry of
/// `kind = "bedrock"`. Both allowlists below are operator-owned; the
/// routectl binary ships no defaults so AWS schema drift (Anthropic
/// adds a beta, Bedrock gates a body field) does not require a
/// routectl release. See `examples/bedrock.toml` for the empirical
/// 2026-05-12 baseline; copy and tune as your account's gating
/// evolves.
///
/// **Empty list = pass-through.** Either field, when empty (or the
/// entire `[bedrock]` section omitted), disables that filter -- the
/// upstream sees the assembled value unchanged. This is the
/// discovery-mode default: bring up routectl, observe actual traffic
/// via `ROUTECTL_LOG=routectl_providers::bedrock=trace`, then
/// populate the list with what you observe.
///
/// Startup validation in `crate::factory::validate_bedrock_global_config`
/// kicks in only when `allowed_body_fields` is non-empty, and rejects
/// a list missing routectl-mandatory keys (`messages`,
/// `anthropic_version`, `max_tokens`) or missing `anthropic_beta`
/// when a `[providers.X] anthropic_beta` floor is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BedrockGlobalConfig {
    /// Bedrock-accepted `anthropic_beta` flags. AWS validates each
    /// entry independently and 400s the request on the first
    /// unsupported flag. **Empty list = pass-through** (no filtering;
    /// every flag the ingress lifts in is forwarded). Populate via
    /// TOML to enable filtering. `examples/bedrock.toml` ships the
    /// empirical 2026-05-12 baseline.
    ///
    /// Per-provider `[providers.X] anthropic_beta` is unrelated and
    /// keeps its existing semantics (operator-asserted floor that is
    /// always sent and bypasses this filter).
    #[serde(default)]
    pub allowed_betas: Vec<String>,

    /// Bedrock-accepted top-level body fields (Invoke) /
    /// `additionalModelRequestFields` keys (Converse). Bedrock 400s
    /// any unrecognized field with `"Extra inputs are not permitted"`,
    /// so the Anthropic ingress's forward-compat sweep needs filtering
    /// on the Bedrock egress when this list is non-empty. **Empty
    /// list = pass-through** (no filtering; every key in the assembled
    /// body / bag is forwarded).
    ///
    /// When non-empty, must include the routectl-mandatory keys
    /// (`messages`, `anthropic_version`, `max_tokens`) for requests
    /// to construct successfully -- startup validation rejects an
    /// incomplete list with a copy-paste hint.
    #[serde(default)]
    pub allowed_body_fields: Vec<String>,
}

/// Operator-side reasoning defaults. Folds into ChatRequest.reasoning
/// per-attempt at the router; caller's non-None values always win.
///
/// Both fields are optional and default to None. Setting either populates
/// the corresponding `ChatRequest.reasoning.{effort,enabled}` field on
/// requests routing through this provider, but only when the caller did
/// not already supply that field on the wire. The router never
/// overwrites a caller-supplied value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReasoningDefaults {
    /// Maps to ChatRequest.reasoning.effort. Vocabulary is passthrough
    /// (egresses interpret); empty string rejected at startup. Common
    /// values: "minimal", "low", "medium", "high", "xhigh", "max",
    /// "none". Unknown values pass through verbatim for forward
    /// compatibility with vendor-specific levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Maps to ChatRequest.reasoning.enabled. `Some(true)` opts into
    /// reasoning by default for this provider; `Some(false)` pins it
    /// off; `None` defers to whatever the caller and provider's own
    /// defaults decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl ReasoningDefaults {
    /// Construct an empty `ReasoningDefaults`. Use the `with_thinking`
    /// / `with_enabled` builders to populate fields. Builder pattern
    /// matches the rest of the config surface (`ProviderEntry::with_*`,
    /// `AliasEntry::with_*`) and keeps external callers compatible
    /// across `#[non_exhaustive]` field additions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the operator-side `thinking` (effort) value. Replaces any
    /// previously-set value.
    pub fn with_thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = Some(thinking.into());
        self
    }

    /// Set the operator-side `enabled` flag. Replaces any
    /// previously-set value.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// True when neither `thinking` nor `enabled` is set. Used by the
    /// router to skip the merge step entirely on providers without
    /// configured defaults.
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none() && self.enabled.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind host. Defaults to localhost. Refuses non-loopback unless
    /// `--unsafe-public` is passed on the CLI.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,

    /// Listener-side auth. When `tokens` is non-empty, every request
    /// must carry a matching `x-api-key` or `Authorization: Bearer
    /// <token>` header. Tokens are SecretRef URIs (env://, file://,
    /// literal:) and are resolved at startup.
    #[serde(default)]
    pub auth: Option<ServerAuth>,

    /// When true, lossy translation seams (e.g. cache_control on a
    /// canonical -> OpenAI-compat egress) return a 400 instead of
    /// emitting a `tracing::warn!`. Default false (warn-and-drop)
    /// preserves dev ergonomics; flip to true for production CI.
    #[serde(default)]
    pub strict_translation: bool,

    /// When true (default), the `x-routectl-disable-fallbacks` request
    /// header lets a client pin a request to a single provider with
    /// no fallback chain. Useful for tests, dev probing, and
    /// per-request triage. Set to `false` for hardened multi-tenant
    /// deployments where authenticated clients should not be able to
    /// disable the gateway's HA story or probe per-provider health.
    #[serde(default = "default_allow_disable_fallbacks")]
    pub allow_disable_fallbacks: bool,
}

fn default_allow_disable_fallbacks() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: default_allow_disable_fallbacks(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerAuth {
    /// Allowed tokens, stored as SecretRef URIs. Empty list means
    /// "no auth required" (loopback dev default).
    #[serde(default)]
    pub tokens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngressConfig {
    #[serde(default)]
    pub openai: IngressShape,
    #[serde(default)]
    pub anthropic: IngressShape,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngressShape {
    /// Map a wire `model` field value to a configured alias. When the
    /// request model matches a key here, routing uses the value as
    /// the alias. The `x-routectl-alias` header overrides this.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8787
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProviderEntry {
    #[non_exhaustive]
    OpenaiCompat {
        base_url: String,
        /// Reference to the API key. One of:
        ///   - `env://VAR_NAME`             (process env var)
        ///   - `file:///abs/path/to/key`    (mode-600 file)
        ///   - `literal:plaintext`          (inline; placeholders only)
        api_key_ref: String,
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        #[serde(default)]
        default_extras: Option<serde_json::Value>,
        #[serde(default)]
        reasoning_dialect: ReasoningDialect,
        /// How to handle the `reasoning` / `reasoning_content` /
        /// `reasoning_details` fields on outgoing assistant messages
        /// in multi-turn history.
        ///
        ///   - `auto` (default): use the dialect's default. DeepSeek
        ///     and vLLM strip; OpenAI and OpenRouter pass through.
        ///   - `strip`: force-drop reasoning fields from outgoing
        ///     assistant messages, regardless of dialect. Use for
        ///     DeepSeek v3 / vLLM <= 0.6 hosts that 400 on echo-back.
        ///   - `preserve`: emit the dialect-native preserve shape on
        ///     outgoing assistant messages (DeepSeek/vLLM emit
        ///     `reasoning_content`; OpenRouter emits `reasoning_details`).
        ///     Required by DeepSeek v4+, which 400s with
        ///     `"reasoning_content in the thinking mode must be passed
        ///     back"` if echo-back is missing.
        ///
        /// The `auto` default preserves backward compatibility for
        /// existing configs. Operators upgrading to DeepSeek v4 must
        /// set `history_reasoning = "preserve"` explicitly.
        #[serde(default)]
        history_reasoning: HistoryReasoning,
        /// Override the outbound User-Agent.
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
        /// Operator-side reasoning defaults (see [`ReasoningDefaults`]).
        /// `thinking` lifts to `reasoning.effort`, `enabled` lifts to
        /// `reasoning.enabled`. Caller-supplied values always win.
        #[serde(default, flatten)]
        reasoning_defaults: ReasoningDefaults,
    },
    #[non_exhaustive]
    AnthropicApi {
        api_key_ref: String,
        #[serde(default = "default_anthropic_base")]
        base_url: String,
        #[serde(default = "default_anthropic_version")]
        anthropic_version: String,
        /// How the Messages API authenticates this provider. Default
        /// is `api-key` (the standard `x-api-key` header). Use
        /// `oauth-bearer` when `api_key_ref` resolves to a Claude Code
        /// subscription access token (`sk-ant-oat01-...`).
        #[serde(default)]
        auth_kind: AuthKind,
        /// Extra HTTP headers applied to every Anthropic API request.
        /// Common usage: `extra_headers = { "anthropic-beta" = "context-1m-2025-08-07" }`.
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        /// Override the outbound User-Agent. Useful for IAM-gated upstreams.
        #[serde(default)]
        user_agent: Option<String>,
        /// Use the Opus 4.7+ adaptive thinking wire shape on this provider.
        /// When `true`, routectl rewrites `thinking: {type:"enabled",
        /// budget_tokens:N}` to `thinking: {type:"adaptive"}` and lifts
        /// `reasoning.effort` (verbatim string) into top-level
        /// `output_config.effort`. Older Claude models (4.5/4.6 family)
        /// still accept the legacy shape, so this is opt-in per provider
        /// rather than a compiled model-name match. Default: `false`.
        #[serde(default)]
        adaptive_thinking: Option<bool>,
        /// Optional operator-supplied allowlist for `anthropic_beta`
        /// flags forwarded to api.anthropic.com. Default (empty) is
        /// pass-through: every beta the client requests goes upstream
        /// verbatim. When non-empty, ingress-lifted values not in the
        /// list are dropped at DEBUG level. Mirrors the Bedrock-egress
        /// `[bedrock] allowed_betas` shape so multi-tenant / API-gateway
        /// deployments can constrain which betas authenticated clients
        /// can opt into (e.g. billing-gated features).
        #[serde(default)]
        allowed_betas: Vec<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
        /// Operator-side reasoning defaults (see [`ReasoningDefaults`]).
        /// `thinking` lifts to `reasoning.effort`, `enabled` lifts to
        /// `reasoning.enabled`. Caller-supplied values always win.
        #[serde(default, flatten)]
        reasoning_defaults: ReasoningDefaults,
    },
    /// OpenAI Responses API provider. Three auth surfaces (CG.A wires
    /// the first; CG.D/E land the others):
    /// - `chatgpt-oauth`: ChatGPT subscription JWT.
    /// - `api-key`: standard OpenAI API key.
    /// - `bedrock-mantle`: AWS Mantle proxy over SigV4.
    ///
    /// `base_url` is optional: when unset, the factory picks the
    /// auth_kind-appropriate default at provider build time.
    #[cfg(feature = "openai-responses")]
    #[non_exhaustive]
    OpenaiResponses {
        /// Resolves to the bearer JWT (ChatgptOauth) or API key
        /// (ApiKey). Ignored for BedrockMantle which signs via SigV4.
        api_key_ref: String,
        /// ChatGPT account UUID. Required when `auth_kind =
        /// "chatgpt-oauth"`; must be absent for the other variants.
        #[serde(default)]
        account_id_ref: Option<String>,
        /// Endpoint base URL. None -> factory picks an auth_kind-
        /// appropriate default. Operators can pin a specific value
        /// to point at a staging shard, a localhost mock, etc.
        #[serde(default)]
        base_url: Option<String>,
        /// Which auth surface to dispatch on. Default
        /// `chatgpt-oauth`.
        #[serde(default)]
        auth_kind: OpenaiResponsesAuthKind,
        /// Extra HTTP headers applied to every request (after auth).
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        /// Override the outbound User-Agent. None -> default
        /// `routectl/<version> codex-cli`.
        #[serde(default)]
        user_agent: Option<String>,
        /// Override the `originator` header on the ChatgptOauth surface.
        /// None -> `codex_cli_rs` (codex's `DEFAULT_ORIGINATOR`).
        #[serde(default)]
        originator: Option<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
        /// Operator-side reasoning defaults (see [`ReasoningDefaults`]).
        /// `thinking` lifts to `reasoning.effort`, `enabled` lifts to
        /// `reasoning.enabled`. Caller-supplied values always win.
        #[serde(default, flatten)]
        reasoning_defaults: ReasoningDefaults,
    },
    /// Native AWS Bedrock provider. Speaks SigV4 directly to
    /// `bedrock-runtime.<region>.amazonaws.com`. Pick `api_shape` to
    /// switch between vendor-specific InvokeModel (default) and
    /// vendor-neutral Converse.
    #[cfg(feature = "bedrock")]
    #[non_exhaustive]
    Bedrock {
        region: String,
        model_id: String,
        #[serde(default)]
        api_shape: BedrockApiShapeConfig,
        creds: BedrockCredsConfig,
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        #[serde(default)]
        anthropic_beta: Vec<String>,
        #[serde(default)]
        additional_model_request_fields: Option<serde_json::Value>,
        /// Use the Opus 4.7+ adaptive thinking wire shape on this provider.
        /// Same semantics as the AnthropicApi variant above. Set this on
        /// Bedrock providers whose `model_id` is an opus-4-7+ inference
        /// profile (e.g. `global.anthropic.claude-opus-4-7-v1:0`).
        /// Default: `false`.
        #[serde(default)]
        adaptive_thinking: Option<bool>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
        /// Operator-side reasoning defaults (see [`ReasoningDefaults`]).
        /// `thinking` lifts to `reasoning.effort`, `enabled` lifts to
        /// `reasoning.enabled`. Caller-supplied values always win.
        #[serde(default, flatten)]
        reasoning_defaults: ReasoningDefaults,
    },
}

/// TOML-side mirror of `routectl_providers::bedrock::BedrockApiShape`.
/// We don't re-export the providers-side enum directly because TOML
/// configs need to parse cleanly even when the `bedrock` feature is
/// off (so non-Bedrock builds stay lean), and serde derives don't
/// like cfg-gated re-exports.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BedrockApiShapeConfig {
    #[default]
    Invoke,
    Converse,
}

/// TOML-side credentials descriptor for a Bedrock provider.
///
/// Each variant is tagged by `kind`. Secret-bearing fields hold raw
/// secret-URI strings (`env://`, `file://`, `literal:`) which the
/// factory parses + resolves at provider build time -- same pattern
/// as `api_key_ref` on the other provider variants.
///
/// Examples:
/// ```toml
/// # Bedrock console short-term API key
/// creds = { kind = "bearer-key", key_ref = "file:///home/me/.config/routectl/bedrock.key" }
///
/// # Static AWS access keys
/// creds = { kind = "static",
///           access_key_ref  = "env://AWS_ACCESS_KEY_ID",
///           secret_key_ref  = "env://AWS_SECRET_ACCESS_KEY",
///           session_token_ref = "env://AWS_SESSION_TOKEN" }
///
/// # Named profile in ~/.aws/credentials (incl. SSO)
/// creds = { kind = "profile", name = "bedrock-prod" }
///
/// # Standard AWS provider chain (env -> profile -> SSO -> IRSA -> IMDS)
/// creds = { kind = "default-chain" }
/// ```
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BedrockCredsConfig {
    BearerKey {
        key_ref: String,
    },
    Static {
        access_key_ref: String,
        secret_key_ref: String,
        #[serde(default)]
        session_token_ref: Option<String>,
    },
    Profile {
        name: String,
    },
    DefaultChain,
}

impl ProviderEntry {
    /// Get the runtime policy attached to this entry. Centralizes the
    /// match so the router doesn't repeat it.
    pub fn runtime(&self) -> &ProviderRuntimePolicy {
        match self {
            Self::OpenaiCompat { runtime, .. } | Self::AnthropicApi { runtime, .. } => runtime,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => runtime,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => runtime,
        }
    }

    /// Get the operator-side reasoning defaults attached to this entry.
    /// Returns `None` when both fields on the underlying struct are
    /// unset, so the router's merge step can short-circuit cheaply.
    pub fn reasoning_defaults(&self) -> Option<&ReasoningDefaults> {
        let rd = match self {
            Self::OpenaiCompat {
                reasoning_defaults, ..
            }
            | Self::AnthropicApi {
                reasoning_defaults, ..
            } => reasoning_defaults,
            #[cfg(feature = "bedrock")]
            Self::Bedrock {
                reasoning_defaults, ..
            } => reasoning_defaults,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                reasoning_defaults, ..
            } => reasoning_defaults,
        };
        if rd.is_empty() {
            None
        } else {
            Some(rd)
        }
    }

    pub fn openai_compat(base_url: impl Into<String>, api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiCompat {
            base_url: base_url.into(),
            api_key_ref: api_key_ref.into(),
            extra_headers: BTreeMap::new(),
            default_extras: None,
            reasoning_dialect: ReasoningDialect::default(),
            history_reasoning: HistoryReasoning::default(),
            user_agent: None,
            runtime: ProviderRuntimePolicy::default(),
            reasoning_defaults: ReasoningDefaults::default(),
        }
    }

    pub fn anthropic_api(api_key_ref: impl Into<String>) -> Self {
        Self::AnthropicApi {
            api_key_ref: api_key_ref.into(),
            base_url: default_anthropic_base(),
            anthropic_version: default_anthropic_version(),
            auth_kind: AuthKind::ApiKey,
            extra_headers: BTreeMap::new(),
            user_agent: None,
            adaptive_thinking: None,
            allowed_betas: Vec::new(),
            runtime: ProviderRuntimePolicy::default(),
            reasoning_defaults: ReasoningDefaults::default(),
        }
    }

    pub fn with_auth_kind(mut self, kind: AuthKind) -> Self {
        match &mut self {
            Self::AnthropicApi { auth_kind, .. } => *auth_kind = kind,
            _ => panic!("ProviderEntry::with_auth_kind only applies to anthropic-api"),
        }
        self
    }

    pub fn with_runtime(mut self, rt: ProviderRuntimePolicy) -> Self {
        match &mut self {
            Self::OpenaiCompat { runtime, .. } | Self::AnthropicApi { runtime, .. } => {
                *runtime = rt
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => *runtime = rt,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => *runtime = rt,
        }
        self
    }

    pub fn with_extra_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        match &mut self {
            Self::OpenaiCompat { extra_headers, .. } => *extra_headers = headers,
            _ => panic!("ProviderEntry::with_extra_headers only applies to openai-compat"),
        }
        self
    }

    pub fn with_default_extras(mut self, extras: Option<serde_json::Value>) -> Self {
        match &mut self {
            Self::OpenaiCompat { default_extras, .. } => *default_extras = extras,
            _ => panic!("ProviderEntry::with_default_extras only applies to openai-compat"),
        }
        self
    }

    pub fn with_reasoning_dialect(mut self, dialect: ReasoningDialect) -> Self {
        match &mut self {
            Self::OpenaiCompat {
                reasoning_dialect, ..
            } => *reasoning_dialect = dialect,
            _ => {
                panic!("ProviderEntry::with_reasoning_dialect only applies to openai-compat")
            }
        }
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let u = url.into();
        match &mut self {
            Self::OpenaiCompat { base_url, .. } | Self::AnthropicApi { base_url, .. } => {
                *base_url = u
            }
            _ => panic!("ProviderEntry::with_base_url only applies to api-backed providers"),
        }
        self
    }

    pub fn with_anthropic_version(mut self, version: impl Into<String>) -> Self {
        match &mut self {
            Self::AnthropicApi {
                anthropic_version, ..
            } => *anthropic_version = version.into(),
            _ => {
                panic!("ProviderEntry::with_anthropic_version only applies to anthropic-api")
            }
        }
        self
    }

    pub fn redact_secrets(&mut self) {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } | Self::AnthropicApi { api_key_ref, .. } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { creds, .. } => creds.redact(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                ..
            } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
                if let Some(a) = account_id_ref {
                    *a = redact_literal_secret(a);
                }
            }
        }
    }

    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } | Self::AnthropicApi { api_key_ref, .. } => {
                vec![api_key_ref.as_str()]
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { creds, .. } => creds.secret_uris(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                ..
            } => {
                let mut v = vec![api_key_ref.as_str()];
                if let Some(a) = account_id_ref {
                    v.push(a.as_str());
                }
                v
            }
        }
    }
}

#[cfg(feature = "bedrock")]
impl BedrockCredsConfig {
    /// Replace literal-prefixed secret values with `literal:[REDACTED]`.
    /// Other URI schemes are already non-secret pointers.
    pub fn redact(&mut self) {
        match self {
            Self::BearerKey { key_ref } => {
                *key_ref = redact_literal_secret(key_ref);
            }
            Self::Static {
                access_key_ref,
                secret_key_ref,
                session_token_ref,
            } => {
                *access_key_ref = redact_literal_secret(access_key_ref);
                *secret_key_ref = redact_literal_secret(secret_key_ref);
                if let Some(t) = session_token_ref {
                    *t = redact_literal_secret(t);
                }
            }
            Self::Profile { .. } | Self::DefaultChain => {}
        }
    }

    /// Enumerate every secret-URI string a config check should resolve.
    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            Self::BearerKey { key_ref } => vec![key_ref.as_str()],
            Self::Static {
                access_key_ref,
                secret_key_ref,
                session_token_ref,
            } => {
                let mut v = vec![access_key_ref.as_str(), secret_key_ref.as_str()];
                if let Some(t) = session_token_ref {
                    v.push(t.as_str());
                }
                v
            }
            Self::Profile { .. } | Self::DefaultChain => Vec::new(),
        }
    }
}

fn redact_literal_secret(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("literal:") {
        if rest.is_empty() {
            "literal:".into()
        } else {
            "literal:[REDACTED]".into()
        }
    } else {
        uri.to_string()
    }
}

/// Per-provider runtime knobs that gate dispatch: rate limits and a
/// passive circuit breaker. All fields default to "off" so omitting
/// the block leaves provider behavior unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderRuntimePolicy {
    /// Maximum requests per minute. When exceeded, the router treats
    /// this provider as a fallbackable failure and tries the next entry
    /// in the chain. None = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// Trip the circuit breaker after this many consecutive failed
    /// attempts within the failure window. Once tripped, the router
    /// skips this provider for `circuit_cooldown_ms`.
    /// None = breaker disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_failures: Option<u32>,
    /// How long to keep the circuit open (skip provider) once tripped.
    /// Defaults to 30s when `circuit_failures` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_cooldown_ms: Option<u64>,
    /// Per-attempt request timeout that applies to every alias chain
    /// entry routing through this provider. Only used when the alias's
    /// `[aliases.X.retry] request_timeout_ms` is unset; the alias-level
    /// override always wins.
    ///
    /// Resolution order (alias > provider > global):
    ///   alias.retry.request_timeout_ms
    ///     -> provider.request_timeout_ms (this field)
    ///       -> [retry] request_timeout_ms (workspace global)
    ///         -> None (no cap, reqwest's default)
    ///
    /// Use this when many aliases share the same upstream and the
    /// timeout is an upstream characteristic (e.g., NIM cold-start),
    /// not a routing decision (e.g., "heavy alias retries less").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Per-attempt first-byte timeout for streaming responses through
    /// this provider. Same alias > provider > global resolution as
    /// `request_timeout_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,
}

fn default_anthropic_base() -> String {
    "https://api.anthropic.com".into()
}

fn default_anthropic_version() -> String {
    "2023-06-01".into()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompat,
    AnthropicApi,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningDialect {
    #[default]
    Openai,
    Deepseek,
    Vllm,
    RawThinkTag,
    Openrouter,
    Passthrough,
}

/// Outgoing-history reasoning policy for openai-compat providers.
/// See `ProviderEntry::OpenaiCompat::history_reasoning` for semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryReasoning {
    /// Use the dialect's default (DeepSeek/vLLM strip; OpenAI/OpenRouter
    /// pass through). Backward-compatible.
    #[default]
    Auto,
    /// Force-strip reasoning fields from outgoing assistant messages.
    Strip,
    /// Force-emit the dialect's preserve shape on outgoing assistant
    /// messages.
    Preserve,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AliasEntry {
    /// Ordered list of `provider:model` targets. First entry is preferred.
    pub chain: Vec<String>,
    /// Optional override of the default retry policy for this alias.
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
}

impl AliasEntry {
    pub fn new(chain: Vec<String>) -> Self {
        Self { chain, retry: None }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Default retry-attempts cap per provider in the chain. Used when
    /// no per-error-class override below is set.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Initial backoff in ms.
    #[serde(default = "default_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff multiplier per attempt.
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Random additional ms added to each backoff sleep (0..jitter_ms).
    /// Prevents thundering-herd retries when many clients fail at once.
    #[serde(default)]
    pub jitter_ms: u64,
    /// Status codes that trigger fallback to the next provider in the chain
    /// (in addition to network errors).
    #[serde(default = "default_fallback_status")]
    pub fallback_on_status: Vec<u16>,

    /// Per-error-class retry caps. When set, override `max_attempts` for
    /// that specific class. Useful because rate-limits often clear in
    /// a single retry while flaky 5xx may need more attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_429: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_5xx: Option<u32>,
    /// Network errors (status 0): DNS, TCP connect, TLS handshake,
    /// request body, request timeout. Default is `max_attempts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_network: Option<u32>,

    /// End-to-end per-attempt timeout. `None` means rely on the
    /// reqwest client's default (no explicit cap). When set, the
    /// router wraps each upstream call in `tokio::time::timeout`
    /// and treats expiry as a network error (status 0, retryable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// First-byte timeout for streaming responses. If the upstream
    /// hasn't emitted any bytes in this window, the stream is
    /// abandoned and (if no chunk has been delivered yet) the next
    /// provider in the chain is tried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            jitter_ms: 0,
            fallback_on_status: default_fallback_status(),
            retry_on_429: None,
            retry_on_5xx: None,
            retry_on_network: None,
            request_timeout_ms: None,
            stream_first_byte_timeout_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderEntry, ReasoningDialect};

    #[test]
    #[should_panic(expected = "with_anthropic_version")]
    fn wrong_variant_setter_panics() {
        let _ = ProviderEntry::openai_compat("https://example.com/v1", "literal:test")
            .with_anthropic_version("2023-06-01");
    }

    #[test]
    fn redact_secrets_redacts_literal_only() {
        let mut entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test")
            .with_reasoning_dialect(ReasoningDialect::Openai);
        entry.redact_secrets();
        assert_eq!(entry.secret_uris(), vec!["literal:[REDACTED]"]);
    }
}

#[cfg(test)]
mod reasoning_defaults_tests {
    use super::{Config, ProviderEntry};
    use crate::factory::validate_reasoning_defaults;

    fn parse_entry(toml_text: &str) -> ProviderEntry {
        toml::from_str::<ProviderEntry>(toml_text).expect("toml parse")
    }

    #[test]
    fn thinking_high_alone_populates_effort() {
        // Arrange
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "high"
"#;

        // Act
        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        // Assert
        assert_eq!(defaults.thinking.as_deref(), Some("high"));
        assert!(defaults.enabled.is_none());
    }

    #[test]
    fn enabled_true_alone_populates_enabled() {
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
enabled = true
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert!(defaults.thinking.is_none());
        assert_eq!(defaults.enabled, Some(true));
    }

    #[test]
    fn enabled_false_alone_populates_enabled() {
        // Pin: `enabled = false` must round-trip as Some(false), NOT
        // collapse to None. The router merge layer treats None as "no
        // operator opinion" and Some(false) as "pin reasoning off".
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
enabled = false
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert!(defaults.thinking.is_none());
        assert_eq!(defaults.enabled, Some(false));
    }

    #[test]
    fn both_fields_set_populate_both() {
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "medium"
enabled = true
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("medium"));
        assert_eq!(defaults.enabled, Some(true));
    }

    #[test]
    fn neither_field_yields_all_none_defaults() {
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
"#;

        let entry = parse_entry(toml_text);

        // is_empty() -> reasoning_defaults() returns None.
        assert!(entry.reasoning_defaults().is_none());
    }

    #[test]
    fn thinking_none_maps_to_effort_none() {
        // The vendor-side string "none" is a valid effort level (it
        // tells the egress to disable reasoning). It must not collide
        // with absence-of-the-field semantics.
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "none"
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("none"));
    }

    #[test]
    fn thinking_empty_string_rejected_at_startup() {
        // Arrange: build a Config with one provider whose `thinking =
        // ""`.
        let toml_text = r#"
[providers.bad]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = ""
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");

        // Act
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("bad"), "msg should name provider: {msg}");
        assert!(
            msg.contains("non-empty"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_unknown_value_passthrough() {
        // Forward-compat: vendors add new effort levels without a
        // routectl release. Validator must not gate on a closed enum.
        let toml_text = r#"
[providers.future]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "ultra"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");

        let result = validate_reasoning_defaults(&cfg);

        assert!(result.is_ok(), "expected pass-through Ok, got {result:?}");
        let entry = cfg.providers.get("future").expect("provider parsed");
        let defaults = entry.reasoning_defaults().expect("defaults present");
        assert_eq!(defaults.thinking.as_deref(), Some("ultra"));
    }

    #[test]
    fn thinking_whitespace_only_rejected() {
        // Arrange: a thinking value that's only whitespace would parse
        // upstream as "no effort" but with all the wire overhead of
        // emitting the field. Caught at startup with a precise error
        // pointing the operator at the offending provider.
        let toml_text = r#"
[providers.spacey]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "   "
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");

        // Act
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("spacey"), "msg should name provider: {msg}");
        assert!(
            msg.contains("whitespace-only"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_with_control_characters_rejected() {
        // Arrange: a tab character (ASCII 0x09) in the thinking value.
        // Effort strings must not contain ASCII control characters --
        // they would corrupt log lines and wire bodies. Non-ASCII
        // (printable Unicode) is fine and stays out of the validator's
        // way for forward-compat with future vendor vocabularies.
        let toml_text = "\
[providers.tabby]\n\
type = \"anthropic-api\"\n\
api_key_ref = \"literal:k\"\n\
thinking = \"hi\\tgh\"\n\
";
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");

        // Act
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("tabby"), "msg should name provider: {msg}");
        assert!(
            msg.contains("control"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_exceeding_64_bytes_rejected() {
        // Arrange: a 65-character effort string. Real effort tokens
        // are short ("minimal", "low", "medium", "high", "xhigh",
        // "max", "none"); 64 bytes is well above any realistic value
        // and catches accidental paste of a full prompt or path.
        let long_value = "a".repeat(65);
        let toml_text = format!(
            r#"
[providers.verbose]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "{long_value}"
"#
        );
        let cfg: Config = toml::from_str(&toml_text).expect("toml parse");

        // Act
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("verbose"), "msg should name provider: {msg}");
        assert!(msg.contains("65 bytes"), "msg should report length: {msg}");
        assert!(msg.contains("max 64"), "msg should report cap: {msg}");
    }

    #[test]
    fn validate_reports_all_offending_providers() {
        // Arrange: two providers with different validation failures.
        // The validator must accumulate both errors instead of
        // bailing on the first one -- otherwise the operator has to
        // restart routectl once per offending provider to see them
        // all.
        let toml_text = r#"
[providers.empty_p]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = ""

[providers.spacey_p]
type = "anthropic-api"
api_key_ref = "literal:k"
thinking = "   "
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");

        // Act
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");

        // Assert: BOTH provider names appear in the consolidated error.
        let msg = err.to_string();
        assert!(msg.contains("empty_p"), "msg should name empty_p: {msg}");
        assert!(msg.contains("spacey_p"), "msg should name spacey_p: {msg}");
    }

    #[test]
    fn parse_openai_compat_variant_carries_defaults() {
        let toml_text = r#"
type = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
reasoning_dialect = "vllm"
thinking = "low"
enabled = true
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("low"));
        assert_eq!(defaults.enabled, Some(true));
        assert!(matches!(entry, ProviderEntry::OpenaiCompat { .. }));
    }

    #[test]
    fn parse_anthropic_api_variant_carries_defaults() {
        let toml_text = r#"
type = "anthropic-api"
api_key_ref = "literal:k"
adaptive_thinking = true
thinking = "xhigh"
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("xhigh"));
        assert!(matches!(entry, ProviderEntry::AnthropicApi { .. }));
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn parse_openai_responses_variant_carries_defaults() {
        let toml_text = r#"
type = "openai-responses"
api_key_ref = "literal:k"
account_id_ref = "literal:00000000-0000-0000-0000-000000000000"
auth_kind = "chatgpt-oauth"
thinking = "high"
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("high"));
        assert!(matches!(entry, ProviderEntry::OpenaiResponses { .. }));
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn parse_bedrock_variant_carries_defaults() {
        let toml_text = r#"
type = "bedrock"
region = "us-east-1"
model_id = "anthropic.claude-haiku-4-5"
creds = { kind = "default-chain" }
thinking = "minimal"
enabled = true
"#;

        let entry = parse_entry(toml_text);
        let defaults = entry.reasoning_defaults().expect("defaults present");

        assert_eq!(defaults.thinking.as_deref(), Some("minimal"));
        assert_eq!(defaults.enabled, Some(true));
        assert!(matches!(entry, ProviderEntry::Bedrock { .. }));
    }
}

impl RetryPolicy {
    /// Resolve the retry cap for a given upstream HTTP status code.
    /// Returns 0 for non-retryable errors.
    ///
    /// Note: `retry_on_5xx` only applies to 5xx codes that are ALSO
    /// listed in `fallback_on_status`. A 5xx code an operator removed
    /// from `fallback_on_status` (e.g. 501 "not implemented") is
    /// treated as non-retryable here AND as non-fallbackable in
    /// `should_fallback`, so it propagates immediately to the caller.
    /// This is intentional: an operator who removes a status from
    /// `fallback_on_status` is asking routectl to surface the error
    /// verbatim, and silently retrying anyway would contradict that.
    pub fn retries_for_status(&self, status: u16) -> u32 {
        match status {
            0 => self.retry_on_network.unwrap_or(self.max_attempts),
            429 => self.retry_on_429.unwrap_or(self.max_attempts),
            s if (500..600).contains(&s) && self.fallback_on_status.contains(&s) => {
                self.retry_on_5xx.unwrap_or(self.max_attempts)
            }
            _ => 0,
        }
    }

    /// Maximum attempts a single provider can ever consume regardless
    /// of error class. The router uses this as a hard ceiling so a
    /// misconfigured policy can't loop forever.
    pub fn hard_retry_cap(&self) -> u32 {
        self.max_attempts
            .max(self.retry_on_429.unwrap_or(0))
            .max(self.retry_on_5xx.unwrap_or(0))
            .max(self.retry_on_network.unwrap_or(0))
            .max(1)
    }
}

fn default_max_attempts() -> u32 {
    2
}

fn default_backoff_ms() -> u64 {
    250
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

fn default_fallback_status() -> Vec<u16> {
    vec![408, 429, 500, 502, 503, 504]
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyCompat {
    #[default]
    Openrouter,
    Openai,
}
