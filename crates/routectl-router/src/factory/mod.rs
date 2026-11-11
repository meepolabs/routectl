//! Provider factory root + re-exports.

mod beta_floor_warnings;
mod build;
mod inert_provider;
#[cfg(feature = "openai-responses")]
mod installation_id;
mod validate;
mod validate_cloak;
#[cfg(feature = "bedrock")]
mod validate_region;
mod validate_state_keys;
mod warnings;

#[cfg(test)]
pub use build::apply_model_knobs;
pub use build::ipv4_compatible_embedded;
#[cfg(test)]
pub use build::resolve_max_thinking_entry_bytes_for_test;
pub use build::{
    BuildOptions, ResolvedModelBuild, build_provider, build_provider_with_options,
    build_resolved_models, build_resolved_models_reported, pool_member_refusals,
};
#[cfg(feature = "bedrock")]
pub use validate::validate_bedrock_creds_refs;
#[cfg(feature = "bedrock")]
pub use validate::validate_bedrock_invoke_model_family;
#[cfg(feature = "bedrock")]
pub use validate::validate_provider_bedrock_mantle;
#[cfg(feature = "bedrock")]
pub use validate::validate_provider_openai_mantle;
pub use validate::{
    ConfigValidation, MAX_POOL_MEMBERS, ModelProviderTarget, collect_config_validation,
    configured_codex_version, resolve_model_provider, resolved_codex_version,
    validate_alias_chain_targets, validate_alias_patterns, validate_class_policy,
    validate_codex_version, validate_managed_anthropic_credential, validate_mitm_config,
    validate_pools, validate_provider_credential_sources, validate_registry_patterns,
};
#[cfg(feature = "bedrock")]
pub use validate_region::validate_aws_regions;
pub use warnings::{
    auth_reserved_header_warnings, class_policy_warnings, cloudcode_host_warnings,
    cloudcode_model_warnings, codex_identity_warnings, per_block_breakpoint_warnings,
};
