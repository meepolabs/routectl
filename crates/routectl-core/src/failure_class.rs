//! Coarse, stable classification of a canonical [`Error`] into a
//! [`FailureClass`] plus how the decision was reached ([`MatchedBy`]).
//!
//! The mapping is status-driven: the numeric upstream status decides the
//! policy row. An `upstream_type` / `upstream_code` token may only lift a
//! classification BETWEEN classes in the SAME policy row (a lift never
//! changes retry / fallback / debit behavior). Provider-family token
//! vocabularies live in the `tables` submodule, keyed by the provider
//! `kind` string.
//!
//! Leaf module: it reads only the fields already on [`Error::Upstream`] and
//! pulls in no config, router, or provider-crate types. The one path that
//! looks past the structured fields into the response `body` is the closed
//! replay-rejection matcher in the `replay` submodule, and only after its
//! cheaper gates have already held.

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

/// Closed, fixture-backed recognition of a PROVEN reasoning-replay
/// rejection, plus the caller-supplied signal it gates on.
///
/// A replay rejection reaches [`FailureClass::FeatureUnsupported`] under
/// the `reasoning_replay` capability rather than a class of its own: the
/// existing key already flows into the capability rails where the learned
/// negative belongs, and it reaches the same consumers a new variant would
/// -- at the cost of the full consumer audit [`FailureClass`] mandates and
/// one more forever ledger token.
///
/// Recognition is deliberately narrow. A misclassification silently
/// disables working reasoning continuity until the learned negative
/// decays, so ALL FOUR gates must hold conjunctively and each one is
/// closed rather than heuristic. Widening any of them -- in particular
/// adding the generic `validation_error` token to a family's
/// `feature_unsupported` set -- would over-match every validation error on
/// that provider family.
mod replay {
    use crate::MAX_ERROR_BODY_BYTES;

    /// The capability key a proven replay rejection resolves to. Mirrors
    /// the capability-key vocabulary: the learned layer looks the negative
    /// back up under this exact string.
    pub const CAPABILITY: &str = "reasoning_replay";

    /// GATE 1: the statuses a replay validator rejects with.
    const REJECTION_STATUSES: &[u16] = &[400, 422];

    /// GATE 3: provider `kind` strings for which a replay rejection
    /// envelope has actually been captured. Closed on purpose -- a family
    /// with no captured envelope can never match, whatever its body says.
    const FIXTURE_BACKED_KINDS: &[&str] = &["openai-responses"];

    /// One proven rejection shape.
    ///
    /// `upstream_type` / `upstream_code` are matched against the canonical
    /// structured classifiers when the error carries them, falling back to
    /// the same tokens read out of the body's own error envelope -- see
    /// [`Envelope`] for why the canonical fields alone are not enough.
    /// `message_anchor` is matched against the normalized `error.message`.
    /// A `None` field is "don't care", so a row carrying only structured
    /// tokens matches on those alone -- but no such row exists today,
    /// because every token the captured envelope carries is generic on its
    /// own.
    struct ReplaySignature {
        /// Required `error.type`, or `None` to ignore it.
        upstream_type: Option<&'static str>,
        /// Required `error.code`, or `None` to ignore it.
        upstream_code: Option<&'static str>,
        /// Required PREFIX of the normalized `error.message`, or `None` to
        /// decide on the structured tokens alone. A prefix rather than a
        /// floating substring so the match stays anchored, and rather than
        /// full equality so a reworded tail (the captured envelope ends in
        /// a parenthesized list of accepted prefixes) does not silently
        /// stop matching.
        message_anchor: Option<&'static str>,
    }

    /// GATE 4: every proven replay-rejection shape. Grounded in captured
    /// envelopes only -- an unverified shape waits for its capture.
    const PROVEN_REJECTIONS: &[ReplaySignature] = &[ReplaySignature {
        upstream_type: Some("invalid_request_error"),
        upstream_code: Some("validation_error"),
        message_anchor: Some("encrypted content missing recognized prefix"),
    }];

