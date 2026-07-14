//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.

pub mod activation;
pub mod capability_matcher;
pub mod catalog;
pub(crate) mod catalog_baked;
#[doc(hidden)]
pub mod catalog_codegen;
pub(crate) mod catalog_codegen_selectors;
pub mod catalog_import;
pub(crate) mod catalog_import_state;
pub mod catalog_overlay;
pub(crate) mod catalog_state;
pub mod class_policy;
pub mod config;
pub mod config_effective;
pub mod config_error;
pub mod config_locate;
pub mod config_migrate;
pub mod config_path;
pub mod config_write;
pub mod context_trim;
pub mod cost_gate;
pub mod doctor;
pub mod factory;
pub(crate) mod feature_keys;
pub mod glob;
pub mod k_estimator;
pub mod learned_capability;
pub mod resolved;
pub mod router;
pub mod runtime_state;
pub mod schema_gen;
pub(crate) mod seat_pool;

pub use activation::{
    ActivatedChange, ActivationDelta, ActivationEntry, ActivationState, ActivationStatus,
    DeactivatedChange, UnresolvedReason, compute_activation, diff as diff_activation,
};
pub use catalog::{
    BakedPricingRow, CachePricingOverride, CachePricingSelector, CatalogRow, EffectiveRow, Source,
    baked_table_rows, is_cataloged_provider_kind, is_stale_today, lookup,
    lookup_baked_with_overrides, lookup_overlay_cell, lookup_with_overrides, merge,
    stale_after_days, validate_overrides,
};
pub use catalog_baked::{CATALOG_SNAPSHOT_DATE, CATALOG_VERSION};
pub use catalog_import::{
    CandidateOrigin, DiffRow, ExistingCell, ImportCandidate, ImportDiff, ShrinkCounts,
    ShrinkVerdict, ShrunkFamily, ShrunkSource, SkippedSelector, baked_row_map, baked_shrink_counts,
    build_import_candidate, candidate_shrink_counts, diff_has_no_effective_change, diff_overlay,
    shrink_guard,
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
    AliasValue, CURRENT_CONFIG_VERSION, CacheCapability, CacheConfig, Config, ConfigVersionError,
    HistoryReasoning, LegacyMitmCredentialSourceError, LogConfig, MitmConfig, ModelEntry,
    PricingConfig, ProviderEntry, ProviderRuntimePolicy, ReasoningDialect, ReductionConfig,
    RegistryEntry, RetryPolicy, ServerAuth, ServerConfig, TrimConfig, UsageConfig,
    VersionTooNewError, preflight_config_version, preflight_legacy_mitm_credential_source,
    validate_cache_pricing_retired,
};
#[cfg(feature = "bedrock")]
pub use config::{BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig};
pub use config_effective::{
    ClassPolicyCell, ClassPolicySource, EffectiveView, ModelCell, derive_effective_view,
};
pub use config_error::parse_config;
pub use config_locate::locate_dotted_path;
pub use config_migrate::{
    MigrateError, MigrationError, MigrationOutcome, Refusal, RefusalSource, StepOutcome,
    V1Migration, migrate_to_current, migrate_v1_to_v2, migrate_v2_to_v3,
};
pub use config_path::{PathError, PathShape, validate_config_path};
pub use config_write::{ConfigWriteError, EditOutcome, EditResult, edit_config_toml};
pub use context_trim::{
    ElisionMark, NearLosslessMarks, SteadyStateTrimParams, SteadyStateTrimPlan, apply_trim_plan,
    collect_near_lossless_marks, near_lossless_candidate, propose_steady_state_trim,
    trimmed_prefix_fingerprint,
};
pub use cost_gate::{GateDecision, KeepReason, PrefixReductionCandidate, break_even_k, evaluate};
pub use doctor::{
    DoctorPanels, DoctorReport, Finding, ProbeOutcome, Status, WouldTrimPanel, overall_exit,
};
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_global_config;
pub use factory::{
    BuildOptions, ConfigValidation, apply_catalog_overlay, build_provider,
    build_provider_with_options, build_resolved_models, class_policy_warnings,
    collect_config_validation, validate_alias_chain_targets, validate_alias_patterns,
    validate_class_policy, validate_mitm_config, validate_provider_credential_sources,
    validate_reasoning_defaults, validate_registry_patterns,
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
