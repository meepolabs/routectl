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

use serde::{Deserialize, Serialize};

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

impl FailureClass {
    /// The kebab-case token for this class -- THE single vocabulary source
    /// for the ledger `resolved_class` column, the `/status`
    /// `errors_by_class` JSON keys, and the `[retry.classes.<token>]`
    /// config keys. Each token matches the kebab-case `serde` rename of the
    /// config-facing failure class exactly (a tripwire test pins that
    /// agreement).
    ///
    /// `Unknown` returns `None`, which downstream readers store as NULL and
    /// render as "unclassified". A new `#[non_exhaustive]` variant is a
    /// compile error here until it is given a token or routed to `None` --
    /// the audit this class's doc comment mandates.
    #[must_use]
    pub const fn class_token(&self) -> Option<&'static str> {
        match self {
            Self::RateLimited => Some("rate-limited"),
            Self::Auth => Some("auth"),
            Self::BadRequest => Some("bad-request"),
            Self::ContentPolicy => Some("content-policy"),
            Self::ContextWindow => Some("context-window"),
            Self::ServerError => Some("server-error"),
            Self::Timeout => Some("timeout"),
            Self::NetworkError => Some("network-error"),
            Self::Overloaded => Some("overloaded"),
            Self::FeatureUnsupported { .. } => Some("feature-unsupported"),
            Self::Unknown => None,
        }
    }
}

/// The coarse outcome of the most recent dispatch attempt against a target,
/// recorded on the router's per-target accounting state and surfaced on the
/// health panel.
///
/// A thin wrapper derived from [`FailureClass`] (via [`from_failure_class`])
/// plus the success and gate-refusal cases the classifier never produces --
/// deliberately co-located here so a second, drifting outcome taxonomy never
/// grows elsewhere. Serializes in snake_case.
///
/// [`from_failure_class`]: LastOutcome::from_failure_class
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastOutcome {
    /// The attempt succeeded.
    Ok,
    /// Upstream rate limit.
    RateLimited,
    /// Deadline exceeded.
    Timeout,
    /// Transport-level failure with no HTTP status.
    TransportError,
    /// A 4xx client-side rejection family. Renamed explicitly because
    /// `rename_all = "snake_case"` inserts no separator before a digit
    /// (`Http4xx` -> `http4xx`), and the wire token is `http_4xx`.
    #[serde(rename = "http_4xx")]
    Http4xx,
    /// A 5xx server-side failure family. Renamed for the same reason as
    /// [`Http4xx`]; the wire token is `http_5xx`.
    ///
    /// [`Http4xx`]: LastOutcome::Http4xx
    #[serde(rename = "http_5xx")]
    Http5xx,
    /// The circuit breaker refused the attempt. Never produced by
    /// [`from_failure_class`]; derived only from the circuit phase at
    /// DTO-build time.
    ///
    /// [`from_failure_class`]: LastOutcome::from_failure_class
    CircuitOpen,
}

impl LastOutcome {
    /// Derive the outcome of a failed attempt from its [`FailureClass`].
    ///
    /// Collapses the taxonomy into HTTP families: the client-error classes
    /// (`Auth`, `BadRequest`, `ContentPolicy`, `ContextWindow`,
    /// `FeatureUnsupported`) become [`Http4xx`]; the server-side classes
    /// (`ServerError`, `Overloaded`) and the unclassified catch-all
    /// (`Unknown`) become [`Http5xx`], the conservative server-side
    /// default. Never returns [`Ok`] or [`CircuitOpen`]. A new
    /// `#[non_exhaustive]` variant is a compile error here until it is
    /// mapped to its closest family.
    ///
    /// [`Http4xx`]: LastOutcome::Http4xx
    /// [`Http5xx`]: LastOutcome::Http5xx
    /// [`Ok`]: LastOutcome::Ok
    /// [`CircuitOpen`]: LastOutcome::CircuitOpen
    #[must_use]
    pub const fn from_failure_class(class: &FailureClass) -> Self {
        match class {
            FailureClass::RateLimited => Self::RateLimited,
            FailureClass::Timeout => Self::Timeout,
            FailureClass::NetworkError => Self::TransportError,
            FailureClass::Auth
            | FailureClass::BadRequest
            | FailureClass::ContentPolicy
            | FailureClass::ContextWindow
            | FailureClass::FeatureUnsupported { .. } => Self::Http4xx,
            FailureClass::ServerError | FailureClass::Overloaded => Self::Http5xx,
            FailureClass::Unknown => Self::Http5xx,
        }
    }
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

/// Taxonomy-derived class guidance for a bare HTTP status, for the config
/// migrator's fail-closed refusal text.
///
/// A bare status has no provider- and body-independent failure class: the
/// migrator sees only the numeric code, never the provider or response-body
/// tokens that can lift it. `primary` is the class a bare status resolves to
/// with no provider and no tokens (the union table, [`classify`] with
/// `provider_kind = None`). `alternatives` are the OTHER classes the same
/// status can resolve to once body tokens are present: 503 lifts to
/// [`FailureClass::Overloaded`]; a 4xx lifts to [`FailureClass::ContentPolicy`]
/// / [`FailureClass::ContextWindow`] / [`FailureClass::FeatureUnsupported`].
/// A [`FailureClass::FeatureUnsupported`] alternative carries an empty
/// `capability` -- the concrete capability is a per-token body detail the
/// migrator never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusClassGuidance {
    /// The status the guidance describes.
    pub status: u16,
    /// The class a bare status resolves to with no provider / body tokens.
    pub primary: FailureClass,
    /// Other classes the same status can resolve to on body tokens, in
    /// taxonomy order. Empty when the status is unambiguous.
    pub alternatives: Vec<FailureClass>,
}

/// Derive [`StatusClassGuidance`] for any `status`, reusing the real
/// [`classify_upstream`] path -- never a hand-duplicated status->class table,
/// so a taxonomy change cannot drift the guidance.
///
/// Panic-free for any `u16`: a status outside `{0} U 400..=599` yields the
/// classifier's fallback class with no alternatives.
pub fn class_guidance_for_status(status: u16) -> StatusClassGuidance {
    let primary = classify_upstream(status, None, None, None).class;
    let mut alternatives = Vec::new();
    for token in union_lift_tokens() {
        let lifted = match classify_upstream(status, Some(token), Some(token), None).class {
            FailureClass::FeatureUnsupported { .. } => FailureClass::FeatureUnsupported {
                capability: String::new(),
            },
            other => other,
        };
        if lifted != primary && !alternatives.contains(&lifted) {
            alternatives.push(lifted);
        }
    }
    StatusClassGuidance {
        status,
        primary,
        alternatives,
    }
}

/// Every token that can trigger a same-row lift in the provider-agnostic
/// union table, so the reachable alternatives are read from the real
/// taxonomy rather than restated.
fn union_lift_tokens() -> impl Iterator<Item = &'static str> {
    let t = &tables::UNION;
    t.content_policy
        .iter()
        .chain(t.context_window)
        .chain(t.overloaded)
        .chain(t.feature_unsupported)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::{
        ClassifiedFailure, FailureClass, LastOutcome, MatchedBy, class_guidance_for_status,
        classify, tables,
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
}
