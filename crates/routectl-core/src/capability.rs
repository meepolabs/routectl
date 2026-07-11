//! Capability-key vocabulary shared across routectl-core consumers.
//!
//! Single namespace source for the well-known capability keys: the
//! router's `feature_keys.rs` (alias-chain pre-filter) and the catalog's
//! capability priors both key off these same strings, so a learned
//! negative and a catalog-declared capability meet on identical keys.
//! The vocabulary is open-ended -- the catalog map and
//! `derive_feature_keys` both accept arbitrary string keys -- this
//! module documents only the well-known subset.

/// Feature key for web-search tool use.
pub const WEB_SEARCH: &str = "web_search";

/// Feature key for computer-use tool use.
pub const COMPUTER_USE: &str = "computer_use";

/// Feature key for requests that need constrained decoding -- either an
/// `output_config.format` structured output or a strict tool.
pub const STRUCTURED_OUTPUT: &str = "structured_output";

/// All well-known capability keys. Not exhaustive: both `derive_feature_keys`
/// and the catalog's capability map accept arbitrary tool-type strings
/// beyond this list; this slice documents the ones routectl itself knows
/// about.
pub const WELL_KNOWN_CAPABILITY_KEYS: &[&str] = &[WEB_SEARCH, COMPUTER_USE, STRUCTURED_OUTPUT];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_capability_keys_contains_each_const() {
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&WEB_SEARCH));
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&COMPUTER_USE));
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&STRUCTURED_OUTPUT));
    }
}
