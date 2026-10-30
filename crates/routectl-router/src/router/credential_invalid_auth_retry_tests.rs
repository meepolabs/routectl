//! The generic 401 refresh-and-retry spends its one redial only where a
//! refresh could help: an untyped 401 or one typed as an expired credential
//! is refreshed and redialed once, while a 401 typed as an invalid credential
//! parks the seat after its single dial, since no refresh can fix a key the
//! upstream says was never valid. Driven through every dispatch surface.

use std::sync::atomic::Ordering;

use routectl_core::UpstreamReason;

use super::credential_park_dispatch_tests::{
    INVALID, ScriptedProvider, Step, Surface, chain_router, dispatch, park_reason, request,
};

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];
const EXPIRED: Option<UpstreamReason> = Some(UpstreamReason::CredentialExpired);

/// What one request did to the first seat.
#[derive(Debug, PartialEq, Eq)]
struct SeatOutcome {
    dials: usize,
    refreshes: usize,
    park: Option<UpstreamReason>,
}

#[tokio::test]
async fn a_401_is_refreshed_and_redialed_once_unless_it_names_an_invalid_credential() {
    let cases = [
        (
            "untyped 401 then success",
            vec![Step::Reject(401, None), Step::Ok],
            SeatOutcome {
                dials: 2,
                refreshes: 1,
                park: None,
            },
        ),
        (
            "401 expired",
            vec![Step::Reject(401, EXPIRED)],
            SeatOutcome {
                dials: 2,
                refreshes: 1,
                park: EXPIRED,
            },
        ),
        (
            "401 invalid",
            vec![Step::Reject(401, INVALID)],
            SeatOutcome {
                dials: 1,
                refreshes: 0,
                park: INVALID,
            },
        ),
    ];
    for surface in SURFACES {
        for (name, script, expected) in &cases {
            // Arrange
            let primary = ScriptedProvider::new("p-a", script);
            let sibling = ScriptedProvider::new("p-b", &[Step::Ok]);
            let router = chain_router(primary.clone(), sibling, false);

            // Act
            dispatch(&router, surface, request(None, false)).await;

            // Assert
            let outcome = SeatOutcome {
                dials: primary.calls.load(Ordering::SeqCst),
                refreshes: primary.refreshes.load(Ordering::SeqCst),
                park: park_reason(&router, "m-a"),
            };
            assert_eq!(&outcome, expected, "{surface:?} {name}");
        }
    }
}
