//! A success lifts the credential park only in the generation its attempt
//! was admitted under, and a reload's lift costs at most one request per
//! model slot on the credential account before a still-bad credential
//! re-parks the account.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use routectl_core::UpstreamReason;
use routectl_core::failure_class::LastOutcome;

use super::super::Router;
use super::super::credential_park_dispatch_tests::{
    Hold, INVALID, ScriptedProvider, Step, Surface, chain_router, dispatch, park_reason, rejection,
    request,
};

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

fn last_outcome(router: &Router, state_key: &str) -> Option<LastOutcome> {
    router.gate_status_for_tests(state_key).last_outcome
}

#[tokio::test]
async fn a_success_admitted_before_a_reload_leaves_a_park_set_after_it() {
    for surface in SURFACES {
        for (reload, expected) in [(true, INVALID), (false, None)] {
            // Arrange: a healthy seat; the attempt that will succeed is held
            // inside the upstream call after its admission.
            let hold = Arc::new(Hold::default());
            let primary = ScriptedProvider::held("p-a", &[Step::Ok], None, hold.clone());
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary, sibling, false);
            let label = format!("{surface:?} reload={reload}");

            // Act: while the attempt is in flight, an optional reload starts a
            // new generation and a rejection admitted after it parks the
            // seat; then the held attempt's success comes back.
            let in_flight = dispatch(&router, surface, request(None, false));
            let interleave = async {
                hold.entered.notified().await;
                if reload {
                    router.lift_entry_credential_parks();
                }
                let generation = router
                    .admit_for_credential_tests("m-a")
                    .expect("a healthy seat admits");
                let parked = router.settle_credential_rejection_for_tests(
                    "m-a",
                    &rejection("p-a", 403, INVALID),
                    generation,
                );
                hold.release.notify_one();
                parked
            };
            let (served, parked) = tokio::join!(in_flight, interleave);

            // Assert
            assert!(served, "{label}: the held attempt must succeed");
            assert!(
                parked,
                "{label}: the rejection must park before the success"
            );
            assert_eq!(park_reason(&router, "m-a"), expected, "{label}");
            assert_eq!(
                last_outcome(&router, "m-a"),
                Some(LastOutcome::Ok),
                "{label}: the breaker success is recorded either way",
            );
        }
    }
}

#[tokio::test]
async fn the_seam_lifts_a_park_only_in_the_success_generation() {
    for stale in [true, false] {
        // Arrange: a park set in the current generation.
        let primary = ScriptedProvider::new("p-a", &[Step::Ok]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary, sibling, false);
        let before_reload = router
            .admit_for_credential_tests("m-a")
            .expect("a healthy seat admits");
        router.lift_entry_credential_parks();
        let current = router
            .admit_for_credential_tests("m-a")
            .expect("a healthy seat admits");
        assert!(router.settle_credential_rejection_for_tests(
            "m-a",
            &rejection("p-a", 403, INVALID),
            current,
        ));
        let generation = if stale { before_reload } else { current };

        // Act
        router.settle_seat_success_for_tests("m-a", true, generation);

        // Assert
        let expected = stale.then_some(UpstreamReason::CredentialInvalid);
        assert_eq!(park_reason(&router, "m-a"), expected, "stale={stale}");
        assert_eq!(
            last_outcome(&router, "m-a"),
            Some(LastOutcome::Ok),
            "stale={stale}"
        );
    }
}

#[tokio::test]
async fn a_reload_erases_a_park_and_the_next_rejection_parks_again() {
    for surface in SURFACES {
        // Arrange: a seat whose credential stays bad, parked by a rejection
        // admitted before the reload.
        let primary = ScriptedProvider::new("p-a", &[Step::Reject(403, INVALID)]);
        let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
        let router = chain_router(primary.clone(), sibling, false);
        dispatch(&router, surface, request(None, false)).await;
        assert_eq!(park_reason(&router, "m-a"), INVALID, "{surface:?} premise");

        // Act: the reload lifts the park, then traffic resumes.
        let cleared = router.lift_entry_credential_parks();
        let after_clear = park_reason(&router, "m-a");
        dispatch(&router, surface, request(None, false)).await;
        let after_next = park_reason(&router, "m-a");
        dispatch(&router, surface, request(None, false)).await;

        // Assert: the clear erased the park, the first attempt after it paid
        // the one request and re-parked, and the seat is skipped again.
        assert_eq!(cleared, 1, "{surface:?}");
        assert_eq!(after_clear, None, "{surface:?}");
        assert_eq!(after_next, INVALID, "{surface:?}");
        assert_eq!(primary.calls.load(Ordering::SeqCst), 2, "{surface:?}");
    }
}
