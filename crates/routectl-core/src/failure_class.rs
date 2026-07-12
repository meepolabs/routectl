//! Coarse, stable classification of a canonical [`Error`] into a
//! [`FailureClass`] plus how the decision was reached ([`MatchedBy`]).
//!
//! The mapping is status-driven: the numeric upstream status decides the
//! policy row. An `upstream_type` / `upstream_code` token may only lift a
//! classification BETWEEN classes in the SAME policy row (a lift never
//! changes retry / fallback / debit behavior). Provider-family token
//! vocabularies live in the [`tables`] submodule, keyed by the provider
//! `kind` string.
//!
//! Leaf module: it reads only the structured fields already on
//! [`Error::Upstream`] and pulls in no config, router, or provider-crate
//! types.

use crate::error::Error;

/// A coarse, stable failure category derived from a canonical [`Error`].
///
/// Non-exhaustive: new categories may be added without a breaking change.
///
/// Adding a variant is not complete until every consumer has been
/// audited: the router's debit set (which classes count against a seat's
/// health), the class label emitted on observability, the fallback and
/// retry matches, and the configuration-side name adapter that maps the
/// new class to its intended default. Until a consumer is audited for
/// the new variant it treats it fail-closed -- as terminal / unknown --
/// so an unaudited addition never silently changes routing or health
/// accounting.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// Upstream rate limit (HTTP 429).
    RateLimited,
    /// Authentication or authorization rejection (HTTP 401 / 403 / 407).
    Auth,
    /// Caller-side request error the operator cannot retry into success.
    BadRequest,
    /// Request rejected by a content / safety policy.
    ContentPolicy,
    /// Prompt exceeded the model context window.
    ContextWindow,
    /// Upstream server-side failure (HTTP 5xx).
    ServerError,
    /// Deadline exceeded. Never produced by [`classify`]; reserved for a
    /// later configuration key set.
    Timeout,
    /// Transport-level failure with no HTTP status (status 0) or a
    /// streaming transport error.
    NetworkError,
    /// Upstream signalled temporary overload (HTTP 529, or 503 with an
    /// overloaded token).
    Overloaded,
    /// A requested capability is not supported by the upstream.
    FeatureUnsupported {
        /// The upstream token that identified the unsupported capability.
        capability: String,
    },
    /// No confident classification.
    Unknown,
}

/// Which signal on the [`Error`] decided the [`FailureClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    /// The [`Error`] variant alone decided it (streaming / non-upstream).
    Variant,
    /// The numeric HTTP status decided it (status 0 counts as `Status`).
    Status,
    /// A same-policy-row upstream-type / code token lift applied.
    UpstreamType,
}

/// The result of classifying an [`Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedFailure {
    /// The coarse failure category.
    pub class: FailureClass,
    /// How the category was decided.
    pub matched_by: MatchedBy,
}

/// Provider-family token vocabularies for same-policy-row lifts.
///
/// Tokens are drawn from the real error envelopes each family emits
/// (Anthropic `error.type`, OpenAI `error.type` / `error.code`, and the
/// Anthropic-on-Bedrock in-stream / converse tokens). A token appearing
/// in one of these sets can only move a classification between classes in
/// the same policy row; it never changes retry / fallback / debit
/// behavior.
mod tables {
    /// One provider family's token sets. Each field lists the tokens that
    /// trigger a same-row lift into the corresponding class.
    pub struct FamilyTokens {
        /// Tokens that lift a 4xx into [`super::FailureClass::ContentPolicy`].
        pub content_policy: &'static [&'static str],
        /// Tokens that lift a 4xx into [`super::FailureClass::ContextWindow`].
        pub context_window: &'static [&'static str],
        /// Tokens that lift a 503 into [`super::FailureClass::Overloaded`].
        pub overloaded: &'static [&'static str],
        /// Tokens that lift a 4xx into
        /// [`super::FailureClass::FeatureUnsupported`]; the matched token
        /// becomes the `capability` string.
        pub feature_unsupported: &'static [&'static str],
    }

    /// Anthropic Messages API (`error.type` vocabulary).
    pub const ANTHROPIC: FamilyTokens = FamilyTokens {
        content_policy: &[],
        context_window: &[],
        overloaded: &["overloaded_error"],
        feature_unsupported: &[],
    };

    /// OpenAI-compatible chat completions (`error.type` / `error.code`).
    pub const OPENAI: FamilyTokens = FamilyTokens {
        content_policy: &[
            "content_policy_violation",
            "content_filter",
            "invalid_prompt",
        ],
        context_window: &["context_length_exceeded", "context_window_exceeded"],
        overloaded: &[],
        feature_unsupported: &[
            "unsupported_parameter",
            "unsupported_value",
            "unsupported_country_region_territory",
        ],
    };

    /// Bedrock (Anthropic-on-Bedrock in-stream tokens + converse tokens).
    ///
    /// These lifts are reachable only via the in-stream / converse error
    /// paths; the synchronous invoke HTTP path does not yet populate the
    /// upstream type / code fields these tokens match on.
    pub const BEDROCK: FamilyTokens = FamilyTokens {
        content_policy: &["content_filtered", "guardrail_intervened", "invalid_prompt"],
        context_window: &["model_context_window_exceeded", "context_window_exceeded"],
        overloaded: &["overloaded_error"],
        feature_unsupported: &[],
    };

    /// Union of all families, used when the provider kind is absent or
    /// unrecognized. Safe because every lift stays within one policy row.
    pub const UNION: FamilyTokens = FamilyTokens {
        content_policy: &[
            "content_policy_violation",
            "content_filter",
            "invalid_prompt",
            "content_filtered",
            "guardrail_intervened",
        ],
        context_window: &[
            "context_length_exceeded",
            "context_window_exceeded",
            "model_context_window_exceeded",
        ],
        overloaded: &["overloaded_error"],
        feature_unsupported: &[
            "unsupported_parameter",
            "unsupported_value",
            "unsupported_country_region_territory",
        ],
    };

    /// Resolve the token table for a provider `kind` string. Falls back to
    /// the [`UNION`] when the kind is absent or unrecognized.
    pub fn family_for(kind: Option<&str>) -> &'static FamilyTokens {
        match kind {
            Some("anthropic-api") => &ANTHROPIC,
            Some("openai-compat") => &OPENAI,
            Some("bedrock") => &BEDROCK,
            _ => &UNION,
        }
    }
}

