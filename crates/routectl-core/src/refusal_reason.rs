//! Typed reason routectl refused a request locally, before or instead of
//! sending it upstream.
//!
//! The vocabulary is closed: a variant names one structural reason routectl
//! cannot proceed, and adding one is a deliberate register edit. Each token
//! is public vocabulary in logs and status output and keeps its spelling once
//! shipped. [`RefusalReason::failure_class`] gives every reason exactly one
//! [`FailureClass`]: an ingress-scope refusal (raised before any target is
//! chosen) is `Unknown`, an egress-scope refusal is `BadRequest`.

use crate::failure_class::FailureClass;

/// Why routectl refused a request locally. Readers of a persisted or rendered
/// token treat one they do not recognize as unknown
/// ([`RefusalReason::from_token`] returns `None`), never as an error.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalReason {
    /// The request carries more cache breakpoints than prompt caching allows,
    /// and dropping any would change what the caller asked to cache. Scope:
    /// ingress and egress. Class: `BadRequest`.
    CacheBreakpointCap,
    /// A longer-TTL cache breakpoint follows a shorter-TTL one, an order
    /// prompt caching rejects, and reordering would change the cached
    /// prefixes. Scope: ingress and egress. Class: `BadRequest`.
    CacheTtlOrder,
    /// A tool call or tool result has no id, so the target dialect has no
    /// way to pair the result with its call. Scope: egress. Class:
    /// `BadRequest`.
    ToolCallIdMissing,
    /// A field routectl must read has the wrong JSON type (for example a
    /// non-string system prompt or thinking display), so its meaning cannot
    /// be recovered. Scope: ingress. Class: `Unknown`.
    FieldType,
    /// The request references a stored previous response, which routectl
    /// holds no state for and cannot reconstruct. Scope: ingress. Class:
    /// `Unknown`.
    PreviousResponseId,
    /// The request names more beta features than routectl accepts on one
    /// request. Scope: ingress. Class: `Unknown`.
    BetaHeaderCap,
    /// An output schema nests deeper than routectl's schema repair walk is
    /// bounded to, so the schema cannot be repaired for the target. Scope:
    /// egress. Class: `BadRequest`.
    SchemaDepth,
    /// An output schema holds more nodes than routectl's schema repair walk
    /// is bounded to, so the schema cannot be repaired for the target.
    /// Scope: egress. Class: `BadRequest`.
    SchemaSize,
    /// The translated request no longer accounts for every tool turn of the
    /// source request, so forwarding it would silently drop part of the
    /// transcript. Scope: egress. Class: `BadRequest`.
    TranslationAccounting,
    /// Tool calls and tool results do not pair up, and the target must look
    /// up each result's call to emit a valid body. Scope: egress. Class:
    /// `BadRequest`.
    ToolPairing,
    /// The target's request type has no slot for a shape the request
    /// carries, so translation would have to drop it. Scope: egress. Class:
    /// `BadRequest`.
    Unrepresentable,
    /// The operator enabled the strict loss check and translation to the
    /// target would lose part of the request. Scope: egress. Class:
    /// `BadRequest`.
    StrictTranslation,
    /// The target forwards the caller's bearer credential and the request
    /// carries none, so there is nothing to authenticate with. Scope:
    /// egress. Class: `BadRequest`.
    ForwardedBearerMissing,
    /// The request's identity cloak cannot relocate caller content to where
    /// the target requires it without altering that content. Scope: egress.
    /// Class: `BadRequest`.
    CloakRelocation,
    /// A caller schema exceeds the ceiling routectl cleans it within for a
    /// Gemini target, so it cannot be reduced to a form Gemini accepts.
    /// Scope: egress. Class: `BadRequest`.
    GeminiSeed,
    /// The body is not JSON, not a JSON object, or does not deserialize into
    /// a request, so there is nothing to route. Scope: ingress. Class:
    /// `Unknown`.
    Unparseable,
}