    /// What the attempted request carried, supplied by the DISPATCHER.
    ///
    /// GATE 2 lives here. The signal is passed in rather than sniffed out
    /// of the rejection body because only the caller knows what it actually
    /// put on the wire: a rejection arriving after every replay artifact
    /// was already stripped is an ordinary failure, and no property of the
    /// body distinguishes it from one that carried artifacts.
    ///
    /// [`ReplayAttempt::none`] is the default and reproduces exactly the
    /// pre-existing classification for every caller that does not replay
    /// reasoning artifacts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ReplayAttempt {
        gray_artifacts: usize,
    }

    impl ReplayAttempt {
        /// No reasoning artifact of unestablished scheme was carried --
        /// either none was sent at all, or every one was stripped before
        /// dispatch.
        #[must_use]
        pub const fn none() -> Self {
            Self { gray_artifacts: 0 }
        }

        /// The attempt carried `count` reasoning artifacts whose replay
        /// onto the target lane was not established either way.
        #[must_use]
        pub const fn with_gray_artifacts(count: usize) -> Self {
            Self {
                gray_artifacts: count,
            }
        }

        /// Whether the attempt carried at least one such artifact.
        #[must_use]
        pub const fn carried_gray_artifact(self) -> bool {
            self.gray_artifacts > 0
        }
    }

    /// Whether all four gates hold for this rejection.
    ///
    /// Pure in its inputs: no clock, no registry, no provider handle, so it
    /// is exercisable without a live upstream. Ordered cheapest-first, so
    /// the body is parsed only for a rejection that already passed the
    /// status, artifact, and family gates.
    pub fn is_replay_rejection(
        status: u16,
        upstream_type: Option<&str>,
        upstream_code: Option<&str>,
        body: &str,
        provider_kind: Option<&str>,
        attempt: ReplayAttempt,
    ) -> bool {
        if !REJECTION_STATUSES.contains(&status) || !attempt.carried_gray_artifact() {
            return false;
        }
        if !matches!(provider_kind, Some(kind) if FIXTURE_BACKED_KINDS.contains(&kind)) {
            return false;
        }
        let envelope = Envelope::read(body);
        PROVEN_REJECTIONS
            .iter()
            .any(|sig| signature_matches(sig, upstream_type, upstream_code, &envelope))
    }

    /// The `type`, `code`, and normalized `message` of a JSON error
    /// envelope.
    ///
    /// The envelope is read because the canonical error does NOT reliably
    /// carry these tokens: a provider that recognizes a body as its own
    /// first-party `{"error":{...}}` envelope carries the body RAW and
    /// leaves `upstream_type` / `upstream_code` empty, since the
    /// structured-classifier fields exist for the shapes whose tokens live
    /// at the top level instead. Matching only the canonical fields would
    /// therefore never fire on exactly the family this matcher is for.
    ///
    /// `param` is deliberately not read: it is `null` in the captured
    /// envelope, so it buys no discrimination.
    #[derive(Default)]
    struct Envelope {
        error_type: Option<String>,
        code: Option<String>,
        message: Option<String>,
    }

    impl Envelope {
        /// Read the envelope, or an empty one when the body is not a JSON
        /// `{"error":{...}}` shape.
        ///
        /// Strict by design: a non-envelope body yields no fields rather
        /// than falling back to a raw excerpt, so an anchor can never match
        /// text that merely appeared somewhere in an unstructured body. An
        /// oversized body is refused unparsed, bounding the JSON a hostile
        /// upstream can push onto the routing path -- the same ceiling the
        /// request-fault producers write against.
        fn read(body: &str) -> Self {
            if body.len() > MAX_ERROR_BODY_BYTES {
                return Self::default();
            }
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) else {
                return Self::default();
            };
            let Some(error) = parsed.get("error") else {
                return Self::default();
            };
            Self {
                error_type: string_at(error, "type"),
                code: string_at(error, "code"),
                message: string_at(error, "message").map(|m| normalize(&m)),
            }
        }
    }

    fn string_at(value: &serde_json::Value, key: &str) -> Option<String> {
        value.get(key)?.as_str().map(str::to_string)
    }

    /// Lowercase with whitespace runs collapsed, so a re-cased or rewrapped
    /// rendering of the same rejection still matches its anchor.
    fn normalize(text: &str) -> String {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn signature_matches(
        sig: &ReplaySignature,
        upstream_type: Option<&str>,
        upstream_code: Option<&str>,
        envelope: &Envelope,
    ) -> bool {
        // The canonical structured field wins when present; the envelope's
        // own token is the fallback for the families that leave it empty.
        let effective_type = upstream_type.or(envelope.error_type.as_deref());
        let effective_code = upstream_code.or(envelope.code.as_deref());
        if sig.upstream_type.is_some() && sig.upstream_type != effective_type {
            return false;
        }
        if sig.upstream_code.is_some() && sig.upstream_code != effective_code {
            return false;
        }
        match sig.message_anchor {
            None => true,
            Some(anchor) => envelope
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with(anchor)),
        }
    }

    #[cfg(test)]
    pub(super) fn proven_message_anchors() -> impl Iterator<Item = &'static str> {
        PROVEN_REJECTIONS
            .iter()
            .filter_map(|sig| sig.message_anchor)
    }
}

