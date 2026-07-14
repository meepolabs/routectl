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

/// Whether a learned negative came from a provider that names the
/// unsupported capability outright, or from an inferred free-text match.
///
/// The `as_str` tokens are a persisted contract: they land in the usage
/// ledger's `signal_tier` column and any future warm-rebuild replayer
/// reads them back. Changing a token silently re-tiers historical rows,
/// so the mapping is fixed forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTier {
    /// The provider's error identifies the unsupported capability
    /// directly (e.g. an openai-compat `unsupported_parameter` token).
    /// A single observation is enough to act.
    SelfIdentifying,
    /// The capability was inferred from a whole-phrase match against a
    /// free-text error body. Requires corroboration before acting.
    Inferred,
}

impl SignalTier {
    /// Stable ledger token for this tier. Forever contract -- see the
    /// type docs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfIdentifying => "self-identifying",
            Self::Inferred => "inferred",
        }
    }
}

/// Known top-level Bedrock Converse request-bag prefixes. A Converse
/// body nests capability fields inside these bags, so a validation-error
/// path such as `additionalModelRequestFields.anthropic_beta` names the
/// capability in the segment immediately after the bag -- not in a
/// deeper smithy segment.
const BEDROCK_REQUEST_BAG_PREFIXES: &[&str] = &["additionalModelRequestFields", "toolConfig"];

/// Normalize a raw capability key to the canonical form shared by the
/// learned-capability registry, the usage-ledger writer, and the future
/// warm-rebuild replayer -- all of which must key off identical strings.
///
/// Conservative by design (open namespace): every provider kind other
/// than the exact `bedrock` routing kind passes its key through
/// unchanged, so an unknown key a provider names is a first-class
/// citizen. `bedrock` is the sole `kind_str()` token for Bedrock; the
/// `bedrock-invoke` / `bedrock-converse` labels are tracing-only and
/// never reach this seam, so the match is an exact literal.
///
/// Bedrock validation errors reference dotted smithy field paths where
/// the capability name sits at the HEAD of the path and deeper segments
/// are structural nesting (`toolSpec`, `inputSchema`, `json`). A Bedrock
/// key is therefore reduced to:
///
/// - the segment immediately FOLLOWING a known top-level request-bag
///   prefix (`additionalModelRequestFields`, `toolConfig`) when the path
///   starts with one -- e.g. `additionalModelRequestFields.anthropic_beta`
///   -> `anthropic_beta`, `toolConfig.tools.toolSpec.inputSchema` ->
///   `tools`;
/// - otherwise the FIRST non-empty segment -- e.g. `.anthropic_beta` ->
///   `anthropic_beta`, a bare `web_search` -> `web_search`.
///
/// When the path carries no non-empty segment at all (empty string, or
/// only separators like `...`), the raw string is returned verbatim so
/// distinct garbage never collapses onto one shared persisted key. This
/// keeps the reduction idempotent: the result of one pass has no leading
/// bag prefix, so a second pass returns it unchanged.
pub fn normalize_capability_key(raw: &str, provider_kind: &str) -> String {
    if provider_kind == "bedrock" {
        return normalize_bedrock_key(raw);
    }
    raw.to_string()
}

