//! How far an upstream failure reaches beyond the target that returned it,
//! and the recognized upstream patterns that establish a wider reach.
//!
//! A separate axis from [`crate::UpstreamReason`]: a reason says why a
//! credential was rejected, a scope says which neighbouring targets share the
//! failure. Both vocabularies are closed and additive only: a variant, once
//! added, keeps its token. `None` on the [`crate::Error::Upstream`] carrier
//! means unrecognized, which reads as [`FailureScope::Target`].

/// How far a failure reaches: the set of targets that would fail the same
/// way if tried next.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureScope {
    /// Only the target that failed.
    Target,
    /// Every target that shares the failed target's credential or account.
    Account,
    /// Every target served by the same upstream endpoint.
    Endpoint,
    /// Every target dispatching to the same `[models]` entry.
    Model,
}

/// An upstream failure pattern recognized from exact evidence on the
/// response, each with a fixed [`FailureScope`] and retry verdict. Readers of
/// a persisted or rendered token treat one they do not recognize as unknown
/// ([`ScopeRow::from_token`] returns `None`), never as an error.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeRow {
    /// An Anthropic OAuth 429 whose `rate_limit_error` envelope carries the
    /// bare message `Error` and none of the rate-limit headers a genuine
    /// limit sends.
    AnthropicBareRateLimit,
}

impl ScopeRow {
    /// The stable kebab-case token for this row: the single source of the
    /// vocabulary in logs, persisted rows, and status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicBareRateLimit => "anthropic-bare-rate-limit",
        }
    }

    /// The row a token names, matched exactly (no trimming or case folding).
    /// `None` for any token outside the vocabulary.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "anthropic-bare-rate-limit" => Some(Self::AnthropicBareRateLimit),
            _ => None,
        }
    }

    /// How far a failure matching this row reaches.
    pub const fn scope(self) -> FailureScope {
        match self {
            Self::AnthropicBareRateLimit => FailureScope::Endpoint,
        }
    }

    /// Whether retrying within [`ScopeRow::scope`] can succeed.
    pub const fn retryable(self) -> bool {
        match self {
            Self::AnthropicBareRateLimit => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureScope, ScopeRow};

    #[test]
    fn each_row_round_trips_through_its_token() {
        let rows = [(
            ScopeRow::AnthropicBareRateLimit,
            "anthropic-bare-rate-limit",
        )];
        for (row, token) in rows {
            assert_eq!(row.as_str(), token, "{row:?}");

            let parsed = ScopeRow::from_token(row.as_str());

            assert_eq!(parsed, Some(row), "{token}");
        }
    }

    #[test]
    fn from_token_reads_a_token_outside_the_vocabulary_as_unknown() {
        let rows = [
            ("empty", ""),
            ("unknown", "credential-expired"),
            ("case variant", "Anthropic-Bare-Rate-Limit"),
            ("padded", " anthropic-bare-rate-limit"),
            ("trailing pad", "anthropic-bare-rate-limit "),
        ];
        for (name, token) in rows {
            assert_eq!(ScopeRow::from_token(token), None, "{name}");
        }
    }

    #[test]
    fn each_row_carries_its_scope_and_retry_verdict() {
        let rows = [(
            ScopeRow::AnthropicBareRateLimit,
            FailureScope::Endpoint,
            false,
        )];
        for (row, scope, retryable) in rows {
            assert_eq!(row.scope(), scope, "{row:?}");
            assert_eq!(row.retryable(), retryable, "{row:?}");
        }
    }
}
