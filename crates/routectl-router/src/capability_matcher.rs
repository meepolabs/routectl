//! Resolves a use-time upstream rejection to the CANONICAL capability it
//! names -- the single closed-set resolver shared by learn capture,
//! act-side route-away/strip lookup (via the same canonical key namespace),
//! and probe same-capability settlement. Its output keys on the request's
//! capability namespace (the `derive_feature_keys` vocabulary), so a learned
//! negative and a later dispatch-time lookup meet on identical strings.
//!
//! Data-driven, keyed by provider `kind` (the `class_policy` pattern), and
//! deliberately outside `routectl-core` -- the failure classifier stays
//! body-parse free, and this is not a `Provider` trait method. Two arms:
//!
//! - SELF-IDENTIFYING: the classifier already lifted the rejection to
//!   [`FailureClass::FeatureUnsupported`]. For openai-compat, the class
//!   carries an `error.code` TOKEN (`unsupported_parameter`, ...), which is
//!   NOT a capability key; the field that actually names the offending
//!   capability is `error.param` -- a WIRE param name (`response_format`,
//!   ...), which is not itself the request-capability key either. So this
//!   arm maps `/error/param` onto the canonical request-capability namespace
//!   through a CLOSED set: an explicit translation table
//!   ([`OPENAI_PARAM_TRANSLATIONS`], e.g. `response_format` ->
//!   `structured_output`) for wire params that name a capability under a
//!   different key than the request side, plus a passthrough for a param
//!   that -- after `strip_date_suffix` + `normalize_capability_key` -- is
//!   ALREADY a well-known capability key (a typed built-in tool an upstream
//!   rejects by `type`). A param outside that closed set, or an absent
//!   `param`, yields `None` -- the loop never learns a capability the request
//!   side cannot look up under the same key -- EXCEPT the small closed set of
//!   paramless rejections that still name a correct target-level route-away
//!   (a geo/region block), for which the code token itself is the canonical
//!   key. For other providers (Bedrock, as its token table grows) the class
//!   carries a field path and is normalized directly. One observation is
//!   trustworthy.
//! - INFERRED: a generic [`FailureClass::BadRequest`] whose free-text
//!   `error.message` names a capability only in prose. Matched by
//!   whole-phrase equality (case-insensitive) against a small per-provider
//!   table of phrases grounded in real captured / documented 400 envelopes.
//!   Precision over recall: an unverified phrase is omitted, and a
//!   near-miss or embedded phrase never matches.
//!
//! Every other class, provider, or malformed body yields `None` -- the
//! resolver never manufactures a false positive.

use routectl_core::capability::{
    STRUCTURED_OUTPUT, SignalTier, WELL_KNOWN_CAPABILITY_KEYS, normalize_capability_key,
};
use routectl_core::error::Error;
use routectl_core::failure_class::{ClassifiedFailure, FailureClass};

use crate::feature_keys::strip_date_suffix;

/// The openai-compat provider `kind` string. For this family the
/// `FeatureUnsupported` class carries an `error.code` token rather than a
/// capability, so the resolver reads `/error/param` instead.
const OPENAI_COMPAT_KIND: &str = "openai-compat";

/// Closed-set translation of an openai-compat `error.param` WIRE param name
/// onto the canonical request-capability key the request side derives and
/// the act side looks up. openai names the offending field by its wire key
/// (`response_format` for constrained decoding), which the request side keys
/// under a DIFFERENT canonical name (`structured_output`); without this
/// mapping a learned negative keyed on the raw wire param would never meet
/// the request-derived key, so the loop would never route away.
///
/// EXTENSION POINT: add a `(wire_param, canonical_key)` row here when a new
/// provider body surface must map onto an existing canonical capability. A
/// param whose canonicalized form is ALREADY a well-known capability key
/// (a typed built-in tool an upstream rejects by `type`) passes through
/// without a row; every other param yields `None` (no-learn), keeping the
/// set closed.
const OPENAI_PARAM_TRANSLATIONS: &[(&str, &str)] = &[("response_format", STRUCTURED_OUTPUT)];

