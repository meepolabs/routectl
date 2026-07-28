//! Tests for the failure classifier: the status-driven policy rows, the
//! same-row token lifts, the derived `LastOutcome` family collapse, and the
//! closed reasoning-replay matcher.

use super::{
    ClassifiedFailure, FailureClass, LastOutcome, MatchedBy, ReplayAttempt,
    class_guidance_for_status, classify, classify_with_attempt, replay, tables,
};
use crate::error::Error;

/// Build an `Error::Upstream` carrying a status and optional
/// classifier tokens.
fn upstream(status: u16, ty: Option<&str>, code: Option<&str>) -> Error {
    Error::upstream_full(
        "p",
        status,
        "body",
        None,
        ty.map(str::to_string),
        code.map(str::to_string),
    )
}

fn class_of(err: &Error, kind: Option<&str>) -> FailureClass {
    classify(err, kind).class
}

/// Assert that EVERY token in a 4xx lift set moves a 400 into
/// `expected` via an upstream-type match. Looping over the table
/// const (not one representative) makes a typo or reorder in any
/// literal fail here.
fn assert_4xx_lift(kind: Option<&str>, set: &[&str], expected: FailureClass) {
    assert!(!set.is_empty(), "expected a non-empty token set");
    for token in set {
        let err = upstream(400, Some(token), None);
        assert_eq!(
            classify(&err, kind),
            ClassifiedFailure {
                class: expected.clone(),
                matched_by: MatchedBy::UpstreamType,
            },
            "token {token} kind {kind:?}"
        );
    }
}

/// Assert that EVERY feature-unsupported token yields a capability
/// equal to the matched token. OpenAI carries these on `error.code`.
fn assert_feature_lift(kind: Option<&str>, set: &[&str]) {
    assert!(!set.is_empty(), "expected a non-empty token set");
    for token in set {
        let err = upstream(400, None, Some(token));
        assert_eq!(
            classify(&err, kind),
            ClassifiedFailure {
                class: FailureClass::FeatureUnsupported {
                    capability: (*token).to_string(),
                },
                matched_by: MatchedBy::UpstreamType,
            },
            "token {token} kind {kind:?}"
        );
    }
}

// --- Transport / status-0 ---