/// Classify a canonical [`Error`] into a [`ClassifiedFailure`].
///
/// Total: every [`Error`] variant and every status in `{0} U 400..=599`
/// yields a value without panicking. `provider_kind` is the config `kind`
/// string (e.g. `"anthropic-api"`, `"openai-compat"`, `"bedrock"`); pass
/// `None` to use the union token table. Never returns
/// [`FailureClass::Timeout`].
pub fn classify(err: &Error, provider_kind: Option<&str>) -> ClassifiedFailure {
    match err {
        Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => classify_upstream(
            *status,
            upstream_type.as_deref(),
            upstream_code.as_deref(),
            provider_kind,
        ),
        Error::Streaming(_) => by_variant(FailureClass::NetworkError),
        _ => by_variant(FailureClass::Unknown),
    }
}

fn classify_upstream(
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
    provider_kind: Option<&str>,
) -> ClassifiedFailure {
    if status == 0 {
        return by_status(FailureClass::NetworkError);
    }
    let table = tables::family_for(provider_kind);
    match status {
        401 | 403 | 407 => by_status(FailureClass::Auth),
        429 => by_status(FailureClass::RateLimited),
        400..=499 => bad_request_row(upstream_type, upstream_code, table),
        529 => by_status(FailureClass::Overloaded),
        503 if token_in(upstream_type, upstream_code, table.overloaded) => {
            by_type(FailureClass::Overloaded)
        }
        500..=599 => by_status(FailureClass::ServerError),
        _ => by_status(FailureClass::Unknown),
    }
}

/// The 4xx catch-all: [`FailureClass::BadRequest`] unless a same-row token
/// lift moves it to a sibling client-error class. A generic
/// `invalid_request_error` matches no lift set and stays `BadRequest`.
fn bad_request_row(
    ty: Option<&str>,
    code: Option<&str>,
    table: &tables::FamilyTokens,
) -> ClassifiedFailure {
    if token_in(ty, code, table.content_policy) {
        return by_type(FailureClass::ContentPolicy);
    }
    if token_in(ty, code, table.context_window) {
        return by_type(FailureClass::ContextWindow);
    }
    if let Some(capability) = matched_token(ty, code, table.feature_unsupported) {
        return by_type(FailureClass::FeatureUnsupported { capability });
    }
    by_status(FailureClass::BadRequest)
}

const fn by_variant(class: FailureClass) -> ClassifiedFailure {
    ClassifiedFailure {
        class,
        matched_by: MatchedBy::Variant,
    }
}

const fn by_status(class: FailureClass) -> ClassifiedFailure {
    ClassifiedFailure {
        class,
        matched_by: MatchedBy::Status,
    }
}

const fn by_type(class: FailureClass) -> ClassifiedFailure {
    ClassifiedFailure {
        class,
        matched_by: MatchedBy::UpstreamType,
    }
}

/// Return the first `(type, then code)` token present in `set`, if any.
fn matched_token(ty: Option<&str>, code: Option<&str>, set: &[&str]) -> Option<String> {
    if let Some(t) = ty
        && set.contains(&t)
    {
        return Some(t.to_string());
    }
    if let Some(c) = code
        && set.contains(&c)
    {
        return Some(c.to_string());
    }
    None
}

fn token_in(ty: Option<&str>, code: Option<&str>, set: &[&str]) -> bool {
    matched_token(ty, code, set).is_some()
}

#[cfg(test)]
mod tests {
    use super::{ClassifiedFailure, FailureClass, MatchedBy, classify, tables};
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
        // Arrange: one representative of every non-upstream variant. The
        // `Auth` string variant is NOT the `Auth` class -- that class only
        // comes from an upstream 401 / 403 / 407 status.
        let errs = [
            Error::NormalizeRequest("p".into(), "m".into()),
            Error::NormalizeResponse("p".into(), "m".into()),
            Error::UnknownAlias("a".into()),
            Error::Auth("bad token".into()),
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
}
