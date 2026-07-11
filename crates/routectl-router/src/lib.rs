//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.

pub mod catalog;
pub(crate) mod catalog_baked;
#[doc(hidden)]
pub mod catalog_codegen;
pub(crate) mod catalog_codegen_selectors;
pub mod catalog_import;
pub(crate) mod catalog_import_state;
pub mod catalog_overlay;
pub(crate) mod catalog_state;
pub mod config;
pub mod config_migrate;
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

pub use catalog::{
    BakedPricingRow, CachePricingOverride, CachePricingSelector, CatalogRow, EffectiveRow, Source,
    baked_table_rows, is_stale_today, lookup, lookup_baked_with_overrides, lookup_overlay_cell,
    lookup_with_overrides, merge, stale_after_days, validate_overrides,
};
pub use catalog_baked::{CATALOG_SNAPSHOT_DATE, CATALOG_VERSION};
pub use catalog_import::{
    CandidateOrigin, DiffRow, ExistingCell, ImportCandidate, ImportDiff, ShrinkCounts,
    ShrinkVerdict, ShrunkFamily, ShrunkSource, SkippedSelector, baked_row_map, baked_shrink_counts,
    build_import_candidate, candidate_shrink_counts, diff_overlay, shrink_guard,
};
pub use catalog_import_state::{
    CatalogImportState, CatalogImportStateError, default_path as catalog_import_state_default_path,
    load_baseline as load_catalog_import_baseline,
    persist_baseline as persist_catalog_import_baseline,
};
pub use catalog_overlay::{
    CATALOG_OVERLAY_SCHEMA_VERSION, CatalogOverlay, OverlayCell, OverlayError, OverlaySource,
    default_path as overlay_default_path, load as load_catalog_overlay, overlay_revision,
    save as save_catalog_overlay, with_overlay_write_lock,
};
pub use catalog_state::{
    ImpactClass, ImpactField, check_drift_and_persist_state, classify_field,
    default_path as catalog_state_default_path, escalate,
    selector_key as catalog_state_selector_key,
};
pub use config::{
    AliasValue, CURRENT_CONFIG_VERSION, CacheCapability, CacheConfig, Config, HistoryReasoning,
    LogConfig, MitmConfig, ModelEntry, PricingConfig, ProviderEntry, ProviderRuntimePolicy,
    ReasoningDialect, ReductionConfig, RegistryEntry, RetryPolicy, ServerAuth, ServerConfig,
    TrimConfig, UsageConfig, VersionTooNewError, preflight_config_version,
    validate_cache_pricing_retired,
};
#[cfg(feature = "bedrock")]
pub use config::{BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig};
pub use config_migrate::{MigrationError, MigrationOutcome, migrate_v1_to_v2};
pub use context_trim::{
    ElisionMark, NearLosslessMarks, SteadyStateTrimParams, SteadyStateTrimPlan, apply_trim_plan,
    collect_near_lossless_marks, near_lossless_candidate, propose_steady_state_trim,
    trimmed_prefix_fingerprint,
};
pub use cost_gate::{GateDecision, KeepReason, PrefixReductionCandidate, break_even_k, evaluate};
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_global_config;
pub use factory::{
    BuildOptions, apply_catalog_overlay, build_provider, build_provider_with_options,
    build_resolved_models, validate_alias_chain_targets, validate_alias_patterns,
    validate_mitm_config, validate_reasoning_defaults, validate_registry_patterns,
    validate_retry_policy,
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