impl RefusalReason {
    /// Every reason, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::CacheBreakpointCap,
        Self::CacheTtlOrder,
        Self::ToolCallIdMissing,
        Self::FieldType,
        Self::PreviousResponseId,
        Self::BetaHeaderCap,
        Self::SchemaDepth,
        Self::SchemaSize,
        Self::TranslationAccounting,
        Self::ToolPairing,
        Self::Unrepresentable,
        Self::StrictTranslation,
        Self::ForwardedBearerMissing,
        Self::CloakRelocation,
        Self::GeminiSeed,
        Self::Unparseable,
    ];

    /// The stable kebab-case token for this reason: the single source of the
    /// vocabulary in logs, metrics labels, and status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CacheBreakpointCap => "cache-breakpoint-cap",
            Self::CacheTtlOrder => "cache-ttl-order",
            Self::ToolCallIdMissing => "tool-call-id-missing",
            Self::FieldType => "field-type",
            Self::PreviousResponseId => "previous-response-id",
            Self::BetaHeaderCap => "beta-header-cap",
            Self::SchemaDepth => "schema-depth",
            Self::SchemaSize => "schema-size",
            Self::TranslationAccounting => "translation-accounting",
            Self::ToolPairing => "tool-pairing",
            Self::Unrepresentable => "unrepresentable",
            Self::StrictTranslation => "strict-translation",
            Self::ForwardedBearerMissing => "forwarded-bearer-missing",
            Self::CloakRelocation => "cloak-relocation",
            Self::GeminiSeed => "gemini-seed",
            Self::Unparseable => "unparseable",
        }
    }

    /// The reason a token names, matched exactly (no trimming or case
    /// folding). `None` for any token outside the vocabulary.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "cache-breakpoint-cap" => Some(Self::CacheBreakpointCap),
            "cache-ttl-order" => Some(Self::CacheTtlOrder),
            "tool-call-id-missing" => Some(Self::ToolCallIdMissing),
            "field-type" => Some(Self::FieldType),
            "previous-response-id" => Some(Self::PreviousResponseId),
            "beta-header-cap" => Some(Self::BetaHeaderCap),
            "schema-depth" => Some(Self::SchemaDepth),
            "schema-size" => Some(Self::SchemaSize),
            "translation-accounting" => Some(Self::TranslationAccounting),
            "tool-pairing" => Some(Self::ToolPairing),
            "unrepresentable" => Some(Self::Unrepresentable),
            "strict-translation" => Some(Self::StrictTranslation),
            "forwarded-bearer-missing" => Some(Self::ForwardedBearerMissing),
            "cloak-relocation" => Some(Self::CloakRelocation),
            "gemini-seed" => Some(Self::GeminiSeed),
            "unparseable" => Some(Self::Unparseable),
            _ => None,
        }
    }

    /// The failure class a refusal for this reason records. Ingress-scope
    /// reasons are `Unknown` (nothing dispatches, so the request ends);
    /// egress-scope reasons are `BadRequest`, so fallback may try a target
    /// that can represent the request.
    pub const fn failure_class(self) -> FailureClass {
        match self {
            Self::FieldType
            | Self::PreviousResponseId
            | Self::BetaHeaderCap
            | Self::Unparseable => FailureClass::Unknown,
            Self::CacheBreakpointCap
            | Self::CacheTtlOrder
            | Self::ToolCallIdMissing
            | Self::SchemaDepth
            | Self::SchemaSize
            | Self::TranslationAccounting
            | Self::ToolPairing
            | Self::Unrepresentable
            | Self::StrictTranslation
            | Self::ForwardedBearerMissing
            | Self::CloakRelocation
            | Self::GeminiSeed => FailureClass::BadRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::RefusalReason;
    use crate::failure_class::FailureClass;

    #[test]
    fn each_reason_round_trips_through_its_token() {
        let rows = [
            (RefusalReason::CacheBreakpointCap, "cache-breakpoint-cap"),
            (RefusalReason::CacheTtlOrder, "cache-ttl-order"),
            (RefusalReason::ToolCallIdMissing, "tool-call-id-missing"),
            (RefusalReason::FieldType, "field-type"),
            (RefusalReason::PreviousResponseId, "previous-response-id"),
            (RefusalReason::BetaHeaderCap, "beta-header-cap"),
            (RefusalReason::SchemaDepth, "schema-depth"),
            (RefusalReason::SchemaSize, "schema-size"),
            (
                RefusalReason::TranslationAccounting,
                "translation-accounting",
            ),
            (RefusalReason::ToolPairing, "tool-pairing"),
            (RefusalReason::Unrepresentable, "unrepresentable"),
            (RefusalReason::StrictTranslation, "strict-translation"),
            (
                RefusalReason::ForwardedBearerMissing,
                "forwarded-bearer-missing",
            ),
            (RefusalReason::CloakRelocation, "cloak-relocation"),
            (RefusalReason::GeminiSeed, "gemini-seed"),
            (RefusalReason::Unparseable, "unparseable"),
        ];
        assert_eq!(rows.len(), RefusalReason::ALL.len());
        for (reason, token) in rows {
            assert_eq!(reason.as_str(), token, "{reason:?}");

            let parsed = RefusalReason::from_token(reason.as_str());

            assert_eq!(parsed, Some(reason), "{token}");
        }
    }

    #[test]
    fn from_token_reads_a_token_outside_the_vocabulary_as_unknown() {
        let rows = [
            ("empty", ""),
            ("unknown", "access-denied"),
            ("case variant", "Schema-Depth"),
            ("padded leading", " schema-depth"),
            ("padded trailing", "schema-depth "),
        ];
        for (name, token) in rows {
            assert_eq!(RefusalReason::from_token(token), None, "{name}");
        }
    }

    #[test]
    fn ingress_reasons_are_unknown_and_egress_reasons_are_bad_request() {
        let rows = [
            (RefusalReason::CacheBreakpointCap, FailureClass::BadRequest),
            (RefusalReason::CacheTtlOrder, FailureClass::BadRequest),
            (RefusalReason::ToolCallIdMissing, FailureClass::BadRequest),
            (RefusalReason::FieldType, FailureClass::Unknown),
            (RefusalReason::PreviousResponseId, FailureClass::Unknown),
            (RefusalReason::BetaHeaderCap, FailureClass::Unknown),
            (RefusalReason::SchemaDepth, FailureClass::BadRequest),
            (RefusalReason::SchemaSize, FailureClass::BadRequest),
            (
                RefusalReason::TranslationAccounting,
                FailureClass::BadRequest,
            ),
            (RefusalReason::ToolPairing, FailureClass::BadRequest),
            (RefusalReason::Unrepresentable, FailureClass::BadRequest),
            (RefusalReason::StrictTranslation, FailureClass::BadRequest),
            (
                RefusalReason::ForwardedBearerMissing,
                FailureClass::BadRequest,
            ),
            (RefusalReason::CloakRelocation, FailureClass::BadRequest),
            (RefusalReason::GeminiSeed, FailureClass::BadRequest),
            (RefusalReason::Unparseable, FailureClass::Unknown),
        ];
        for &reason in RefusalReason::ALL {
            let Some((_, expected)) = rows.iter().find(|(row, _)| *row == reason) else {
                panic!("no expected class for {reason:?}");
            };

            assert_eq!(&reason.failure_class(), expected, "{reason:?}");
        }
    }

    fn is_kebab_case(token: &str) -> bool {
        !token.is_empty()
            && !token.starts_with('-')
            && !token.ends_with('-')
            && !token.contains("--")
            && token
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    #[test]
    fn all_holds_distinct_reasons_with_distinct_kebab_case_tokens() {
        let reasons: HashSet<RefusalReason> = RefusalReason::ALL.iter().copied().collect();
        let tokens: HashSet<&str> = RefusalReason::ALL.iter().map(|r| r.as_str()).collect();

        assert_eq!(reasons.len(), RefusalReason::ALL.len());
        assert_eq!(tokens.len(), RefusalReason::ALL.len());
        for &reason in RefusalReason::ALL {
            assert!(is_kebab_case(reason.as_str()), "{reason:?}");
        }
    }
}
