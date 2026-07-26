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

/// Which detection phase attributed a learned negative.
///
/// The `as_str` tokens are a persisted contract: they land in the
/// learned-capability ledger and any future warm-rebuild replayer reads
/// them back. Changing a token silently re-attributes historical rows,
/// so the mapping is fixed forever. `parse` is open-set-tolerant: an
/// unknown token yields `None` rather than panicking, so a ledger row
/// written by a newer phase vocabulary never crashes an older replayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePhase {
    /// Wire-token strip phase: a droppable capability named directly by
    /// the provider's validation error.
    F1,
    /// Feature-naming phase: a non-droppable capability inferred from a
    /// per-provider deterministic pattern.
    F2,
    /// Positive-detection phase.
    F3,
}

impl FailurePhase {
    /// Stable ledger token for this phase. Forever contract -- see the
    /// type docs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F1 => "f1",
            Self::F2 => "f2",
            Self::F3 => "f3",
        }
    }

    /// Open-set-tolerant parse of a persisted phase token. Unknown
    /// tokens yield `None` -- never a panic, never a silent default.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "f1" => Some(Self::F1),
            "f2" => Some(Self::F2),
            "f3" => Some(Self::F3),
            _ => None,
        }
    }
}

/// Whether the evidence behind a learned negative came from live traffic
/// or from an out-of-band probe.
///
/// The `as_str` tokens are a persisted contract read back by any future
/// warm-rebuild replayer, so the mapping is fixed forever. `parse` is
/// open-set-tolerant: an unknown token yields `None` rather than
/// panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    /// Observed on a real client request in flight.
    Live,
    /// Observed by a routectl-issued probe.
    Probe,
}

impl EvidenceSource {
    /// Stable ledger token for this source. Forever contract -- see the
    /// type docs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Probe => "probe",
        }
    }

    /// Open-set-tolerant parse of a persisted source token. Unknown
    /// tokens yield `None` -- never a panic, never a silent default.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "live" => Some(Self::Live),
            "probe" => Some(Self::Probe),
            _ => None,
        }
    }
}

/// The derived read-model verdict for a capability against a target.
///
/// This is a DERIVED view: only the negative verdicts persist in the
/// ledger; `Assumed` and `Unknown` are computed at read time. The
/// `as_str` tokens are a forever contract shared with any warm-rebuild
/// replayer. `Assumed(bool)` maps to `"assumed"` regardless of the bool
/// -- the bool is the prior's truthiness, not part of the token; the
/// phase inside `LearnedBroken` is likewise carried in a sibling ledger
/// field, not encoded in the verdict token. Reconstruct a persisted
/// verdict through `from_parts`, which reads those sibling columns; it is
/// open-set-tolerant so an unrecognized token never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No learned evidence yet; the bool carries the catalog prior's
    /// truthiness.
    Assumed(bool),
    /// Confirmed working by positive detection.
    VerifiedWorking,
    /// Learned unsupported, attributed to the carried phase.
    LearnedBroken(FailurePhase),
    /// A learned negative the operator chose to ignore.
    SuspectIgnored,
    /// No signal in either direction.
    Unknown,
}