/// openai-compat `error.code` tokens whose rejection carries no
/// `/error/param` yet still names a correct route-away: a geo/region block
/// applies to the whole account, not a single request field. For these the
/// code token itself is the canonical key. Every other paramless openai
/// rejection yields `None` (no-learn).
///
/// DORMANT at capture: this token is a target-level geo signal, not a
/// request-derived feature, so `derive_feature_keys` never produces it and
/// the request-membership gate in `observe_for_learning` blocks the learn.
/// A target-level route-away for geo-blocks needs its own mechanism (out of
/// scope here); the resolver arm stays so that mechanism can reuse it.
const OPENAI_PARAMLESS_ROUTE_AWAY: &[&str] = &["unsupported_country_region_territory"];

/// A learned capability key. Feature keys are open-namespace strings
/// shared with the catalog prior and the alias-chain pre-filter.
type FeatureKey = String;

/// Capability key for assistant-message prefill. Open-namespace key (not
/// one of the well-known `routectl_core::capability` consts): the registry
/// namespace is open, so a capability the upstream names by prose is a
/// first-class key.
///
/// DORMANT until an act-side derivation for this capability exists. The
/// resolver emits `prefill` for a matched phrase, but `derive_feature_keys`
/// has no rule that produces it, so the capture-side request-membership gate
/// (`request_features.contains(key)` in `observe_for_learning`) blocks the
/// learn: learn-side output is not yet a subset of the act-side vocabulary.
/// The table and matcher stay wired so the day an act-side derivation lands,
/// the loop closes with no matcher change.
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

/// Resolve a classified rejection to the CANONICAL capability it names and
/// the signal tier of that evidence, or `None` when the rejection names no
/// capability this resolver can attribute. The single shared resolver: its
/// output keys on the request-capability namespace so learn capture, the
/// act-side lookup, and probe settlement all meet on identical strings.
pub(crate) fn resolve_requested_capability(
    provider_kind: &str,
    err: &Error,
    cf: &ClassifiedFailure,
) -> Option<(FeatureKey, SignalTier)> {
    match &cf.class {
        FailureClass::FeatureUnsupported { capability } => {
            resolve_self_identifying(provider_kind, err, capability)
        }
        FailureClass::BadRequest => match_inferred(provider_kind, err),
        _ => None,
    }
}

/// The self-identifying arm. For openai-compat the class token is an
/// `error.code`, not a capability, so the real capability is read from
/// `/error/param`; other providers carry a field path in the token and are
/// normalized directly.
fn resolve_self_identifying(
    provider_kind: &str,
    err: &Error,
    upstream_token: &str,
) -> Option<(FeatureKey, SignalTier)> {
    if provider_kind == OPENAI_COMPAT_KIND {
        return resolve_openai_param(err, upstream_token);
    }
    Some((
        normalize_capability_key(upstream_token, provider_kind),
        SignalTier::SelfIdentifying,
    ))
}

/// openai-compat: map `/error/param` onto the canonical request-capability
/// namespace through the closed set (an explicit translation row, then a
/// well-known tool-type passthrough), so a learned key lands where the
/// request side derives it. A missing param yields `None` unless the code
/// token is a paramless route-away.
fn resolve_openai_param(err: &Error, upstream_token: &str) -> Option<(FeatureKey, SignalTier)> {
    if let Some(param) = upstream_error_field(err, "param") {
        return resolve_openai_param_surface(param.trim());
    }
    if OPENAI_PARAMLESS_ROUTE_AWAY.contains(&upstream_token) {
        return Some((upstream_token.to_string(), SignalTier::SelfIdentifying));
    }
    None
}

/// Resolve a raw openai `error.param` surface against the closed set: an
/// explicit translation row first, then a well-known tool-type passthrough
/// (canonicalized via `strip_date_suffix` + `normalize_capability_key`),
/// else `None`. The set is closed so an arbitrary rejected wire param never
/// learns a key the act side cannot look up.
fn resolve_openai_param_surface(param: &str) -> Option<(FeatureKey, SignalTier)> {
    // Share one date-stripped input across both closed-set paths so a dated
    // variant of a translated surface (`response_format_20250305`) still
    // resolves rather than silently falling through to no-learn.
    let base = strip_date_suffix(param);
    if let Some((_, canonical)) = OPENAI_PARAM_TRANSLATIONS
        .iter()
        .find(|(surface, _)| *surface == base)
    {
        return Some(((*canonical).to_string(), SignalTier::SelfIdentifying));
    }
    let canonical = normalize_capability_key(base, OPENAI_COMPAT_KIND);
    if WELL_KNOWN_CAPABILITY_KEYS.contains(&canonical.as_str()) {
        return Some((canonical, SignalTier::SelfIdentifying));
    }
    None
}

