//! Maps a use-time upstream rejection to the capability it names.
//!
//! Data-driven, keyed by provider `kind` (the `class_policy` pattern), and
//! deliberately outside `routectl-core` -- the failure classifier stays
//! body-parse free, and this is not a `Provider` trait method. Two arms:
//!
//! - SELF-IDENTIFYING: the classifier already lifted the rejection to
//!   [`FailureClass::FeatureUnsupported`] and carries the capability token
//!   (openai-compat's `unsupported_parameter` family; Bedrock as its token
//!   table grows). The token is normalized to the canonical key. One
//!   observation is trustworthy.
//! - INFERRED: a generic [`FailureClass::BadRequest`] whose free-text
//!   `error.message` names a capability only in prose. Matched by
//!   whole-phrase equality (case-insensitive) against a small per-provider
//!   table of phrases grounded in real captured / documented 400 envelopes.
//!   Precision over recall: an unverified phrase is omitted, and a
//!   near-miss or embedded phrase never matches.
//!
//! Every other class, provider, or malformed body yields `None` -- the
//! matcher never manufactures a false positive.

use routectl_core::capability::{SignalTier, normalize_capability_key};
use routectl_core::error::Error;
use routectl_core::failure_class::{ClassifiedFailure, FailureClass};

/// A learned capability key. Feature keys are open-namespace strings
/// shared with the catalog prior and the alias-chain pre-filter.
type FeatureKey = String;

/// Capability key for assistant-message prefill. Open-namespace key (not
/// one of the well-known `routectl_core::capability` consts): the registry
/// namespace is open, so a capability the upstream names by prose is a
/// first-class key.
const PREFILL: &str = "prefill";

/// One inferred-rejection phrase: a free-text `error.message` equal to
/// `phrase` (case-insensitive, trimmed) names `capability`.
struct InferredPhrase {
    /// The verbatim upstream `error.message` phrase.
    phrase: &'static str,
    /// The capability key the phrase names.
    capability: &'static str,
}

/// Anthropic Messages API inferred-rejection phrases. Small by design;
/// each phrase is grounded in a real captured / documented 400 envelope
/// (sources cited in the module tests). Unverified capabilities wait.
const ANTHROPIC_INFERRED: &[InferredPhrase] = &[InferredPhrase {
    phrase: "Prefilling assistant messages is not supported for this model.",
    capability: PREFILL,
}];

/// Map a classified rejection to the capability it names and the signal
/// tier of that evidence, or `None` when the rejection names no capability
/// this matcher can attribute.
pub(crate) fn match_capability(
    provider_kind: &str,
    err: &Error,
    cf: &ClassifiedFailure,
) -> Option<(FeatureKey, SignalTier)> {
    match &cf.class {
        FailureClass::FeatureUnsupported { capability } => Some((
            normalize_capability_key(capability, provider_kind),
            SignalTier::SelfIdentifying,
        )),
        FailureClass::BadRequest => match_inferred(provider_kind, err),
        _ => None,
    }
}

/// Whole-phrase match of the upstream `error.message` against the
/// provider's inferred table.
fn match_inferred(provider_kind: &str, err: &Error) -> Option<(FeatureKey, SignalTier)> {
    let table = inferred_table_for(provider_kind)?;
    let message = upstream_error_message(err)?;
    let needle = message.trim();
    let matched = table
        .iter()
        .find(|entry| entry.phrase.eq_ignore_ascii_case(needle))?;
    Some((matched.capability.to_string(), SignalTier::Inferred))
}

/// The inferred phrase table for a provider `kind`, or `None` when the
/// provider has no inferred matcher in this slice.
fn inferred_table_for(provider_kind: &str) -> Option<&'static [InferredPhrase]> {
    match provider_kind {
        "anthropic-api" => Some(ANTHROPIC_INFERRED),
        _ => None,
    }
}

/// Ceiling on the upstream error body we JSON-parse for inferred matching.
/// A malicious upstream must not be able to force repeated large-JSON parses
/// on the routing path; a body over this cap skips inferred matching.
const MAX_INFERRED_BODY_BYTES: usize = 64 * 1024;