pub use replay::ReplayAttempt;

/// Classify a canonical [`Error`] into a [`ClassifiedFailure`].
///
/// Total: every [`Error`] variant and every status in `{0} U 400..=599`
/// yields a value without panicking. `provider_kind` is the config `kind`
/// string (e.g. `"anthropic-api"`, `"openai-compat"`, `"bedrock"`); pass
/// `None` to use the union token table. Never returns
/// [`FailureClass::Timeout`].
///
/// Equivalent to [`classify_with_attempt`] with [`ReplayAttempt::none`]:
/// a caller that did not replay reasoning artifacts cannot have hit a
/// replay rejection, so the closed matcher is inert for it.
pub fn classify(err: &Error, provider_kind: Option<&str>) -> ClassifiedFailure {
    classify_with_attempt(err, provider_kind, ReplayAttempt::none())
}

/// Classify a canonical [`Error`], additionally recognizing a proven
/// reasoning-replay rejection when the dispatched request carried
/// reasoning artifacts whose replay onto the target lane was not
/// established.
///
/// `attempt` is the dispatcher's own record of what it put on the wire;
/// see [`ReplayAttempt`] for why the signal cannot be recovered from the
/// rejection itself. Every other classification is identical to
/// [`classify`].
pub fn classify_with_attempt(
    err: &Error,
    provider_kind: Option<&str>,
    attempt: ReplayAttempt,
) -> ClassifiedFailure {
    match err {
        Error::Upstream {
            status,
            body,
            upstream_type,
            upstream_code,
            ..
        } => classify_upstream_attempt(
            *status,
            upstream_type.as_deref(),
            upstream_code.as_deref(),
            body,
            provider_kind,
            attempt,
        ),
        Error::Streaming(_) => by_variant(FailureClass::NetworkError),
        Error::Auth(_) => by_variant(FailureClass::Auth),
        _ => by_variant(FailureClass::Unknown),
    }
}

fn classify_upstream(
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
    provider_kind: Option<&str>,
) -> ClassifiedFailure {
    classify_upstream_attempt(
        status,
        upstream_type,
        upstream_code,
        "",
        provider_kind,
        ReplayAttempt::none(),
    )
}

fn classify_upstream_attempt(
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
    body: &str,
    provider_kind: Option<&str>,
    attempt: ReplayAttempt,
) -> ClassifiedFailure {
    if status == 0 {
        return by_status(FailureClass::NetworkError);
    }
    let table = tables::family_for(provider_kind);
    match status {
        401 | 403 | 407 => by_status(FailureClass::Auth),
        429 => by_status(FailureClass::RateLimited),
        400..=499 => {
            let row = bad_request_row(upstream_type, upstream_code, table);
            if row.class != FailureClass::BadRequest {
                return row;
            }
            replay_rejection(
                status,
                upstream_type,
                upstream_code,
                body,
                provider_kind,
                attempt,
            )
            .unwrap_or(row)
        }
        529 => by_status(FailureClass::Overloaded),
        503 if token_in(upstream_type, upstream_code, table.overloaded) => {
            by_type(FailureClass::Overloaded)
        }
        500..=599 => by_status(FailureClass::ServerError),
        _ => by_status(FailureClass::Unknown),
    }
}

/// The closed replay-rejection lift, or `None` when any gate fails.
///
/// Only a rejection that would otherwise be a plain
/// [`FailureClass::BadRequest`] is eligible: a token lift that already
/// named a content-policy, context-window, or feature-unsupported cause has
/// the upstream's own account of the failure, which outranks this
/// inference. That ordering is what keeps an adaptive-thinking or
/// context-window 400 out of the replay class even in a request that did
/// carry artifacts.
fn replay_rejection(
    status: u16,
    upstream_type: Option<&str>,
    upstream_code: Option<&str>,
    body: &str,
    provider_kind: Option<&str>,
    attempt: ReplayAttempt,
) -> Option<ClassifiedFailure> {
    replay::is_replay_rejection(
        status,
        upstream_type,
        upstream_code,
        body,
        provider_kind,
        attempt,
    )
    .then(|| {
        by_type(FailureClass::FeatureUnsupported {
            capability: replay::CAPABILITY.to_string(),
        })
    })
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
/// `classify_upstream` path -- never a hand-duplicated status->class table,
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
#[path = "failure_class_tests.rs"]
mod tests;
