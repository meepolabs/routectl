//! Typed reason an upstream gave for rejecting a request, carried on
//! [`crate::Error::Upstream`] beside the coarse failure class.
//!
//! The vocabulary is closed and additive only: a variant, once added, keeps
//! its token. `None` on the carrier means unclassified, which is today's
//! policy for every rejection. A reason refines an outcome; it never changes
//! its [`crate::failure_class::FailureClass`] (both credential reasons still
//! classify `Auth`).

/// Why an upstream rejected a request, when the rejection carried exact
/// evidence of it. Readers of a persisted or rendered token treat one they do
/// not recognize as unknown ([`UpstreamReason::from_token`] returns `None`),
/// never as an error.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamReason {
    /// The credential was valid once and has expired.
    CredentialExpired,
    /// The credential is not valid: revoked, malformed, or never issued.
    CredentialInvalid,
}

impl UpstreamReason {
    /// The stable kebab-case token for this reason: the single source of the
    /// vocabulary in logs, persisted rows, and client-facing detail.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CredentialExpired => "credential-expired",
            Self::CredentialInvalid => "credential-invalid",
        }
    }

    /// The reason a token names, matched exactly (no trimming or case
    /// folding). `None` for any token outside the vocabulary.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "credential-expired" => Some(Self::CredentialExpired),
            "credential-invalid" => Some(Self::CredentialInvalid),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UpstreamReason;

    #[test]
    fn each_reason_round_trips_through_its_token() {
        let rows = [
            (UpstreamReason::CredentialExpired, "credential-expired"),
            (UpstreamReason::CredentialInvalid, "credential-invalid"),
        ];
        for (reason, token) in rows {
            assert_eq!(reason.as_str(), token, "{reason:?}");

            let parsed = UpstreamReason::from_token(reason.as_str());

            assert_eq!(parsed, Some(reason), "{token}");
        }
    }

    #[test]
    fn from_token_reads_a_token_outside_the_vocabulary_as_unknown() {
        let rows = [
            ("empty", ""),
            ("unknown", "access-denied"),
            ("case variant", "Credential-Expired"),
            ("padded", " credential-expired"),
        ];
        for (name, token) in rows {
            assert_eq!(UpstreamReason::from_token(token), None, "{name}");
        }
    }
}