/// Extract `error.message` from an [`Error::Upstream`] body. The envelope
/// nests the message at `error.message`; any other error variant or body
/// shape (non-JSON, missing field, non-string message) yields `None`.
/// A body larger than [`MAX_INFERRED_BODY_BYTES`] is not parsed and yields
/// `None`.
fn upstream_error_message(err: &Error) -> Option<String> {
    let Error::Upstream { body, .. } = err else {
        return None;
    };
    if body.len() > MAX_INFERRED_BODY_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::match_capability;
    use routectl_core::capability::SignalTier;
    use routectl_core::error::Error;
    use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy, classify};

    /// The verbatim Anthropic Messages API 400 body for a prefill
    /// rejection.
    const PREFILL_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Prefilling assistant messages is not supported for this model."}}"#;

    fn upstream(status: u16, body: &str, ty: Option<&str>, code: Option<&str>) -> Error {
        Error::upstream_full(
            "p",
            status,
            body,
            None,
            ty.map(str::to_string),
            code.map(str::to_string),
        )
    }

    fn cf(class: FailureClass) -> ClassifiedFailure {
        ClassifiedFailure {
            class,
            matched_by: MatchedBy::Status,
        }
    }

    fn anthropic_body(message: &str) -> String {
        serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message}
        })
        .to_string()
    }

    // --- Arm 1: self-identifying ---

    #[test]
    fn openai_feature_unsupported_tokens_map_self_identifying() {
        // The classifier lifts these OpenAI error.code tokens into
        // FeatureUnsupported; the matcher must echo the capability and
        // tier it as self-identifying (one observation is enough).
        for token in [
            "unsupported_parameter",
            "unsupported_value",
            "unsupported_country_region_territory",
        ] {
            // Arrange
            let err = upstream(400, "{}", None, Some(token));
            let classified = classify(&err, Some("openai-compat"));

            // Act
            let got = match_capability("openai-compat", &err, &classified);

            // Assert
            assert_eq!(
                got,
                Some((token.to_string(), SignalTier::SelfIdentifying)),
                "token {token}"
            );
        }
    }

    #[test]
    fn feature_unsupported_capability_is_normalized_for_bedrock() {
        // Arm 1 runs the capability through normalize_capability_key; a
        // Bedrock request-bag field path reduces to the bag field.
        let class = FailureClass::FeatureUnsupported {
            capability: "additionalModelRequestFields.anthropic_beta".to_string(),
        };

        // Act
        let got = match_capability("bedrock", &upstream(400, "{}", None, None), &cf(class));

        // Assert
        assert_eq!(
            got,
            Some(("anthropic_beta".to_string(), SignalTier::SelfIdentifying))
        );
    }

    #[test]
    fn feature_unsupported_takes_precedence_and_ignores_body() {
        // A FeatureUnsupported class wins arm 1 regardless of provider or
        // body: the inferred body-parse never runs.
        let class = FailureClass::FeatureUnsupported {
            capability: "web_search".to_string(),
        };

        // Act -- body matches the inferred table, but arm 1 short-circuits.
        let got = match_capability(
            "anthropic-api",
            &upstream(400, PREFILL_BODY, None, None),
            &cf(class),
        );

        // Assert
        assert_eq!(
            got,
            Some(("web_search".to_string(), SignalTier::SelfIdentifying))
        );
    }

    // --- Arm 2: Anthropic inferred whole-phrase ---

    #[test]
    fn anthropic_prefill_phrase_maps_to_prefill_inferred() {
        // Source: Anthropic Messages API errors doc, "Prefill not
        // supported" (platform.claude.com/docs/en/api/errors). The 400 body
        // carries the phrase in free-text error.message.
        let err = upstream(400, PREFILL_BODY, Some("invalid_request_error"), None);
        let classified = classify(&err, Some("anthropic-api"));

        // Sanity: a generic invalid_request_error stays BadRequest.
        assert_eq!(classified.class, FailureClass::BadRequest);

        // Act
        let got = match_capability("anthropic-api", &err, &classified);

        // Assert
        assert_eq!(got, Some(("prefill".to_string(), SignalTier::Inferred)));
    }

    #[test]
    fn anthropic_prefill_phrase_match_is_case_insensitive() {
        // Arrange
        let body = anthropic_body("PREFILLING ASSISTANT MESSAGES IS NOT SUPPORTED FOR THIS MODEL.");

        // Act
        let got = match_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, Some(("prefill".to_string(), SignalTier::Inferred)));
    }

    #[test]
    fn anthropic_prefill_phrase_matches_ignoring_surrounding_whitespace() {
        // Arrange
        let body =
            anthropic_body("  Prefilling assistant messages is not supported for this model.  ");

        // Act
        let got = match_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, Some(("prefill".to_string(), SignalTier::Inferred)));
    }

    #[test]
    fn near_miss_anthropic_phrase_does_not_match() {
        // A truncated variant is not the verified whole phrase -- no fuzzy
        // contains, so it must not learn.
        let body = anthropic_body("Prefilling assistant messages is not supported.");

        // Act
        let got = match_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, None);
    }

    #[test]
    fn phrase_embedded_in_larger_message_does_not_match() {
        // Whole-phrase means the message IS the phrase; a phrase buried in
        // a larger message is a different, unverified shape -> no match.
        let body = anthropic_body(
            "Error: Prefilling assistant messages is not supported for this model. Please adjust.",
        );

        // Act
        let got = match_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, None);
    }

    // --- Arm 2: malformed / missing bodies ---

    #[test]
    fn garbage_body_yields_none() {
        let got = match_capability(
            "anthropic-api",
            &upstream(400, "not json at all {{{", None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn empty_body_yields_none() {
        let got = match_capability(
            "anthropic-api",
            &upstream(400, "", None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn body_without_error_message_field_yields_none() {
        for body in [
            r#"{"foo":"bar"}"#,
            r#"{"error":{"type":"invalid_request_error"}}"#,
            r#"{"error":"just a string, not an object"}"#,
        ] {
            let got = match_capability(
                "anthropic-api",
                &upstream(400, body, None, None),
                &cf(FailureClass::BadRequest),
            );
            assert_eq!(got, None, "body {body}");
        }
    }

    #[test]
    fn non_string_error_message_yields_none() {
        let got = match_capability(
            "anthropic-api",
            &upstream(400, r#"{"error":{"message":42}}"#, None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn oversized_body_yields_none() {
        // A body over the parse ceiling is never JSON-parsed, so even a body
        // that would otherwise match the inferred phrase yields None: a
        // malicious upstream cannot force a large-JSON parse on this path.
        let phrase = "Prefilling assistant messages is not supported for this model.";
        let padding = "x".repeat(64 * 1024 + 1);
        let body = anthropic_body(&format!("{padding}{phrase}"));
        assert!(body.len() > 64 * 1024, "sanity: body exceeds the cap");

        let got = match_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    // --- Provider + class gating ---

    #[test]
    fn matching_phrase_on_non_anthropic_kind_yields_none() {
        // The inferred table is keyed by provider_kind; only anthropic-api
        // has one in this slice.
        for kind in ["openai-compat", "bedrock", "gemini", "future-vendor"] {
            let got = match_capability(
                kind,
                &upstream(400, PREFILL_BODY, None, None),
                &cf(FailureClass::BadRequest),
            );
            assert_eq!(got, None, "kind {kind}");
        }
    }

    #[test]
    fn matching_phrase_but_wrong_class_yields_none() {
        // Arm 2 only fires on BadRequest; any other class never body-parses.
        for class in [
            FailureClass::RateLimited,
            FailureClass::Auth,
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::ServerError,
            FailureClass::NetworkError,
            FailureClass::Overloaded,
            FailureClass::Timeout,
            FailureClass::Unknown,
        ] {
            let got = match_capability(
                "anthropic-api",
                &upstream(400, PREFILL_BODY, None, None),
                &cf(class.clone()),
            );
            assert_eq!(got, None, "class {class:?}");
        }
    }

    #[test]
    fn non_upstream_error_with_bad_request_class_yields_none() {
        // Only Error::Upstream carries a body to parse.
        let got = match_capability(
            "anthropic-api",
            &Error::Streaming("connection reset".into()),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }
}
