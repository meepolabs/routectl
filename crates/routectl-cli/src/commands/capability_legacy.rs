//! Shared detection of the deprecated capability-list keys superseded by
//! `[capability.overrides]`. Consumed by BOTH the `serve` load path (which
//! emits the deprecation WARN) and `doctor` (which renders the migrate
//! nudge), so the two never diverge on which keys count as present.
//!
//! Names only ever leave this module -- never the operator's list VALUES,
//! which can sit next to secrets in the config file.

use routectl_router::Config;

/// Legacy `unsupported_features` lists on providers / models.
pub(crate) const LEGACY_UNSUPPORTED_FEATURES: &str = "unsupported_features";
/// Legacy `[bedrock] allowed_betas` list.
pub(crate) const LEGACY_ALLOWED_BETAS: &str = "allowed_betas";
/// Legacy `[bedrock] allowed_body_fields` list.
pub(crate) const LEGACY_ALLOWED_BODY_FIELDS: &str = "allowed_body_fields";

/// The legacy capability-list key NAMES a parsed config still carries, in a
/// stable order. A key counts as present when its list is non-empty -- an
/// empty list is the pass-through default and needs no migration. Returns
/// names only; the operator's list VALUES never leave this function.
pub(crate) fn present_legacy_capability_keys(config: &Config) -> Vec<&'static str> {
    let mut keys = Vec::new();

    let unsupported_present = config
        .providers
        .values()
        .any(|p| !p.runtime().unsupported_features.is_empty())
        || config
            .models
            .values()
            .any(|m| !m.unsupported_features.is_empty());
    if unsupported_present {
        keys.push(LEGACY_UNSUPPORTED_FEATURES);
    }
    if !config.bedrock.allowed_betas.is_empty() {
        keys.push(LEGACY_ALLOWED_BETAS);
    }
    if !config.bedrock.allowed_body_fields.is_empty() {
        keys.push(LEGACY_ALLOWED_BODY_FIELDS);
    }

    keys
}
