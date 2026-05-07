//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Default retry policy applied per-provider attempt.
    #[serde(default)]
    pub retry: RetryPolicy,

    /// Schema compatibility mode for the outward shape.
    /// `openrouter` (default): full reasoning_details surface.
    /// `openai`: strip routectl/openrouter extensions for paranoid clients.
    #[serde(default)]
    pub legacy_compat: LegacyCompat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind host. Defaults to localhost. Refuses non-loopback unless
    /// `--unsafe-public` is passed on the CLI.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8787
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProviderEntry {
    OpenaiCompat {
        base_url: String,
        /// Reference to the API key in the OS keychain, e.g. `keychain://routectl/deepseek`.
        /// Or, for dev convenience only, a literal `env://OPENAI_API_KEY`.
        api_key_ref: String,
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        #[serde(default)]
        default_extras: Option<serde_json::Value>,
        #[serde(default)]
        reasoning_dialect: ReasoningDialect,
    },
    AnthropicApi {
        api_key_ref: String,
        #[serde(default = "default_anthropic_base")]
        base_url: String,
        #[serde(default = "default_anthropic_version")]
        anthropic_version: String,
    },
    ClaudeCookie {
        session_ref: String,
        #[serde(default)]
        organization_id: Option<String>,
    },
    ChatgptCookie {
        session_ref: String,
    },
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
    ClaudeCookie,
    ChatgptCookie,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasEntry {
    /// Ordered list of `provider:model` targets. First entry is preferred.
    pub chain: Vec<String>,
    /// Optional override of the default retry policy for this alias.
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Max attempts per provider in the chain.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Initial backoff in ms.
    #[serde(default = "default_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff multiplier per attempt.
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Status codes that trigger fallback to the next provider in the chain
    /// (in addition to network errors).
    #[serde(default = "default_fallback_status")]
    pub fallback_on_status: Vec<u16>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            fallback_on_status: default_fallback_status(),
        }
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
