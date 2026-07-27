//! Provider factory root + re-exports.

mod build;
#[cfg(feature = "openai-responses")]
mod installation_id;
mod validate;
mod warnings;

#[cfg(test)]
pub use build::resolve_max_thinking_entry_bytes_for_test;
pub use build::{
    BuildOptions, apply_catalog_overlay, build_provider, build_provider_with_options,
    build_resolved_models,
};
#[cfg(feature = "bedrock")]
pub use validate::validate_bedrock_creds_refs;
#[cfg(feature = "bedrock")]
pub use validate::validate_bedrock_global_config;
#[cfg(feature = "bedrock")]
pub use validate::validate_provider_bedrock_mantle;
#[cfg(feature = "bedrock")]
pub use validate::validate_provider_openai_mantle;
pub use validate::{
    ConfigValidation, collect_config_validation, resolved_codex_version,
    validate_alias_chain_targets, validate_alias_patterns, validate_class_policy,
    validate_codex_version, validate_mitm_config, validate_provider_credential_sources,
    validate_reasoning_defaults, validate_registry_patterns,
};
pub use warnings::{class_policy_warnings, codex_identity_warnings};
