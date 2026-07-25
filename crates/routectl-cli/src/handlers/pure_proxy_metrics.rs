//! Forwarded-mode (pure-proxy) ingress admission-rejection counter +
//! structured rejection log.
//!
//! Reuses the hand-rolled `AtomicU64` + tracing pattern of
//! `crate::proxy::metrics` (routectl has no metrics registry or exporter,
//! so `pure_proxy_rejections_total{reason}` is a lock-free counter array
//! indexed by a CLOSED enum of reasons), but DEVIATES from it on lifetime:
//! `ProxyMetrics` is an `Arc<ProxyMetrics>` constructed per proxy listener
//! and threaded through the request path, whereas this counter is a single
//! process-global `static LazyLock`. That deviation is deliberate and
//! acceptable here: ingress admission rejects a forwarded request BEFORE a
//! listener-scoped metrics carrier is in hand, so there is no per-request
//! object to hang the counter on at this point. A process-global counter is
//! the simplest correct home for a whole-process admission tally, and it
//! stays leak-safe by construction: the counter dimension can only ever be
//! one of the two fixed reason strings -- NEVER a token, header, or body
//! value -- because the only input `incr` accepts is a
//! `PureProxyRejectionReason`, and the only input the rejection log
//! accepts is that reason plus a boolean.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;

/// Closed set of forwarded-mode ingress admission rejection reasons. This is
/// the ONLY dimension of `pure_proxy_rejections_total`; being a closed enum,
/// a counter label can never carry a token / header / body value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PureProxyRejectionReason {
    /// MITM-inference-path request (seam header present) with no inbound
    /// `Authorization` bearer (Claude Code not logged into claude.ai).
    /// HTTP 401.
    TokenMissing,
    /// MITM-inference-path request missing `x-claude-code-session-id`; fail
    /// before egress rather than minting identity. HTTP 400.
    IdentityMissing,
}

impl PureProxyRejectionReason {
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        match self {
            Self::TokenMissing => 0,
            Self::IdentityMissing => 1,
        }
    }

    /// The stable, safe label written to both the counter dimension and the
    /// rejection log's `reason` field. Fixed strings only.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TokenMissing => "token_missing",
            Self::IdentityMissing => "identity_missing",
        }
    }

    /// HTTP status each rejection maps to: the
    /// absent-credential case is 401, every other admission failure is a
    /// 400 bad-request shape.
    pub(crate) const fn status(self) -> StatusCode {
        match self {
            Self::TokenMissing => StatusCode::UNAUTHORIZED,
            Self::IdentityMissing => StatusCode::BAD_REQUEST,
        }
    }

    /// Every variant, for exhaustive tests.
    #[cfg(test)]
    pub(crate) const ALL: [Self; Self::COUNT] = [Self::TokenMissing, Self::IdentityMissing];
}

/// Lock-free `pure_proxy_rejections_total{reason}` counter. One relaxed
/// atomic per reason; every increment is a single atomic op that never
/// blocks or panics, and the counter is never read for control flow.
#[derive(Debug)]
pub(crate) struct PureProxyRejections {
    by_reason: [AtomicU64; PureProxyRejectionReason::COUNT],
}

