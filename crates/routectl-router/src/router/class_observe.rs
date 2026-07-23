//! Pure classification/observability helpers shared across the dispatch
//! surfaces. Stable, low-cardinality labels for failure classes and the
//! surface an error arm belongs to, plus the safe, structured facts pulled
//! from an [`Error`] for the router's class-decision events. Deliberately
//! excludes any body, prompt, header, token, or free-form upstream message
//! text so nothing sensitive can leak into an observability field.

use routectl_core::Error;
use routectl_core::failure_class::{FailureClass, MatchedBy};

/// Which dispatch surface an error arm belongs to. Carried as a stable
/// `surface` field on the router's class-decision observability events so
/// operators can tell a completion failure from a stream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DispatchSurface {
    Complete,
    Stream,
}

impl DispatchSurface {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Stream => "stream",
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
