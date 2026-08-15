#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]
//! Router: alias resolution, fallback chains, retry policy.
//!
//! Reads a `Config` (typically loaded from `~/.config/routectl/config.toml`),
//! resolves an incoming request's `model` against the configured aliases, and
//! walks the fallback chain on `5xx`/`429`/timeout errors.
//!
//! Beyond dispatch, the crate owns the configuration schema and its
//! validation, the prompt-cache pricing catalog and its operator overlay,
//! the OAuth activation inventory, and the `doctor` report shapes.

pub(crate) mod activation;
pub(crate) mod anthropic_family;
pub(crate) mod calibration;
pub(crate) mod capability_detect;
pub(crate) mod capability_display;
pub(crate) mod capability_matcher;
pub(crate) mod capability_rebuild;
pub(crate) mod capability_strip;
pub mod catalog;
pub(crate) mod catalog_baked;
#[doc(hidden)]
pub mod catalog_codegen;
pub(crate) mod catalog_codegen_selectors;
pub(crate) mod catalog_import;
pub(crate) mod catalog_import_state;
pub(crate) mod catalog_overlay;
pub(crate) mod catalog_state;
pub mod class_policy;
pub mod config;
pub(crate) mod config_effective;
pub(crate) mod config_error;
pub(crate) mod config_locate;
pub(crate) mod config_migrate;
pub(crate) mod config_path;
pub(crate) mod config_write;
pub(crate) mod context_trim;
pub(crate) mod cost_gate;
pub(crate) mod doctor;
pub(crate) mod factory;
pub(crate) mod feature_keys;
pub(crate) mod glob;
pub(crate) mod k_estimator;
pub(crate) mod learned_capability;
pub(crate) mod learned_replay;
pub(crate) mod log_hash;
pub(crate) mod override_registry;
pub(crate) mod resolved;
pub mod router;
pub mod runtime_state;
pub mod schema_gen;
pub(crate) mod seat_pool;
#[cfg(test)]
pub(crate) mod test_secret;

pub use activation::{
    ActivatedChange, ActivationDelta, ActivationEntry, ActivationState, ActivationStatus,
    DeactivatedChange, UnresolvedReason, compute_activation, diff as diff_activation,
    provider_kind_for_oauth_id,
};
pub use calibration::{CalibrationLedgerReader, CalibrationLedgerRow, CalibrationRebuildSummary};
pub use capability_detect::{CapabilityObservation, DetectorContext, ObservationDirection, detect};
pub use capability_display::{DisplayVerdict, resolve_display_verdict};
pub use capability_matcher::resolve_requested_capability;
pub use capability_rebuild::{
    CapabilityEventRow, CapabilityLedgerReader, CapabilityRebuildSummary, ReplayTombstone,
    rebuild_capabilities_into,
};
pub use catalog::{
    BakedPricingRow, CachePricingOverride, CachePricingSelector, CatalogRow, EffectiveRow, Source,
    baked_table_rows, epoch_day_age, is_cataloged_provider_kind, is_stale_days,
    is_stale_days_today, is_stale_today, lookup, lookup_baked_with_overrides, lookup_overlay_cell,
    lookup_with_overrides, merge, stale_after_days, today_epoch_day, validate_overrides,
};
pub use catalog_baked::{CATALOG_SNAPSHOT_DATE, CATALOG_VERSION};
pub use catalog_import::{
    CandidateOrigin, DiffRow, ExistingCell, ImportCandidate, ImportDiff, ShrinkCounts,
    ShrinkVerdict, ShrunkFamily, ShrunkSource, SkipKind, SkippedSelector, baked_row_map,
    baked_shrink_counts, build_import_candidate, candidate_shrink_counts,
    diff_has_no_effective_change, diff_overlay, is_import_cell, shrink_guard,
};
pub use catalog_import_state::{
    CatalogImportState, CatalogImportStateError, default_path as catalog_import_state_default_path,
    load_baseline as load_catalog_import_baseline, load_last_import,
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
    AliasValue, CURRENT_CONFIG_VERSION, CacheCapability, CacheConfig, CalibrationConfig,
    CapabilityConfig, Config, ConfigVersionError, HistoryReasoning,
    LegacyMitmCredentialSourceError, LogConfig, MitmConfig, ModelEntry, OverrideEntry,
    PricingConfig, ProviderEntry, ProviderRuntimePolicy, ReasoningDialect, ReductionConfig,
    RegistryEntry, RetryPolicy, ServerAuth, ServerConfig, TrimConfig, UsageConfig,
    VersionTooNewError, WindowGateConfig, preflight_config_version,
    preflight_legacy_mitm_credential_source, validate_cache_pricing_retired,
};
#[cfg(feature = "bedrock")]
pub use config::{
    BedrockApiShapeConfig, BedrockCredsConfig, BedrockGlobalConfig, BedrockMantleConfig,
};
pub use config_effective::{
    AliasChain, ClassPolicyCell, ClassPolicySource, EffectiveView, ModelCell, ProviderCell,
    derive_effective_view,
};
pub use config_error::parse_config;
pub use config_locate::locate_dotted_path;
pub use config_migrate::{
    MigrateError, MigrationError, MigrationPlan, OverlayWrite, Refusal, RefusalSource, StepOutcome,
    WriteKind, apply_config_transforms, migrate_v2_to_v3, normalize_capability_overrides,
    plan_migration,
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
    CapabilityMatrixPanel, DoctorPanels, DoctorReport, Finding, MatrixAvailability, MatrixCell,
    MatrixLane, ProbeOutcome, Status, WouldTrimPanel, overall_exit,
};
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_creds_refs;
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_global_config;
#[cfg(feature = "bedrock")]
pub use factory::validate_bedrock_invoke_model_family;
#[cfg(feature = "bedrock")]
pub use factory::validate_provider_bedrock_mantle;
#[cfg(feature = "bedrock")]
pub use factory::validate_provider_openai_mantle;
pub use factory::{
    BuildOptions, ConfigValidation, apply_catalog_overlay, build_provider,
    build_provider_with_options, build_resolved_models, class_policy_warnings,
    codex_identity_warnings, collect_config_validation, resolved_codex_version,
    validate_alias_chain_targets, validate_alias_patterns, validate_class_policy,
    validate_codex_version, validate_mitm_config, validate_provider_credential_sources,
    validate_reasoning_defaults, validate_registry_patterns,
};
pub use glob::{AliasPattern, PrefixIndex};
pub use k_estimator::{
    Confidence, EstimateSource, K_SESSION_CAPACITY, KEstimate, KEstimator, KQuery, KSessionKey,
    KSessionStore, KSessionWindow, LedgerBackedK, LedgerReader, LedgerSampleRow, Sample,
    ShadowOutcome, ShadowStore, rebuild_into,
};
pub use learned_capability::{LearnedCapabilityRegistry, LearnedRegistryEntry};
pub use override_registry::{
    OverrideProvenance, OverrideRegistry, OverrideRow, OverrideVerdict,
    validate_capability_overrides,
};
pub use resolved::ResolvedModel;
pub use router::{
    ALIAS_MAX_RECURSION_DEPTH, DispatchMeta, Dispatched, DispatchedStream, Router, RouterOptions,
    class_debits,
};