impl Verdict {
    /// Stable ledger token for this verdict. Forever contract -- see the
    /// type docs. The bool in `Assumed` and the phase in `LearnedBroken`
    /// are not encoded here; they live in sibling ledger fields.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assumed(_) => "assumed",
            Self::VerifiedWorking => "verified",
            Self::LearnedBroken(_) => "broken",
            Self::SuspectIgnored => "suspect",
            Self::Unknown => "unknown",
        }
    }

    /// The attributed phase when this verdict is a learned negative;
    /// `None` for every other variant.
    pub const fn broken_phase(self) -> Option<FailurePhase> {
        match self {
            Self::LearnedBroken(phase) => Some(phase),
            _ => None,
        }
    }

    /// Reconstruct a verdict from a persisted ledger row: the verdict
    /// token plus the sibling `phase` and `prior` columns that carry the
    /// data the token itself does not encode. This is the documented
    /// entry point for ledger replay -- open-set-tolerant, so an
    /// unrecognized token yields `Unknown` and never panics.
    ///
    /// A data-carrying token whose sibling column is absent degrades to
    /// `Unknown` rather than fabricating a value: a `"broken"` row with
    /// no phase or an `"assumed"` row with no prior is malformed, and
    /// guessing a phase or truthiness would report a type-correct but
    /// wrong verdict downstream.
    pub fn from_parts(token: &str, phase: Option<FailurePhase>, prior: Option<bool>) -> Self {
        match token {
            "assumed" => match prior {
                Some(prior) => Self::Assumed(prior),
                None => Self::Unknown,
            },
            "verified" => Self::VerifiedWorking,
            "broken" => match phase {
                Some(phase) => Self::LearnedBroken(phase),
                None => Self::Unknown,
            },
            "suspect" => Self::SuspectIgnored,
            _ => Self::Unknown,
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

    #[test]
    fn failure_phase_tokens_are_pinned() {
        assert_eq!(FailurePhase::F1.as_str(), "f1");
        assert_eq!(FailurePhase::F2.as_str(), "f2");
        assert_eq!(FailurePhase::F3.as_str(), "f3");
    }

    #[test]
    fn failure_phase_round_trips_through_its_token() {
        for phase in [FailurePhase::F1, FailurePhase::F2, FailurePhase::F3] {
            assert_eq!(FailurePhase::parse(phase.as_str()), Some(phase));
        }
    }

    #[test]
    fn failure_phase_parse_rejects_garbage_without_panic() {
        assert_eq!(FailurePhase::parse("f4"), None);
        assert_eq!(FailurePhase::parse(""), None);
        assert_eq!(FailurePhase::parse("F1"), None);
    }

    #[test]
    fn evidence_source_tokens_are_pinned() {
        assert_eq!(EvidenceSource::Live.as_str(), "live");
        assert_eq!(EvidenceSource::Probe.as_str(), "probe");
    }

    #[test]
    fn evidence_source_round_trips_through_its_token() {
        for source in [EvidenceSource::Live, EvidenceSource::Probe] {
            assert_eq!(EvidenceSource::parse(source.as_str()), Some(source));
        }
    }

    #[test]
    fn evidence_source_parse_rejects_garbage_without_panic() {
        assert_eq!(EvidenceSource::parse("synthetic"), None);
        assert_eq!(EvidenceSource::parse(""), None);
    }

    #[test]
    fn verdict_tokens_are_pinned() {
        assert_eq!(Verdict::Assumed(true).as_str(), "assumed");
        assert_eq!(Verdict::Assumed(false).as_str(), "assumed");
        assert_eq!(Verdict::VerifiedWorking.as_str(), "verified");
        assert_eq!(Verdict::LearnedBroken(FailurePhase::F1).as_str(), "broken");
        assert_eq!(Verdict::LearnedBroken(FailurePhase::F2).as_str(), "broken");
        assert_eq!(Verdict::SuspectIgnored.as_str(), "suspect");
        assert_eq!(Verdict::Unknown.as_str(), "unknown");
    }

    #[test]
    fn verdict_from_parts_round_trips_with_sibling_columns() {
        // Assumed carries its prior truthiness via the prior column.
        assert_eq!(
            Verdict::from_parts("assumed", None, Some(true)),
            Verdict::Assumed(true)
        );
        assert_eq!(
            Verdict::from_parts("assumed", None, Some(false)),
            Verdict::Assumed(false)
        );
        // Broken carries its phase via the phase column -- every phase,
        // not a fabricated default.
        for phase in [FailurePhase::F1, FailurePhase::F2, FailurePhase::F3] {
            assert_eq!(
                Verdict::from_parts("broken", Some(phase), None),
                Verdict::LearnedBroken(phase)
            );
        }
        // Data-free tokens ignore the sibling columns.
        assert_eq!(
            Verdict::from_parts("verified", None, None),
            Verdict::VerifiedWorking
        );
        assert_eq!(
            Verdict::from_parts("suspect", None, None),
            Verdict::SuspectIgnored
        );
        assert_eq!(Verdict::from_parts("unknown", None, None), Verdict::Unknown);
    }

    #[test]
    fn verdict_from_parts_is_open_set_tolerant() {
        assert_eq!(
            Verdict::from_parts("gibberish", Some(FailurePhase::F1), Some(true)),
            Verdict::Unknown
        );
        assert_eq!(Verdict::from_parts("", None, None), Verdict::Unknown);
        assert_eq!(
            Verdict::from_parts("VERIFIED", None, None),
            Verdict::Unknown
        );
    }

    #[test]
    fn verdict_from_parts_degrades_when_sibling_column_missing() {
        // A data-carrying token with its sibling column absent degrades
        // to Unknown rather than fabricating a phase or a prior.
        assert_eq!(Verdict::from_parts("broken", None, None), Verdict::Unknown);
        assert_eq!(Verdict::from_parts("assumed", None, None), Verdict::Unknown);
        // A stray sibling column on a data-free token is ignored.
        assert_eq!(
            Verdict::from_parts("broken", None, Some(true)),
            Verdict::Unknown
        );
    }

    #[test]
    fn learned_broken_exposes_its_phase() {
        assert_eq!(
            Verdict::LearnedBroken(FailurePhase::F2).broken_phase(),
            Some(FailurePhase::F2)
        );
        assert_eq!(Verdict::VerifiedWorking.broken_phase(), None);
        assert_eq!(Verdict::Assumed(true).broken_phase(), None);
        assert_eq!(Verdict::Unknown.broken_phase(), None);
    }

    #[test]
    fn verdict_types_reexported_from_crate_root() {
        // Pin the public re-export surface for the new contract types.
        let phase = crate::FailurePhase::F2;
        let source = crate::EvidenceSource::Probe;
        let verdict = crate::Verdict::LearnedBroken(phase);

        assert_eq!(phase.as_str(), "f2");
        assert_eq!(source.as_str(), "probe");
        assert_eq!(verdict.as_str(), "broken");
        assert_eq!(verdict.broken_phase(), Some(crate::FailurePhase::F2));
    }
}
