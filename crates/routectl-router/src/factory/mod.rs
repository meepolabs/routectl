//! Provider factory root + re-exports.

mod build;
mod validate;
mod warnings;

#[cfg(test)]
pub use build::resolve_max_thinking_entry_bytes_for_test;
pub use build::{
    BuildOptions, apply_catalog_overlay, build_provider, build_provider_with_options,
    build_resolved_models,
};
#[cfg(feature = "bedrock")]
pub use validate::validate_bedrock_global_config;
#[cfg(feature = "bedrock")]
pub use validate::validate_provider_bedrock_mantle;
pub use validate::{
    ConfigValidation, collect_config_validation, validate_alias_chain_targets,
    validate_alias_patterns, validate_class_policy, validate_mitm_config,
    validate_provider_credential_sources, validate_reasoning_defaults, validate_registry_patterns,
};
pub use warnings::class_policy_warnings;