#[test]
fn status_zero_is_network_error_matched_by_status() {
    // Arrange
    let err = upstream(0, None, None);

    // Act
    let got = classify(&err, Some("openai-compat"));

    // Assert
    assert_eq!(
        got,
        ClassifiedFailure {
            class: FailureClass::NetworkError,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn streaming_error_is_network_error_matched_by_variant() {
    // Arrange
    let err = Error::Streaming("connection reset".into());

    // Act
    let got = classify(&err, None);

    // Assert
    assert_eq!(
        got,
        ClassifiedFailure {
            class: FailureClass::NetworkError,
            matched_by: MatchedBy::Variant,
        }
    );
}

// --- Auth row ---

#[test]
fn auth_statuses_map_to_auth_matched_by_status() {
    for status in [401, 403, 407] {
        // Arrange
        let err = upstream(status, None, None);

        // Act
        let got = classify(&err, Some("anthropic-api"));

        // Assert
        assert_eq!(
            got,
            ClassifiedFailure {
                class: FailureClass::Auth,
                matched_by: MatchedBy::Status,
            },
            "status {status}"
        );
    }
}

// --- Rate limit row ---

#[test]
fn status_429_is_rate_limited_matched_by_status() {
    // Arrange
    let err = upstream(429, None, None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("openai-compat")),
        ClassifiedFailure {
            class: FailureClass::RateLimited,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn rate_limit_tokens_confirm_but_never_lift_across_rows() {
    for token in [
        "rate_limit_exceeded",
        "insufficient_quota",
        "rate_limit_error",
    ] {
        // Arrange
        let err = upstream(429, Some(token), None);

        // Act
        let got = classify(&err, Some("openai-compat"));

        // Assert: still RateLimited, still Status (no cross-row lift).
        assert_eq!(
            got,
            ClassifiedFailure {
                class: FailureClass::RateLimited,
                matched_by: MatchedBy::Status,
            },
            "token {token}"
        );
    }
}

// --- 4xx catch-all + same-row lifts ---

#[test]
fn plain_400_is_bad_request_matched_by_status() {
    // Arrange
    let err = upstream(400, None, None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("openai-compat")),
        ClassifiedFailure {
            class: FailureClass::BadRequest,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn generic_invalid_request_error_stays_bad_request() {
    // Arrange
    let err = upstream(400, Some("invalid_request_error"), None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("anthropic-api")),
        ClassifiedFailure {
            class: FailureClass::BadRequest,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn status_408_and_499_stay_bad_request() {
    for status in [408, 499] {
        // Arrange
        let err = upstream(status, None, None);

        // Act + Assert
        assert_eq!(
            class_of(&err, None),
            FailureClass::BadRequest,
            "status {status}"
        );
    }
}

#[test]
fn openai_content_policy_tokens_lift_to_content_policy() {
    assert_4xx_lift(
        Some("openai-compat"),
        tables::OPENAI.content_policy,
        FailureClass::ContentPolicy,
    );
}

#[test]
fn openai_context_window_tokens_lift_to_context_window() {
    assert_4xx_lift(
        Some("openai-compat"),
        tables::OPENAI.context_window,
        FailureClass::ContextWindow,
    );
}

#[test]
fn openai_feature_unsupported_tokens_lift_with_capability() {
    assert_feature_lift(Some("openai-compat"), tables::OPENAI.feature_unsupported);
}

#[test]
fn bedrock_content_policy_tokens_lift_to_content_policy() {
    assert_4xx_lift(
        Some("bedrock"),
        tables::BEDROCK.content_policy,
        FailureClass::ContentPolicy,
    );
}

#[test]
fn bedrock_context_window_tokens_lift_to_context_window() {
    assert_4xx_lift(
        Some("bedrock"),
        tables::BEDROCK.context_window,
        FailureClass::ContextWindow,
    );
}

#[test]
fn lift_tokens_are_keyed_by_provider_kind() {
    // Arrange: `content_filtered` is a Bedrock-only token.
    let err = upstream(400, Some("content_filtered"), None);

    // Act: under openai-compat it is not in the table.
    let got = classify(&err, Some("openai-compat"));

    // Assert: no lift; stays BadRequest.
    assert_eq!(
        got,
        ClassifiedFailure {
            class: FailureClass::BadRequest,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn union_table_lifts_when_provider_kind_absent() {
    // Arrange
    let err = upstream(400, Some("content_policy_violation"), None);

    // Act + Assert: None provider_kind uses the union table.
    assert_eq!(class_of(&err, None), FailureClass::ContentPolicy);
}

// --- Overloaded row ---

#[test]
fn status_529_is_overloaded_matched_by_status() {
    // Arrange
    let err = upstream(529, None, None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("anthropic-api")),
        ClassifiedFailure {
            class: FailureClass::Overloaded,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn status_503_with_overloaded_token_lifts_to_overloaded() {
    // Arrange
    let err = upstream(503, Some("overloaded_error"), None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("anthropic-api")),
        ClassifiedFailure {
            class: FailureClass::Overloaded,
            matched_by: MatchedBy::UpstreamType,
        }
    );
}

#[test]
fn status_503_without_overloaded_token_is_server_error() {
    // Arrange
    let err = upstream(503, None, None);

    // Act + Assert
    assert_eq!(
        classify(&err, Some("openai-compat")),
        ClassifiedFailure {
            class: FailureClass::ServerError,
            matched_by: MatchedBy::Status,
        }
    );
}

// --- Server error row ---

#[test]
fn server_error_statuses_map_to_server_error() {
    for status in [500, 501, 502, 504, 599] {
        // Arrange
        let err = upstream(status, None, None);

        // Act + Assert
        assert_eq!(
            class_of(&err, None),
            FailureClass::ServerError,
            "status {status}"
        );
    }
}

#[test]
fn status_501_is_not_feature_unsupported() {
    // Arrange
    let err = upstream(501, None, None);

    // Act + Assert
    assert_ne!(
        classify(&err, None).class,
        FailureClass::FeatureUnsupported {
            capability: String::new(),
        }
    );
    assert_eq!(class_of(&err, None), FailureClass::ServerError);
}

// --- Non-upstream variants ---

#[test]
fn unknown_provider_is_unknown_matched_by_variant() {
    // Arrange
    let err = Error::UnknownProvider("nope".into());

    // Act + Assert
    assert_eq!(
        classify(&err, None),
        ClassifiedFailure {
            class: FailureClass::Unknown,
            matched_by: MatchedBy::Variant,
        }
    );
}

#[test]
fn non_upstream_variants_are_unknown_matched_by_variant() {
    // Arrange: one representative of every non-upstream variant that
    // has no confident classification. `Error::Streaming` (NetworkError)
    // and `Error::Auth` (Auth) are classified by variant and covered
    // by their own tests.
    let errs = [
        Error::NormalizeRequest("p".into(), "m".into()),
        Error::NormalizeResponse("p".into(), "m".into()),
        Error::UnknownAlias("a".into()),
        Error::Config("bad config".into()),
        Error::Internal("boom".into()),
        Error::Validation("bad body".into()),
        Error::NotImplemented("p".into(), "count_tokens".into()),
        Error::Io(std::io::Error::other("disk")),
        Error::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
    ];

    for err in &errs {
        // Act + Assert
        assert_eq!(
            classify(err, None),
            ClassifiedFailure {
                class: FailureClass::Unknown,
                matched_by: MatchedBy::Variant,
            },
            "variant {err:?}"
        );
    }
}

#[test]
fn auth_error_variant_is_auth_matched_by_variant() {
    // Arrange: a local credential/signing failure with no HTTP status.
    let err = Error::Auth("bad token".into());

    // Act
    let got = classify(&err, None);

    // Assert: the variant alone decides Auth, without an upstream status.
    assert_eq!(
        got,
        ClassifiedFailure {
            class: FailureClass::Auth,
            matched_by: MatchedBy::Variant,
        }
    );
}

// --- Totality + Timeout is never produced ---

#[test]
fn classify_is_total_and_never_returns_timeout() {
    let kinds = [
        None,
        Some("anthropic-api"),
        Some("openai-compat"),
        Some("bedrock"),
        Some("some-unknown-kind"),
    ];
    // Representative tokens spanning every lift set plus a generic.
    let tokens = [
        None,
        Some("invalid_request_error"),
        Some("overloaded_error"),
        Some("content_filter"),
        Some("context_length_exceeded"),
        Some("unsupported_parameter"),
        Some("rate_limit_exceeded"),
    ];

    let statuses = std::iter::once(0u16).chain(400..=599);
    for status in statuses {
        for kind in kinds {
            for token in tokens {
                // Arrange
                let err = upstream(status, token, token);

                // Act: must not panic.
                let got = classify(&err, kind);

                // Assert: Timeout is never produced.
                assert_ne!(
                    got.class,
                    FailureClass::Timeout,
                    "status {status} kind {kind:?} token {token:?}"
                );
            }
        }
    }
}

// --- Status-to-class refusal guidance ---

#[test]
fn guidance_for_plain_5xx_is_server_error_with_no_alternatives() {
    // Arrange + Act
    let got = class_guidance_for_status(500);

    // Assert
    assert_eq!(got.primary, FailureClass::ServerError);
    assert!(got.alternatives.is_empty(), "{:?}", got.alternatives);
}

#[test]
fn guidance_for_429_is_rate_limited_with_no_alternatives() {
    // Arrange + Act
    let got = class_guidance_for_status(429);

    // Assert
    assert_eq!(got.primary, FailureClass::RateLimited);
    assert!(got.alternatives.is_empty(), "{:?}", got.alternatives);
}

#[test]
fn guidance_for_503_surfaces_server_error_overloaded_ambiguity() {
    // Arrange + Act
    let got = class_guidance_for_status(503);

    // Assert: bare 503 is ServerError, but an overloaded body token
    // lifts it to Overloaded -- the ambiguity the migrator must name.
    assert_eq!(got.primary, FailureClass::ServerError);
    assert_eq!(got.alternatives, vec![FailureClass::Overloaded]);
}

#[test]
fn guidance_for_generic_4xx_is_bad_request_with_body_lift_alternatives() {
    // Arrange + Act
    let got = class_guidance_for_status(400);

    // Assert: bare 400 is BadRequest; body tokens can lift it to the
    // sibling client-error classes, in taxonomy order.
    assert_eq!(got.primary, FailureClass::BadRequest);
    assert_eq!(
        got.alternatives,
        vec![
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::FeatureUnsupported {
                capability: String::new(),
            },
        ]
    );
}

#[test]
fn guidance_for_non_4xx_5xx_status_is_unknown_with_no_alternatives() {
    // Arrange + Act: a status outside the classified range.
    let got = class_guidance_for_status(200);

    // Assert
    assert_eq!(got.primary, FailureClass::Unknown);
    assert!(got.alternatives.is_empty(), "{:?}", got.alternatives);
}

#[test]
fn guidance_is_panic_free_for_every_u16() {
    for status in 0..=u16::MAX {
        // Act: must not panic for any status.
        let got = class_guidance_for_status(status);

        // Assert: the status round-trips onto the guidance.
        assert_eq!(got.status, status);
    }
}

// --- class_token + LastOutcome vocabulary ---

/// Every current canonical variant. Constructed explicitly so a new
/// `#[non_exhaustive]` variant forces a compile-time revisit here.
fn all_variants() -> Vec<FailureClass> {
    vec![
        FailureClass::RateLimited,
        FailureClass::Auth,
        FailureClass::BadRequest,
        FailureClass::ContentPolicy,
        FailureClass::ContextWindow,
        FailureClass::ServerError,
        FailureClass::Timeout,
        FailureClass::NetworkError,
        FailureClass::Overloaded,
        FailureClass::FeatureUnsupported {
            capability: "some_upstream_token".to_string(),
        },
        FailureClass::Unknown,
    ]
}

#[test]
fn class_token_is_some_kebab_for_every_variant_except_unknown() {
    for class in all_variants() {
        // Act
        let token = class.class_token();

        // Assert
        match class {
            FailureClass::Unknown => {
                assert_eq!(token, None, "Unknown must have no token");
            }
            _ => {
                let token = token.expect("classified variant has a token");
                assert!(!token.is_empty(), "empty token for {class:?}");
                assert!(
                    token.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                    "token {token:?} for {class:?} is not kebab-case"
                );
            }
        }
    }
}

#[test]
fn class_token_emits_the_expected_kebab_tokens() {
    let cases = [
        (FailureClass::RateLimited, Some("rate-limited")),
        (FailureClass::Auth, Some("auth")),
        (FailureClass::BadRequest, Some("bad-request")),
        (FailureClass::ContentPolicy, Some("content-policy")),
        (FailureClass::ContextWindow, Some("context-window")),
        (FailureClass::ServerError, Some("server-error")),
        (FailureClass::Timeout, Some("timeout")),
        (FailureClass::NetworkError, Some("network-error")),
        (FailureClass::Overloaded, Some("overloaded")),
        (
            FailureClass::FeatureUnsupported {
                capability: "x".to_string(),
            },
            Some("feature-unsupported"),
        ),
        (FailureClass::Unknown, None),
    ];
    for (class, expected) in cases {
        assert_eq!(class.class_token(), expected, "class {class:?}");
    }
}

#[test]
fn from_failure_class_maps_each_variant_to_its_family() {
    let cases = [
        (FailureClass::RateLimited, LastOutcome::RateLimited),
        (FailureClass::Timeout, LastOutcome::Timeout),
        (FailureClass::NetworkError, LastOutcome::TransportError),
        (FailureClass::Auth, LastOutcome::Http4xx),
        (FailureClass::BadRequest, LastOutcome::Http4xx),
        (FailureClass::ContentPolicy, LastOutcome::Http4xx),
        (FailureClass::ContextWindow, LastOutcome::Http4xx),
        (
            FailureClass::FeatureUnsupported {
                capability: "x".to_string(),
            },
            LastOutcome::Http4xx,
        ),
        (FailureClass::ServerError, LastOutcome::Http5xx),
        (FailureClass::Overloaded, LastOutcome::Http5xx),
        (FailureClass::Unknown, LastOutcome::Http5xx),
    ];
    for (class, expected) in cases {
        assert_eq!(
            LastOutcome::from_failure_class(&class),
            expected,
            "class {class:?}"
        );
    }
}

#[test]
fn from_failure_class_is_total_and_never_ok_or_circuit_open() {
    // Totality: every current variant yields a value, and the two cases
    // the classifier never produces (success, gate-refusal) stay out.
    for class in all_variants() {
        let outcome = LastOutcome::from_failure_class(&class);
        assert_ne!(outcome, LastOutcome::Ok, "{class:?}");
        assert_ne!(outcome, LastOutcome::CircuitOpen, "{class:?}");
    }
}

#[test]
fn last_outcome_serializes_snake_case_with_http_family_underscores() {
    let cases = [
        (LastOutcome::Ok, "\"ok\""),
        (LastOutcome::RateLimited, "\"rate_limited\""),
        (LastOutcome::Timeout, "\"timeout\""),
        (LastOutcome::TransportError, "\"transport_error\""),
        (LastOutcome::Http4xx, "\"http_4xx\""),
        (LastOutcome::Http5xx, "\"http_5xx\""),
        (LastOutcome::CircuitOpen, "\"circuit_open\""),
    ];
    for (outcome, expected) in cases {
        // Act
        let got = serde_json::to_string(&outcome).expect("serialize");

        // Assert: round-trips through the same wire token.
        assert_eq!(got, expected, "outcome {outcome:?}");
        let back: LastOutcome = serde_json::from_str(&got).expect("deserialize");
        assert_eq!(back, outcome);
    }
}

// --- Closed replay-rejection matcher ---

/// The pinned regression fixture: a real captured replay rejection,
/// byte-exact.
///
/// It lives here as an inline constant rather than in the replay-fixture
/// corpus because that corpus is gitignored and never ships, so a
/// corpus-backed test would be unrunnable for everyone else. The body is
/// 166 bytes and carries no secret.
const REPLAY_REJECTION_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// The provider kind the matcher has a captured envelope for.
const REPLAY_KIND: &str = "openai-responses";

/// The rejection as it reaches the classifier: status, body, and the
/// structured classifiers the provider error reader lifts off it.
fn replay_rejection_error(status: u16) -> Error {
    Error::upstream_full(
        "p",
        status,
        REPLAY_REJECTION_BODY,
        None,
        Some("invalid_request_error".to_string()),
        Some("validation_error".to_string()),
    )
}

fn replay_class() -> FailureClass {
    FailureClass::FeatureUnsupported {
        capability: "reasoning_replay".to_string(),
    }
}

#[test]
fn replay_fixture_is_byte_exact_and_carries_no_secret() {
    // The fixture is a pinned capture; an accidental edit must fail here
    // rather than silently weaken every matcher test below.
    assert_eq!(REPLAY_REJECTION_BODY.len(), 166);
}

#[test]
fn replay_fixture_classifies_as_reasoning_replay_when_all_gates_hold() {
    for status in [400, 422] {
        // Arrange: the captured rejection, on the fixture-backed family,
        // for a request that carried a gray artifact.
        let err = replay_rejection_error(status);

        // Act
        let got = classify_with_attempt(
            &err,
            Some(REPLAY_KIND),
            ReplayAttempt::with_gray_artifacts(1),
        );

        // Assert
        assert_eq!(
            got,
            ClassifiedFailure {
                class: replay_class(),
                matched_by: MatchedBy::UpstreamType,
            },
            "status {status}"
        );
    }
}

#[test]
fn replay_fixture_without_a_carried_gray_artifact_stays_bad_request() {
    // Arrange: the same body and tokens, but the request carried no
    // replay artifact -- the generic-400 false positive the closed
    // matcher exists to avoid.
    let err = replay_rejection_error(400);

    // Act
    let got = classify_with_attempt(&err, Some(REPLAY_KIND), ReplayAttempt::none());

    // Assert
    assert_eq!(
        got,
        ClassifiedFailure {
            class: FailureClass::BadRequest,
            matched_by: MatchedBy::Status,
        }
    );
}

#[test]
fn a_400_after_every_artifact_was_stripped_keeps_its_ordinary_class() {
    // Arrange: the repair path already stripped the artifacts, so the
    // attempt reports none carried even though the upstream body is
    // byte-identical to the proven rejection.
    let stripped = ReplayAttempt::with_gray_artifacts(0);

    // Act
    let got = classify_with_attempt(&replay_rejection_error(400), Some(REPLAY_KIND), stripped);

    // Assert
    assert_eq!(got.class, FailureClass::BadRequest);
}

#[test]
fn classify_without_an_attempt_never_reaches_the_replay_class() {
    // Arrange + Act: the no-signal entry point every existing caller
    // uses stays byte-for-byte equivalent to its previous behavior.
    let got = classify(&replay_rejection_error(400), Some(REPLAY_KIND));

    // Assert
    assert_eq!(got.class, FailureClass::BadRequest);
}

#[test]
fn context_window_and_content_policy_400s_are_never_misclassified() {
    // Arrange: a request that DID carry gray artifacts, rejected for a
    // cause the upstream named itself.
    let carried = ReplayAttempt::with_gray_artifacts(2);
    let cases = [
        ("context_length_exceeded", FailureClass::ContextWindow),
        ("content_policy_violation", FailureClass::ContentPolicy),
    ];

    for (token, expected) in cases {
        let err = Error::upstream_full(
            "p",
            400,
            REPLAY_REJECTION_BODY,
            None,
            Some(token.to_string()),
            None,
        );

        // Act
        let got = classify_with_attempt(&err, Some(REPLAY_KIND), carried);

        // Assert: the upstream's own account outranks the inference.
        assert_eq!(got.class, expected, "token {token}");
    }
}

#[test]
fn an_adaptive_thinking_400_is_not_misclassified() {
    // Arrange: a plain 400 on the same family, same structured tokens,
    // in a request that carried artifacts -- but a different message.
    let err = Error::upstream_full(
        "p",
        400,
        r#"{"error":{"code":"validation_error","message":"adaptive thinking is not supported for this model","param":null,"type":"invalid_request_error"}}"#,
        None,
        Some("invalid_request_error".to_string()),
        Some("validation_error".to_string()),
    );

    // Act
    let got = classify_with_attempt(
        &err,
        Some(REPLAY_KIND),
        ReplayAttempt::with_gray_artifacts(1),
    );

    // Assert
    assert_eq!(got.class, FailureClass::BadRequest);
}

#[test]
fn the_matcher_is_closed_over_provider_family() {
    // Arrange: the byte-exact rejection attributed to families with no
    // captured envelope.
    let err = replay_rejection_error(400);

    for kind in [
        None,
        Some("openai-compat"),
        Some("anthropic-api"),
        Some("bedrock"),
    ] {
        // Act
        let got = classify_with_attempt(&err, kind, ReplayAttempt::with_gray_artifacts(1));

        // Assert
        assert_eq!(got.class, FailureClass::BadRequest, "kind {kind:?}");
    }
}

#[test]
fn the_matcher_is_closed_over_status() {
    // Arrange: statuses outside the rejection set never reach the
    // replay class, whatever the body says.
    for status in [401, 403, 404, 409, 429, 500] {
        let err = replay_rejection_error(status);

        // Act
        let got = classify_with_attempt(
            &err,
            Some(REPLAY_KIND),
            ReplayAttempt::with_gray_artifacts(1),
        );

        // Assert
        assert_ne!(got.class, replay_class(), "status {status}");
    }
}

#[test]
fn a_generic_validation_error_body_does_not_match_the_anchor() {
    // Arrange: the exact token pair the fixture carries, on an ordinary
    // validation failure. Adding `validation_error` to a family token
    // set would classify this one too.
    let err = Error::upstream_full(
        "p",
        400,
        r#"{"error":{"code":"validation_error","message":"max_output_tokens must be a positive integer","param":"max_output_tokens","type":"invalid_request_error"}}"#,
        None,
        Some("invalid_request_error".to_string()),
        Some("validation_error".to_string()),
    );

    // Act
    let got = classify_with_attempt(
        &err,
        Some(REPLAY_KIND),
        ReplayAttempt::with_gray_artifacts(1),
    );

    // Assert
    assert_eq!(got.class, FailureClass::BadRequest);
}

#[test]
fn an_unstructured_body_never_matches_a_message_anchor() {
    // Arrange: the anchor text present, but not inside an `error`
    // envelope -- a raw excerpt must never satisfy gate 4.
    for anchor in replay::proven_message_anchors() {
        let err = Error::upstream_full(
            "p",
            400,
            format!("gateway error: {anchor} while proxying"),
            None,
            Some("invalid_request_error".to_string()),
            Some("validation_error".to_string()),
        );

        // Act
        let got = classify_with_attempt(
            &err,
            Some(REPLAY_KIND),
            ReplayAttempt::with_gray_artifacts(1),
        );

        // Assert
        assert_eq!(got.class, FailureClass::BadRequest, "anchor {anchor}");
    }
}

#[test]
fn the_message_anchor_tolerates_case_and_whitespace_variation() {
    // Arrange: the same rejection reformatted -- a normalized signature
    // must survive re-casing and wrapped whitespace.
    let err = Error::upstream_full(
        "p",
        400,
        "{\"error\":{\"code\":\"validation_error\",\"message\":\"Encrypted Content   Missing\\n  Recognized Prefix (expected `rsn_`)\",\"param\":null,\"type\":\"invalid_request_error\"}}",
        None,
        Some("invalid_request_error".to_string()),
        Some("validation_error".to_string()),
    );

    // Act
    let got = classify_with_attempt(
        &err,
        Some(REPLAY_KIND),
        ReplayAttempt::with_gray_artifacts(1),
    );

    // Assert
    assert_eq!(got.class, replay_class());
}

#[test]
fn an_oversized_body_is_refused_unparsed() {
    // Arrange: a body past the shared request-fault ceiling, padded
    // inside a field the matcher does not read.
    let padding = "z".repeat(crate::MAX_ERROR_BODY_BYTES);
    let body = format!(
        "{{\"error\":{{\"code\":\"validation_error\",\"message\":\"encrypted content missing recognized prefix\",\"param\":\"{padding}\",\"type\":\"invalid_request_error\"}}}}"
    );
    let err = Error::upstream_full(
        "p",
        400,
        body,
        None,
        Some("invalid_request_error".to_string()),
        Some("validation_error".to_string()),
    );

    // Act
    let got = classify_with_attempt(
        &err,
        Some(REPLAY_KIND),
        ReplayAttempt::with_gray_artifacts(1),
    );

    // Assert
    assert_eq!(got.class, FailureClass::BadRequest);
}

#[test]
fn the_replay_class_reuses_the_existing_feature_unsupported_token() {
    // The matcher adds no vocabulary: the class it produces carries the
    // same kebab token every other feature-unsupported rejection does.
    assert_eq!(replay_class().class_token(), Some("feature-unsupported"));
    assert_eq!(
        LastOutcome::from_failure_class(&replay_class()),
        LastOutcome::Http4xx
    );
}

#[test]
fn status_guidance_is_unaffected_by_the_replay_matcher() {
    // Guidance is derived with no attempt signal, so the bare-400 row
    // keeps exactly its previous alternatives.
    let got = class_guidance_for_status(400);

    assert_eq!(got.primary, FailureClass::BadRequest);
    assert_eq!(
        got.alternatives,
        vec![
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::FeatureUnsupported {
                capability: String::new(),
            },
        ]
    );
}

#[test]
fn classify_with_attempt_is_total_across_statuses_and_attempts() {
    let attempts = [
        ReplayAttempt::none(),
        ReplayAttempt::with_gray_artifacts(1),
        ReplayAttempt::with_gray_artifacts(7),
    ];
    let statuses = std::iter::once(0u16).chain(400..=599);

    for status in statuses {
        for attempt in attempts {
            // Arrange
            let err = replay_rejection_error(status);

            // Act: must not panic.
            let got = classify_with_attempt(&err, Some(REPLAY_KIND), attempt);

            // Assert: Timeout is never produced.
            assert_ne!(got.class, FailureClass::Timeout, "status {status}");
        }
    }
}
