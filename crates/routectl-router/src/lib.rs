//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.

pub mod config;
pub mod factory;
pub(crate) mod feature_keys;
pub mod glob;
pub mod resolved;
pub mod router;
pub mod runtime_state;

pub use config::{
    AliasValue, Config, HistoryReasoning, LogConfig, ModelEntry, ProviderEntry, ProviderKind,
    ProviderRuntimePolicy, ReasoningDialect, RetryPolicy, ServerAuth, ServerConfig,
};
#[cfg(feature = "bedrock")]
pub use config::{BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig};
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_global_config;
pub use factory::{
    build_provider, build_provider_with_options, build_resolved_models,
    validate_alias_chain_targets, validate_reasoning_defaults, validate_retry_policy, BuildOptions,
};
pub use glob::{AliasPattern, PrefixIndex};
pub use resolved::ResolvedModel;
pub use router::{Router, RouterOptions};