impl Default for PureProxyRejections {
    fn default() -> Self {
        Self {
            by_reason: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl PureProxyRejections {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Bump the counter for one closed-enum reason.
    pub(crate) fn incr(&self, reason: PureProxyRejectionReason) {
        self.by_reason[reason.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Current count for one reason.
    #[cfg(test)]
    pub(crate) fn count(&self, reason: PureProxyRejectionReason) -> u64 {
        self.by_reason[reason.index()].load(Ordering::Relaxed)
    }

    /// Sum across every reason.
    #[cfg(test)]
    pub(crate) fn total(&self) -> u64 {
        self.by_reason
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }
}

/// Process-global counter instance. Ingress admission increments this via
/// [`record_rejection`]; the unit tests exercise a fresh
/// [`PureProxyRejections`] directly so per-reason assertions stay
/// deterministic under the parallel suite.
static PURE_PROXY_REJECTIONS: LazyLock<PureProxyRejections> =
    LazyLock::new(PureProxyRejections::new);

/// Record one forwarded-mode admission rejection: bump the global
/// `pure_proxy_rejections_total{reason}` counter AND emit ONE structured
/// WARN carrying SAFE dimensions only -- `reason`, `status`, a fixed
/// `credential_source` token, and whether an inbound client session id was
/// present -- NEVER the forwarded token, in a field or the message. Mirrors
/// `routectl_router`'s `log_forwarded_auth_terminal`.
pub(crate) fn record_rejection(reason: PureProxyRejectionReason, has_client_session_id: bool) {
    PURE_PROXY_REJECTIONS.incr(reason);
    tracing::warn!(
        reason = reason.as_str(),
        status = reason.status().as_u16(),
        credential_source = "forwarded",
        has_client_session_id,
        "forwarded-mode ingress request rejected at admission",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Incrementing one reason bumps only that reason's dimension and the
    /// total, leaving the other one untouched.
    #[test]
    fn incr_bumps_only_the_matching_reason() {
        // Arrange
        let counter = PureProxyRejections::new();

        // Act
        counter.incr(PureProxyRejectionReason::TokenMissing);

        // Assert
        assert_eq!(counter.count(PureProxyRejectionReason::TokenMissing), 1);
        assert_eq!(counter.count(PureProxyRejectionReason::IdentityMissing), 0);
        assert_eq!(counter.total(), 1);
    }

    /// Each reason is counted independently and accumulates across calls.
    #[test]
    fn incr_accumulates_per_reason_independently() {
        // Arrange
        let counter = PureProxyRejections::new();

        // Act
        counter.incr(PureProxyRejectionReason::IdentityMissing);
        counter.incr(PureProxyRejectionReason::IdentityMissing);
        counter.incr(PureProxyRejectionReason::TokenMissing);

        // Assert
        assert_eq!(counter.count(PureProxyRejectionReason::IdentityMissing), 2);
        assert_eq!(counter.count(PureProxyRejectionReason::TokenMissing), 1);
        assert_eq!(counter.total(), 3);
    }

    /// The counter dimension is a CLOSED enum: its only labels are the two
    /// fixed, safe reason strings. This is the structural guarantee that a
    /// dimension can never carry a token, header, or body value.
    #[test]
    fn reason_labels_are_a_closed_safe_set() {
        // Arrange: the only labels the counter dimension can ever take.
        let labels: Vec<&str> = PureProxyRejectionReason::ALL
            .iter()
            .map(|r| r.as_str())
            .collect();

        // Assert: exactly the two documented reasons, nothing else.
        assert_eq!(labels, vec!["token_missing", "identity_missing"]);
        // Every label is a short, fixed token -- never anything that could
        // carry request-derived data (no whitespace, no long values).
        for label in labels {
            assert!(!label.is_empty());
            assert!(!label.contains(char::is_whitespace));
            assert!(label.len() <= 32, "reason labels are short tokens");
        }
    }

    /// `record_rejection` drives the process-global `PURE_PROXY_REJECTIONS`
    /// static (the other tests exercise fresh instances, so nothing else
    /// proves the production path touches the real counter). Asserted on the
    /// DELTA around the call -- never an absolute value, and never an exact
    /// count: the static is shared across the whole test binary and the
    /// ingress admission tests drive the same reasons in parallel, so only a
    /// monotonic "the counter moved" claim is race-free.
    #[test]
    fn record_rejection_moves_the_process_global_counter() {
        // Arrange
        let reason = PureProxyRejectionReason::IdentityMissing;
        let before = PURE_PROXY_REJECTIONS.count(reason);

        // Act
        record_rejection(reason, true);

        // Assert: strictly increased. The counter only ever goes up, so a
        // concurrent bump of the same reason can only raise the delta,
        // never make this false.
        assert!(
            PURE_PROXY_REJECTIONS.count(reason) > before,
            "record_rejection must advance the global counter for its reason",
        );
    }
}
