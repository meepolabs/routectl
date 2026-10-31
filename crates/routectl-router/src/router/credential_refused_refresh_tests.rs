//! A request whose seat draws a 401 and then has its refresh token refused
//! marks the seat unusable under the admission's credential generation and
//! walks on to the next target. Any other refresh failure, a probe, and a
//! refusal a reload has overtaken keep the request's own error and mark
//! nothing. Driven through every dispatch surface, because each walks its own
//! chain.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use routectl_core::{EligibilityReason, Error, UpstreamReason};

use super::super::credential_park_dispatch_tests::{
    Hold, INVALID, RefreshErr, ScriptedProvider, Step, Surface, chain_router, chain_router_with,
    dispatch, dispatch_err, park_reason, request,
};
use super::super::{AliasValue, Router, RouterOptions};
use crate::runtime_state::UnusableCause;

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];
const REFUSED: Option<RefreshErr> = Some(INVALID);
const TRANSIENT: Option<RefreshErr> = Some(None);
const MARK_WARN: &str = "seat refresh token refused; marking the seat unusable";

fn unusable_cause(router: &Router, state_key: &str) -> Option<UnusableCause> {
    router
        .gate_status_for_tests(state_key)
        .credential_unusable
        .map(|mark| mark.cause)
}

fn refused_seat(id: &str, refresh: Option<RefreshErr>) -> Arc<ScriptedProvider> {
    ScriptedProvider::with_refresh(id, &[Step::Reject(401, None)], refresh)
}

fn is_unusable_refusal(err: Option<&Error>) -> bool {
    matches!(
        err,
        Some(Error::LocallyUnavailable {
            reason: EligibilityReason::CredentialUnusable,
            retry_after: None,
        })
    )
}

#[tokio::test]
async fn a_refused_refresh_marks_the_seat_unusable_and_the_next_seat_serves() {
    for surface in SURFACES {
        // Arrange
        let primary = refused_seat("p-a", REFUSED);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary.clone(), sibling.clone(), false);

        // Act
        let (served, events) = routectl_testkit::with_capture(Box::pin(dispatch(
            &router,
            surface,
            request(None, false),
        )))
        .await;
        let served_again = dispatch(&router, surface, request(None, false)).await;

        // Assert
        let label = format!("{surface:?}");
        assert!(served, "{label}: the sibling serves the first request");
        assert!(served_again, "{label}: the sibling serves the next request");
        assert_eq!(
            unusable_cause(&router, "m-a"),
            Some(UnusableCause::Revoked),
            "{label}"
        );
        assert_eq!(park_reason(&router, "m-a"), None, "{label}: no park");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(primary.refreshes.load(Ordering::SeqCst), 1, "{label}");
        assert_eq!(sibling.calls.load(Ordering::SeqCst), 2, "{label}");
        let warns: Vec<(&str, Option<&str>, Option<&str>)> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == MARK_WARN)
            .map(|e| (e.message.as_str(), e.field("account"), e.field("cause")))
            .collect();
        assert_eq!(
            warns,
            [(MARK_WARN, Some("#p-a"), Some("revoked"))],
            "{label}: one WARN at the transition"
        );
    }
}

#[tokio::test]
async fn a_refused_refresh_on_the_only_seat_answers_credential_unusable() {
    for surface in SURFACES {
        // Arrange
        let primary = refused_seat("p-a", REFUSED);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router_with(primary, sibling.clone(), false, None, |config| {
            config
                .aliases
                .insert("alias".into(), AliasValue::Chain(vec!["m-a".into()]));
        });

        // Act
        let err = dispatch_err(&router, surface, request(None, false)).await;

        // Assert
        assert!(is_unusable_refusal(err.as_ref()), "{surface:?}: {err:?}");
        assert_eq!(sibling.calls.load(Ordering::SeqCst), 0, "{surface:?}");
    }
}