fn normalize_bedrock_key(raw: &str) -> String {
    let mut segments = raw.split('.').filter(|segment| !segment.is_empty());
    let Some(first) = segments.next() else {
        return raw.to_string();
    };
    if BEDROCK_REQUEST_BAG_PREFIXES.contains(&first)
        && let Some(following) = segments.next()
    {
        return following.to_string();
    }
    first.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_capability_keys_contains_each_const() {
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&WEB_SEARCH));
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&COMPUTER_USE));
        assert!(WELL_KNOWN_CAPABILITY_KEYS.contains(&STRUCTURED_OUTPUT));
    }

    #[test]
    fn self_identifying_tier_maps_to_contract_token() {
        // Arrange
        let tier = SignalTier::SelfIdentifying;

        // Act
        let token = tier.as_str();

        // Assert
        assert_eq!(token, "self-identifying");
    }

    #[test]
    fn inferred_tier_maps_to_contract_token() {
        // Arrange
        let tier = SignalTier::Inferred;

        // Act
        let token = tier.as_str();

        // Assert
        assert_eq!(token, "inferred");
    }

    #[test]
    fn extracts_bedrock_bag_field_after_known_prefix() {
        // Arrange
        let raw = "additionalModelRequestFields.anthropic_beta";

        // Act
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "anthropic_beta");
    }

    #[test]
    fn extracts_bag_field_from_deeply_nested_bedrock_path() {
        // A deep Converse path resolves to the segment right after the
        // `toolConfig` bag, not the smithy leaf (`inputSchema` would match
        // nothing in the feature-key vocabulary).
        let raw = "toolConfig.tools.toolSpec.inputSchema";

        // Act -- `bedrock` is the only routing kind Bedrock ever presents.
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "tools");
    }

    #[test]
    fn bedrock_key_without_dots_passes_through() {
        // Arrange
        let raw = "web_search";

        // Act
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "web_search");
    }

    #[test]
    fn bedrock_leading_dot_yields_first_nonempty_segment() {
        // Arrange
        let raw = ".anthropic_beta";

        // Act
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "anthropic_beta");
    }

    #[test]
    fn bedrock_trailing_dot_after_bag_prefix_extracts_following_segment() {
        // Arrange
        let raw = "toolConfig.tools.";

        // Act
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "tools");
    }

    #[test]
    fn bedrock_all_dots_falls_back_to_raw() {
        // No non-empty segment -- keep the raw token so distinct garbage
        // never collapses onto one shared persisted key.
        let raw = "...";

        // Act
        let key = normalize_capability_key(raw, "bedrock");

        // Assert
        assert_eq!(key, "...");
    }

    #[test]
    fn non_bedrock_provider_passes_dotted_key_through_unchanged() {
        // Arrange -- open namespace: only Bedrock strips.
        let raw = "additionalModelRequestFields.anthropic_beta";

        // Act
        let key = normalize_capability_key(raw, "anthropic-api");

        // Assert
        assert_eq!(key, "additionalModelRequestFields.anthropic_beta");
    }

    #[test]
    fn openai_compat_passes_self_identifying_token_through_unchanged() {
        // Arrange
        let raw = "unsupported_parameter";

        // Act
        let key = normalize_capability_key(raw, "openai-compat");

        // Assert
        assert_eq!(key, "unsupported_parameter");
    }

    #[test]
    fn unknown_provider_kind_passes_key_through_unchanged() {
        // Arrange -- an unknown provider kind is a first-class namespace.
        let raw = "some.vendor.capability";

        // Act
        let key = normalize_capability_key(raw, "future-vendor");

        // Assert
        assert_eq!(key, "some.vendor.capability");
    }

    #[test]
    fn empty_key_returns_empty_for_any_provider() {
        // Arrange / Act / Assert
        assert_eq!(normalize_capability_key("", "bedrock"), "");
        assert_eq!(normalize_capability_key("", "anthropic-api"), "");
    }

    #[test]
    fn tracing_only_bedrock_labels_do_not_strip() {
        // `bedrock-invoke` / `bedrock-converse` are tracing labels, never
        // routing kinds; only the exact `bedrock` kind reaches this seam,
        // so these must pass through untouched.
        let raw = "toolConfig.tools.toolSpec.inputSchema";
        assert_eq!(normalize_capability_key(raw, "bedrock-invoke"), raw);
        assert_eq!(normalize_capability_key(raw, "bedrock-converse"), raw);
    }

    #[test]
    fn bedrock_normalization_is_idempotent() {
        // Arrange
        let raw = "additionalModelRequestFields.anthropic_beta";

        // Act
        let once = normalize_capability_key(raw, "bedrock");
        let twice = normalize_capability_key(&once, "bedrock");

        // Assert
        assert_eq!(once, twice);
    }

    #[test]
    fn deeply_nested_bedrock_normalization_is_idempotent() {
        // Arrange -- the new bag-following rule must still fixpoint.
        let raw = "toolConfig.tools.toolSpec.inputSchema";

        // Act
        let once = normalize_capability_key(raw, "bedrock");
        let twice = normalize_capability_key(&once, "bedrock");

        // Assert
        assert_eq!(once, "tools");
        assert_eq!(once, twice);
    }

    #[test]
    fn bedrock_raw_fallback_is_idempotent() {
        // Arrange -- a segment-less path returns raw; a second pass must
        // return the same raw.
        let raw = "...";

        // Act
        let once = normalize_capability_key(raw, "bedrock");
        let twice = normalize_capability_key(&once, "bedrock");

        // Assert
        assert_eq!(once, "...");
        assert_eq!(once, twice);
    }

    #[test]
    fn passthrough_normalization_is_idempotent() {
        // Arrange
        let raw = "some.vendor.capability";

        // Act
        let once = normalize_capability_key(raw, "anthropic-api");
        let twice = normalize_capability_key(&once, "anthropic-api");

        // Assert
        assert_eq!(once, twice);
    }

    #[test]
    fn reexported_from_crate_root() {
        // Arrange / Act -- pin the public re-export surface.
        let key = crate::normalize_capability_key("toolConfig.tools", "bedrock");
        let tier = crate::SignalTier::Inferred;

        // Assert
        assert_eq!(key, "tools");
        assert_eq!(tier.as_str(), "inferred");
    }
}
