//! Pure classification/observability helpers shared across the dispatch
//! surfaces. Stable, low-cardinality labels for failure classes and the
//! surface an error arm belongs to, plus the safe, structured facts pulled
//! from an [`Error`] for the router's class-decision events. Deliberately
//! excludes any body, prompt, header, token, or free-form upstream message
//! text so nothing sensitive can leak into an observability field.

use std::collections::BTreeMap;

use routectl_core::Error;
use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy};

/// Which dispatch surface an error arm belongs to. Carried as a stable
/// `surface` field on the router's class-decision observability events so
/// operators can tell a completion failure from a stream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DispatchSurface {
    Complete,
    Stream,
    /// The count_tokens walk. Carries no canary cadence today -- see the
    /// pre-flight planner's own docs for why only `Complete` will ever
    /// decrement or claim a canary.
    CountTokens,
}

impl DispatchSurface {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Stream => "stream",
            Self::CountTokens => "count_tokens",
        }
    }

    /// Whether a strict-translation refusal leaves the target unsent and
    /// walks on, or ends the request. A message surface walks because the
    /// next target may carry the capability; a count ends, since it projects
    /// the same request onto every seat.
    pub(super) const fn strict_reject_walks(self) -> bool {
        match self {
            Self::Complete | Self::Stream => true,
            Self::CountTokens => false,
        }
    }

    /// Whether a chain entry naming an unregistered provider is skipped or
    /// ends the request.
    pub(super) const fn unknown_provider_walks(self) -> bool {
        match self {
            Self::Complete | Self::Stream => true,
            Self::CountTokens => false,
        }
    }

    /// Whether a fallbackable upstream health error (429, 5xx, status 0)
    /// walks to the next target or ends the request. Health fallback is
    /// reserved for the message surfaces.
    pub(super) const fn health_error_walks(self) -> bool {
        match self {
            Self::Complete | Self::Stream => true,
            Self::CountTokens => false,
        }
    }

    /// Whether a same-target re-dispatch waits out the policy backoff first.
    /// Only a completion retries on ordinary errors, so only it sleeps.
    pub(super) const fn sleeps_between_attempts(self) -> bool {
        match self {
            Self::Complete => true,
            Self::Stream | Self::CountTokens => false,
        }
    }

    /// Whether a committed attempt records a breaker success. A stream
    /// commits at first content, which proves only a half-open probe; a
    /// healthy breaker's failure count must keep accumulating mid-stream
    /// errors, so the stream tail settles it instead.
    pub(super) const fn records_success_on_commit(self) -> bool {
        match self {
            Self::Complete | Self::CountTokens => true,
            Self::Stream => false,
        }
    }
}

/// Safe, structured facts pulled from an [`Error`] for the router's
/// class-decision observability. Deliberately EXCLUDES the body and the
/// Display/Debug string: only the numeric status and the
/// already-structured classifier tokens are carried, so no prompt, body,
/// header, or free-form upstream message text can leak into a field.
/// `status` is `Some` iff the error is an [`Error::Upstream`].
#[derive(Debug, Clone, Copy)]
pub(super) struct UpstreamFacts<'a> {
    pub(super) status: Option<u16>,
    pub(super) upstream_type: Option<&'a str>,
    pub(super) upstream_code: Option<&'a str>,
}

pub(super) fn upstream_facts(err: &Error) -> UpstreamFacts<'_> {
    match err {
        Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => UpstreamFacts {
            status: Some(*status),
            upstream_type: upstream_type.as_deref(),
            upstream_code: upstream_code.as_deref(),
        },
        _ => UpstreamFacts {
            status: None,
            upstream_type: None,
            upstream_code: None,
        },
    }
}

/// Stable, low-cardinality label for a [`FailureClass`]. The
/// `FeatureUnsupported` capability is surfaced in its own field, so the
/// label collapses that variant to a bare token. Fail-closed: any class
/// the classifier gains later renders as `unknown`.
pub(super) const fn class_label(class: &FailureClass) -> &'static str {
    match class {
        FailureClass::RateLimited => "rate_limited",
        FailureClass::Auth => "auth",
        FailureClass::BadRequest => "bad_request",
        FailureClass::ContentPolicy => "content_policy",
        FailureClass::ContextWindow => "context_window",
        FailureClass::ServerError => "server_error",
        FailureClass::Timeout => "timeout",
        FailureClass::NetworkError => "network_error",
        FailureClass::Overloaded => "overloaded",
        FailureClass::FeatureUnsupported { .. } => "feature_unsupported",
        FailureClass::Unknown => "unknown",
        _ => "unknown",
    }
}

/// Stable label for how the classification was decided.
pub(super) const fn matched_by_label(matched_by: MatchedBy) -> &'static str {
    match matched_by {
        MatchedBy::Variant => "variant",
        MatchedBy::Status => "status",
        MatchedBy::UpstreamType => "upstream_type",
    }
}

/// The upstream HTTP status carried by `err`, for the per-provider class
/// remap lookup ONLY: `Some` for an [`Error::Upstream`] status in
/// `400..=599`, `None` for status 0 and every non-upstream variant. This
/// seam consults the target's own `class_overrides`, never `policy`.
pub(super) fn upstream_status_for_remap(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream { status, .. } if (400..=599).contains(status) => Some(*status),
        _ => None,
    }
}

/// Apply the operator's per-provider status remap on top of the
/// classifier's native decision. A `status` that keys into `overrides`
/// replaces the class with the operator's override, keeping the NATIVE
/// `matched_by` -- the remap changes WHICH class was decided, not HOW the
/// classifier itself matched. No status, or no matching key (including
/// the empty-map default), returns `native` unchanged. Returns the
/// effective `ClassifiedFailure` plus whether a remap fired.
pub(super) fn apply_remap(
    native: ClassifiedFailure,
    status: Option<u16>,
    overrides: &BTreeMap<u16, FailureClass>,
) -> (ClassifiedFailure, bool) {
    match status.and_then(|s| overrides.get(&s)) {
        Some(class) => (
            ClassifiedFailure {
                class: class.clone(),
                matched_by: native.matched_by,
            },
            true,
        ),
        None => (native, false),
    }
}
