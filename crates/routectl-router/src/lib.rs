//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.

pub mod config;
pub mod factory;
pub mod router;
pub mod runtime_state;

pub use config::{
    AliasEntry, Config, IngressConfig, IngressShape, LegacyCompat, ProviderEntry, ProviderKind,
    ProviderRuntimePolicy, ReasoningDialect, RetryPolicy, ServerAuth, ServerConfig,
};
#[cfg(feature = "bedrock")]
pub use config::{BedrockApiShapeConfig, BedrockCredsConfig};
pub use factory::build_provider;
pub use router::{Router, RouterOptions};
