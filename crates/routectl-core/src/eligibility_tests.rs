use super::EligibilityReason;

const TOKENS: [(EligibilityReason, &str, bool); 6] = [
    (
        EligibilityReason::CredentialUnusable,
        "credential_unusable",
        false,
    ),
    (EligibilityReason::AccountParked, "account_parked", false),
    (EligibilityReason::BreakerOpen, "breaker_open", false),
    (EligibilityReason::RateLimited, "rate_limited", false),
    (EligibilityReason::Capability, "capability", true),
    (EligibilityReason::ContextWindow, "context_window", true),
];

#[test]
fn each_reason_has_its_frozen_token_and_round_trips() {
    for (reason, token, _) in TOKENS {
        assert_eq!(reason.as_str(), token, "{reason:?}");

        let parsed = EligibilityReason::from_token(reason.as_str());

        assert_eq!(parsed, Some(reason), "{token}");
    }
}

#[test]
fn only_capability_and_context_window_are_request_only() {
    for (reason, _, request_only) in TOKENS {
        assert_eq!(reason.is_request_only(), request_only, "{reason:?}");
    }
}

#[test]
fn from_token_reads_a_token_outside_the_vocabulary_as_unknown() {
    let rows = [
        ("empty", ""),
        ("unknown", "quota_exhausted"),
        ("case variant", "Breaker_Open"),
        ("kebab variant", "breaker-open"),
        ("padded", " rate_limited"),
    ];
    for (name, token) in rows {
        assert_eq!(EligibilityReason::from_token(token), None, "{name}");
    }
}

#[test]
fn priority_ranks_credential_first_and_context_window_last() {
    let expected = [
        (EligibilityReason::CredentialUnusable, 6),
        (EligibilityReason::AccountParked, 5),
        (EligibilityReason::BreakerOpen, 4),
        (EligibilityReason::RateLimited, 3),
        (EligibilityReason::Capability, 2),
        (EligibilityReason::ContextWindow, 1),
    ];
    for (reason, rank) in expected {
        assert_eq!(reason.priority(), rank, "{reason:?}");
    }
    for pair in expected.windows(2) {
        let (higher, lower) = (pair[0].0, pair[1].0);
        assert!(
            higher.priority() > lower.priority(),
            "{higher:?} must outrank {lower:?}"
        );
    }
}

#[test]
fn a_locally_unavailable_error_displays_only_its_token() {
    for (reason, token, _) in TOKENS {
        let err = crate::Error::LocallyUnavailable {
            reason,
            retry_after: Some(std::time::Duration::from_secs(30)),
        };

        assert_eq!(err.to_string(), format!("locally unavailable: {token}"));
        assert!(format!("{err:?}").contains(token), "{err:?}");
    }
}
