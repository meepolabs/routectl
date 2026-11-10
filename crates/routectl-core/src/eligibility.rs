//! Why routectl itself declined to send a request to a target, carried on
//! [`crate::Error::LocallyUnavailable`].
//!
//! The vocabulary is closed and its tokens are client-facing: a client SDK
//! may branch on them, so a token never changes once shipped. When several
//! targets are refused for different reasons, the one with the highest
//! [`EligibilityReason::priority`] names the refusal.

/// Why a target was not eligible for a request, decided locally before any
/// upstream contact. Readers of a persisted or rendered token treat one they
/// do not recognize as unknown ([`EligibilityReason::from_token`] returns
/// `None`), never as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EligibilityReason {
    /// The target's credential is known to be unusable.
    CredentialUnusable,
    /// The target's credential account is parked after a rejection.
    AccountParked,
    /// The target's circuit breaker is open.
    BreakerOpen,
    /// The target's local request-rate limit has no capacity left.
    RateLimited,
    /// The target lacks a capability this request needs.
    Capability,
    /// This request does not fit the target's context window.
    ContextWindow,
}

impl EligibilityReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::CredentialUnusable,
        Self::AccountParked,
        Self::BreakerOpen,
        Self::RateLimited,
        Self::Capability,
        Self::ContextWindow,
    ];

    /// The stable snake_case token for this reason: the single source of the
    /// vocabulary in client error types, logs and persisted rows.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CredentialUnusable => "credential_unusable",
            Self::AccountParked => "account_parked",
            Self::BreakerOpen => "breaker_open",
            Self::RateLimited => "rate_limited",
            Self::Capability => "capability",
            Self::ContextWindow => "context_window",
        }
    }

    /// The reason a token names, matched exactly (no trimming or case
    /// folding). `None` for any token outside the vocabulary.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "credential_unusable" => Some(Self::CredentialUnusable),
            "account_parked" => Some(Self::AccountParked),
            "breaker_open" => Some(Self::BreakerOpen),
            "rate_limited" => Some(Self::RateLimited),
            "capability" => Some(Self::Capability),
            "context_window" => Some(Self::ContextWindow),
            _ => None,
        }
    }

    /// True when the reason belongs to this request rather than to the
    /// target's state: another request may be eligible on the same target.
    pub const fn is_request_only(self) -> bool {
        matches!(self, Self::Capability | Self::ContextWindow)
    }

    /// Rank used to pick one reason among several refusals: a larger value
    /// names the refusal. Credential unusable ranks highest, then account
    /// parked, breaker open, rate limited, capability and context window.
    /// Stated per variant so reordering the declaration never changes it.
    pub const fn priority(self) -> u8 {
        match self {
            Self::CredentialUnusable => 6,
            Self::AccountParked => 5,
            Self::BreakerOpen => 4,
            Self::RateLimited => 3,
            Self::Capability => 2,
            Self::ContextWindow => 1,
        }
    }
}

#[cfg(test)]
#[path = "eligibility_tests.rs"]
mod tests;
