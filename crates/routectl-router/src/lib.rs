//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.

pub mod cache_pricing;
pub mod config;
pub mod context_trim;
pub mod cost_gate;
pub mod factory;
pub(crate) mod feature_keys;
pub mod glob;
pub mod k_estimator;
pub mod resolved;
pub mod router;
pub mod runtime_state;
pub(crate) mod seat_pool;

pub use cache_pricing::{
    BakedPricingRow, CachePricingOverride, CachePricingRow, CachePricingSelector, baked_table_rows,
    is_stale_today, lookup, lookup_with_overrides, stale_after_days, validate_overrides,
};
pub use config::{
    AliasValue, CacheCapability, CacheConfig, Config, HistoryReasoning, LogConfig, ModelEntry,
    PricingConfig, ProviderEntry, ProviderRuntimePolicy, ReasoningDialect, ReductionConfig,
    RegistryEntry, RetryPolicy, ServerAuth, ServerConfig, UsageConfig,
};
#[cfg(feature = "bedrock")]
pub use config::{BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig};
pub use context_trim::{
    ElisionMark, SteadyStateTrimParams, SteadyStateTrimPlan, apply_trim_plan,
    propose_steady_state_trim, trimmed_prefix_fingerprint,
};
pub use cost_gate::{GateDecision, KeepReason, PrefixReductionCandidate, break_even_k, evaluate};
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_global_config;
pub use factory::{
    BuildOptions, build_provider, build_provider_with_options, build_resolved_models,
    validate_alias_chain_targets, validate_alias_patterns, validate_reasoning_defaults,
    validate_registry_patterns, validate_retry_policy,
};
pub use glob::{AliasPattern, PrefixIndex};
pub use k_estimator::{
    Confidence, EstimateSource, K_SESSION_CAPACITY, KEstimate, KEstimator, KQuery, KSessionKey,
    KSessionStore, KSessionWindow, LedgerBackedK, LedgerReader, LedgerSampleRow, Sample,
    ShadowOutcome, ShadowStore, rebuild_into,
};
pub use resolved::ResolvedModel;
pub use router::{
    ALIAS_MAX_RECURSION_DEPTH, DispatchMeta, Dispatched, DispatchedStream, Router, RouterOptions,
};
