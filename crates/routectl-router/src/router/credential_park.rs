//! Seat credential park: the router side of a seat whose upstream rejected
//! the credential itself. Parks on a typed reason, lifts on a success through
//! the seat, and lifts everywhere on a reload. The park state and its gate
//! behavior live in [`crate::runtime_state::ProviderState`].

use std::time::Instant;

use routectl_core::Error;

use super::Router;

impl Router {
    /// Park the credential of the state slot keyed by `state_key` until
    /// `until`, returning `false` when no such slot exists. Test seam for the
    /// reload and dispatch tests that need a parked (or already lapsed) seat
    /// without driving an upstream rejection.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn park_credential_for_tests(
        &self,
        state_key: &str,
        reason: routectl_core::UpstreamReason,
        until: Instant,
    ) -> bool {
        match self.state.get(state_key) {
            Some(slot) => {
                slot.lock().park_credential_until(reason, until);
                true
            }
            None => false,
        }
    }

    /// Lift the credential park on every seat this Router gates, returning how
    /// many were parked. Clear-all rather than per-credential: no credential
    /// -> state_key map exists for every secret kind, and a wrongly cleared
    /// park costs one request per seat before it re-parks.
    pub fn clear_all_credential_parks(&self) -> usize {
        let mut cleared = 0;
        for state in self.state.values() {
            let mut state = state.lock();
            if state.credential_park().is_some() {
                state.clear_credential_park();
                cleared += 1;
            }
        }
        cleared
    }

    /// Park the seat keyed by `state_key` when `err` carries a typed
    /// credential reason, returning whether it parked. A probe never parks: a
    /// minimal availability request is not evidence about the seat a client
    /// will use. Called beside the breaker settle, never inside its debit
    /// gate, because a credential rejection classifies `Auth`, which does not
    /// debit. Leaves the half-open slot to the caller's probe guard.
    pub(super) fn park_credential_on_rejection(
        &self,
        state_key: &str,
        err: &Error,
        is_probe: bool,
    ) -> bool {
        if is_probe {
            return false;
        }
        let Some(reason) = err.upstream_reason() else {
            return false;
        };
        let Some(state) = self.state.get(state_key) else {
            return false;
        };
        state.lock().park_credential(Instant::now(), reason);
        tracing::warn!(
            state_key = %routectl_core::sanitize_for_log(state_key),
            reason = reason.as_str(),
            parked_for_secs = crate::runtime_state::CREDENTIAL_PARK_BACKSTOP.as_secs(),
            "seat credential rejected; parking the seat",
        );
        true
    }

    /// Lift the credential park on the seat keyed by `state_key`: a success
    /// through the seat proves the credential works.
    pub(super) fn clear_credential_park(&self, state_key: &str) {
        if let Some(state) = self.state.get(state_key) {
            state.lock().clear_credential_park();
        }
    }
}