#[tokio::test]
async fn a_refused_refresh_with_fallbacks_disabled_stops_the_walk() {
    // Arrange
    let primary = refused_seat("p-a", REFUSED);
    let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
    let router = chain_router(primary, sibling.clone(), false);
    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;

    // Act
    let dispatched = router
        .complete_with_options(request(None, false), opts)
        .await;

    // Assert
    let err = dispatched.result.err();
    assert!(is_unusable_refusal(err.as_ref()), "{err:?}");
    assert_eq!(sibling.calls.load(Ordering::SeqCst), 0);
    assert_eq!(unusable_cause(&router, "m-a"), Some(UnusableCause::Revoked));
}

#[tokio::test]
async fn a_refresh_failure_that_must_not_judge_the_seat_keeps_its_error_and_marks_nothing() {
    // (case, refresh failure, request)
    let rows = [
        ("transient refresh failure", TRANSIENT, request(None, false)),
        (
            "probe with a refused refresh",
            REFUSED,
            request(Some(1), false),
        ),
    ];
    for surface in SURFACES {
        for (case, refresh, req) in rows.clone() {
            // Arrange
            let primary = refused_seat("p-a", refresh);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary.clone(), sibling.clone(), false);

            // Act
            let err = dispatch_err(&router, surface, req).await;

            // Assert
            let label = format!("{surface:?} {case}");
            assert_eq!(
                primary.refreshes.load(Ordering::SeqCst),
                1,
                "{label}: the refresh ran"
            );
            let kept = match refresh {
                REFUSED => matches!(
                    err,
                    Some(Error::Credential {
                        reason: UpstreamReason::CredentialInvalid,
                        ..
                    })
                ),
                _ => matches!(err, Some(Error::Auth(_))),
            };
            assert!(kept, "{label}: the refresh error is surfaced, got {err:?}");
            assert_eq!(unusable_cause(&router, "m-a"), None, "{label}");
            assert_eq!(park_reason(&router, "m-a"), None, "{label}");
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 0, "{label}");
        }
    }
}

#[tokio::test]
async fn a_refused_refresh_admitted_before_a_reset_marks_nothing() {
    for surface in SURFACES {
        for (reset, expected) in [(true, None), (false, Some(UnusableCause::Revoked))] {
            // Arrange: the seat is usable at admission; the attempt is held
            // inside the upstream call.
            let hold = Arc::new(Hold::default());
            let primary =
                ScriptedProvider::held("p-a", &[Step::Reject(401, None)], REFUSED, hold.clone());
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary, sibling.clone(), false);

            // Act: a reset lands while the attempt is in flight, then the
            // attempt's 401 comes back and its refresh is refused.
            let in_flight = dispatch(&router, surface, request(None, false));
            let resetter = async {
                hold.entered.notified().await;
                if reset {
                    router.lift_entry_credential_parks();
                }
                hold.release.notify_one();
            };
            let (served, ()) = tokio::join!(in_flight, resetter);

            // Assert
            let label = format!("{surface:?} reset={reset}");
            assert_eq!(unusable_cause(&router, "m-a"), expected, "{label}");
            assert_eq!(served, !reset, "{label}: only a mark walks on");
            let expected_sibling = usize::from(!reset);
            assert_eq!(
                sibling.calls.load(Ordering::SeqCst),
                expected_sibling,
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn count_tokens_walks_past_an_unusable_seat_and_refuses_when_every_seat_is_unusable() {
    // (case, mark the sibling too)
    let rows = [
        ("first seat unusable", false),
        ("every seat unusable", true),
    ];
    for (case, both) in rows {
        // Arrange
        let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary.clone(), sibling.clone(), false);
        let mut marked = vec!["m-a"];
        if both {
            marked.push("m-b");
        }
        for key in marked {
            assert!(router.mark_credential_unusable_for_tests(key, UnusableCause::Revoked));
        }

        // Act
        let counted = router.count_tokens(request(None, false)).await;

        // Assert
        assert_eq!(primary.calls.load(Ordering::SeqCst), 0, "{case}");
        if both {
            let err = counted.err();
            assert!(is_unusable_refusal(err.as_ref()), "{case}: {err:?}");
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 0, "{case}");
        } else {
            assert_eq!(counted.ok().map(|c| c.input_tokens), Some(7), "{case}");
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 1, "{case}");
        }
    }
}
