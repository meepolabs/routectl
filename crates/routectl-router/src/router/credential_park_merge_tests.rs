//! How the chain walk folds one gate refusal into the error it holds: a held
//! dialed or upstream error is kept; between local refusals the reason with
//! the higher priority names the refusal and the wait is the earliest known.

use std::time::Duration;

use routectl_core::{EligibilityReason, Error};

use super::merge_gate_refusal;

const fn refusal(reason: EligibilityReason, secs: Option<u64>) -> Error {
    Error::LocallyUnavailable {
        reason,
        retry_after: match secs {
            Some(secs) => Some(Duration::from_secs(secs)),
            None => None,
        },
    }
}

/// The reason and wait in seconds a folded local refusal carries.
fn folded(err: Option<&Error>) -> Option<(EligibilityReason, Option<u64>)> {
    match err? {
        Error::LocallyUnavailable {
            reason,
            retry_after,
        } => Some((*reason, retry_after.map(|wait| wait.as_secs()))),
        _ => None,
    }
}

#[test]
fn local_refusals_fold_to_the_top_reason_and_the_earliest_known_wait() {
    use EligibilityReason::{AccountParked, BreakerOpen, CredentialUnusable, RateLimited};
    // (row, held refusal, new refusal, expected reason, expected wait)
    let rows = [
        (
            "parked 30 s then breaker 10 s",
            refusal(AccountParked, Some(30)),
            refusal(BreakerOpen, Some(10)),
            AccountParked,
            Some(10),
        ),
        (
            "unusable unknown then breaker 10 s",
            refusal(CredentialUnusable, None),
            refusal(BreakerOpen, Some(10)),
            CredentialUnusable,
            Some(10),
        ),
        (
            "breaker unknown then rpm 5 s",
            refusal(BreakerOpen, None),
            refusal(RateLimited, Some(5)),
            BreakerOpen,
            Some(5),
        ),
        (
            "rpm 5 s then breaker 20 s",
            refusal(RateLimited, Some(5)),
            refusal(BreakerOpen, Some(20)),
            BreakerOpen,
            Some(5),
        ),
        (
            "breaker 10 s then rpm unknown",
            refusal(BreakerOpen, Some(10)),
            refusal(RateLimited, None),
            BreakerOpen,
            Some(10),
        ),
        (
            "both unknown",
            refusal(AccountParked, None),
            refusal(BreakerOpen, None),
            AccountParked,
            None,
        ),
    ];
    for (row, held, new, reason, wait) in rows {
        // Arrange
        let mut last_err = Some(held);

        // Act
        merge_gate_refusal(&mut last_err, new);

        // Assert
        assert_eq!(folded(last_err.as_ref()), Some((reason, wait)), "{row}");
    }
}

#[test]
fn the_first_refusal_is_held_as_is() {
    // Arrange
    let mut last_err = None;

    // Act
    merge_gate_refusal(&mut last_err, refusal(EligibilityReason::RateLimited, None));

    // Assert
    assert_eq!(
        folded(last_err.as_ref()),
        Some((EligibilityReason::RateLimited, None))
    );
}

#[test]
fn a_held_upstream_error_outranks_any_local_refusal() {
    // Arrange
    let mut last_err = Some(Error::upstream("p-a", 503, "real upstream down"));

    // Act
    merge_gate_refusal(
        &mut last_err,
        refusal(EligibilityReason::BreakerOpen, Some(10)),
    );

    // Assert
    assert!(
        matches!(last_err, Some(Error::Upstream { status: 503, .. })),
        "the dialed error must be kept, got {last_err:?}",
    );
}