/// Whole-phrase match of the upstream `error.message` against the
/// provider's inferred table.
fn match_inferred(provider_kind: &str, err: &Error) -> Option<(FeatureKey, SignalTier)> {
    let table = inferred_table_for(provider_kind)?;
    let message = upstream_error_field(err, "message")?;
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

/// Ceiling on the upstream error body we JSON-parse to extract a field.
/// Guards BOTH the self-identifying `error.param` read and the inferred
/// `error.message` read: a malicious upstream must not be able to force
/// repeated large-JSON parses on the routing path; a body over this cap is
/// not parsed and yields `None`.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Extract `error.<field>` (a string) from an [`Error::Upstream`] body.
/// Shared by the self-identifying `param` read and the inferred `message`
/// read. Any non-upstream error variant, an over-cap body, a non-JSON body,
/// a missing field, or a non-string value yields `None`.
fn upstream_error_field(err: &Error, field: &str) -> Option<String> {
    let Error::Upstream { body, .. } = err else {
        return None;
    };
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("error")?.get(field)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::resolve_requested_capability;
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

    /// An openai-compat 400 whose `error.code` lifts to FeatureUnsupported
    /// and whose `error.param` names the offending capability.
    fn openai_unsupported_body(code: &str, param: &str) -> String {
        serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "code": code,
                "param": param,
                "message": "Unsupported parameter."
            }
        })
        .to_string()
    }

    #[test]
    fn openai_resolves_tool_type_param_via_passthrough() {
        // The classifier lifts the `error.code` token into FeatureUnsupported,
        // but the code token is NOT a capability -- the resolver must return
        // the canonicalized `/error/param` when it names a well-known
        // tool-type capability (a typed built-in an upstream rejects by type),
        // date suffix stripped.
        for (code, param, canonical) in [
            ("unsupported_parameter", "web_search_20250305", "web_search"),
            ("unsupported_value", "computer_use", "computer_use"),
        ] {
            // Arrange
            let body = openai_unsupported_body(code, param);
            let err = upstream(400, &body, Some("invalid_request_error"), Some(code));
            let classified = classify(&err, Some("openai-compat"));

            // Act
            let got = resolve_requested_capability("openai-compat", &err, &classified);

            // Assert
            assert_eq!(
                got,
                Some((canonical.to_string(), SignalTier::SelfIdentifying)),
                "code {code} param {param}"
            );
        }
    }

    #[test]
    fn openai_translates_response_format_to_structured_output() {
        // openai names constrained decoding by its wire param
        // (`response_format`); the request side keys the same capability as
        // `structured_output`. The closed-set translation table maps the two
        // so a learned negative meets the request-derived key.
        let body = openai_unsupported_body("unsupported_parameter", "response_format");
        let err = upstream(
            400,
            &body,
            Some("invalid_request_error"),
            Some("unsupported_parameter"),
        );
        let classified = classify(&err, Some("openai-compat"));

        let got = resolve_requested_capability("openai-compat", &err, &classified);

        assert_eq!(
            got,
            Some(("structured_output".to_string(), SignalTier::SelfIdentifying))
        );
    }

    #[test]
    fn openai_translates_dated_variant_of_a_translated_param() {
        // A dated variant of a translated wire surface is date-stripped
        // BEFORE the translation lookup, so it still resolves rather than
        // falling through the closed set to no-learn.
        let body = openai_unsupported_body("unsupported_parameter", "response_format_20250305");
        let err = upstream(
            400,
            &body,
            Some("invalid_request_error"),
            Some("unsupported_parameter"),
        );
        let classified = classify(&err, Some("openai-compat"));

        let got = resolve_requested_capability("openai-compat", &err, &classified);

        assert_eq!(
            got,
            Some(("structured_output".to_string(), SignalTier::SelfIdentifying))
        );
    }

    #[test]
    fn openai_param_outside_the_closed_set_does_not_learn() {
        // A param that is neither a translation-table wire surface nor a
        // well-known tool-type key names a capability the request side cannot
        // look up under the same key -- the closed set yields None (no-learn),
        // so an arbitrary rejected body param never poisons routing.
        for param in ["reasoning", "temperature", "max_tokens", "seed", "stop"] {
            let body = openai_unsupported_body("unsupported_parameter", param);
            let err = upstream(
                400,
                &body,
                Some("invalid_request_error"),
                Some("unsupported_parameter"),
            );
            let classified = classify(&err, Some("openai-compat"));
            let got = resolve_requested_capability("openai-compat", &err, &classified);
            assert_eq!(got, None, "param {param}");
        }
    }

    #[test]
    fn openai_paramless_rejection_does_not_learn() {
        // A `unsupported_parameter` / `unsupported_value` rejection with no
        // `/error/param` names no capability -- the resolver must NOT fall
        // back to the code token (it is not a capability key).
        for code in ["unsupported_parameter", "unsupported_value"] {
            let err = upstream(400, "{}", Some("invalid_request_error"), Some(code));
            let classified = classify(&err, Some("openai-compat"));
            let got = resolve_requested_capability("openai-compat", &err, &classified);
            assert_eq!(got, None, "code {code}");
        }
    }

    #[test]
    fn openai_country_region_rejection_falls_back_to_the_code_token() {
        // A geo/region block carries no `/error/param` yet still names a
        // correct target-level route-away: the closed-set fallback keys on
        // the code token itself.
        let code = "unsupported_country_region_territory";
        let err = upstream(400, "{}", Some("invalid_request_error"), Some(code));
        let classified = classify(&err, Some("openai-compat"));
        let got = resolve_requested_capability("openai-compat", &err, &classified);
        assert_eq!(got, Some((code.to_string(), SignalTier::SelfIdentifying)));
    }

    #[test]
    fn feature_unsupported_capability_is_normalized_for_bedrock() {
        // Non-openai providers carry a field path in the class token; the
        // resolver normalizes it directly (a Bedrock request-bag field path
        // reduces to the bag field). No `/error/param` read for this family.
        let class = FailureClass::FeatureUnsupported {
            capability: "additionalModelRequestFields.anthropic_beta".to_string(),
        };

        // Act
        let got =
            resolve_requested_capability("bedrock", &upstream(400, "{}", None, None), &cf(class));

        // Assert
        assert_eq!(
            got,
            Some(("anthropic_beta".to_string(), SignalTier::SelfIdentifying))
        );
    }

    #[test]
    fn feature_unsupported_takes_precedence_and_ignores_body() {
        // A FeatureUnsupported class on a non-openai provider wins arm 1
        // regardless of body: the inferred body-parse never runs.
        let class = FailureClass::FeatureUnsupported {
            capability: "web_search".to_string(),
        };

        // Act -- body matches the inferred table, but arm 1 short-circuits.
        let got = resolve_requested_capability(
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
        let got = resolve_requested_capability("anthropic-api", &err, &classified);

        // Assert
        assert_eq!(got, Some(("prefill".to_string(), SignalTier::Inferred)));
    }

    #[test]
    fn anthropic_prefill_phrase_match_is_case_insensitive() {
        // Arrange
        let body = anthropic_body("PREFILLING ASSISTANT MESSAGES IS NOT SUPPORTED FOR THIS MODEL.");

        // Act
        let got = resolve_requested_capability(
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
        let got = resolve_requested_capability(
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
        let got = resolve_requested_capability(
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
        let got = resolve_requested_capability(
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
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, "not json at all {{{", None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn empty_body_yields_none() {
        let got = resolve_requested_capability(
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
            let got = resolve_requested_capability(
                "anthropic-api",
                &upstream(400, body, None, None),
                &cf(FailureClass::BadRequest),
            );
            assert_eq!(got, None, "body {body}");
        }
    }

    #[test]
    fn non_string_error_message_yields_none() {
        let got = resolve_requested_capability(
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

        let got = resolve_requested_capability(
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
            let got = resolve_requested_capability(
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
            let got = resolve_requested_capability(
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
        let got = resolve_requested_capability(
            "anthropic-api",
            &Error::Streaming("connection reset".into()),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }
}
