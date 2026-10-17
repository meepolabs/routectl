//! Seat credential park: the router side of a seat whose upstream rejected
//! the credential itself. Parks on a typed reason, lifts on a success through
//! the seat, and lifts everywhere on a reload. The park state and its gate
//! behavior live in [`crate::runtime_state::ProviderState`].

use std::time::Instant;

use routectl_core::{ChatRequest, Error, sanitize_for_log};

use super::runtime_gate::{ProbeSlotGuard, is_probe_request};
use super::{DispatchTarget, Router};

/// The `gate_kind` a credential-parked refusal is logged under.
pub(super) const CREDENTIAL_PARKED_GATE_KIND: &str = "credential_parked";

#[cfg(test)]
type SettleHook = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    static WITHIN_SUCCESS_SETTLE: std::cell::RefCell<Option<SettleHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` once, on the next success settlement made by this thread,
/// after the breaker success is recorded and before the park is cleared.
/// Lets a test stand in for a concurrent rejection landing inside that
/// window. Thread-local, so it needs a current-thread runtime.
#[cfg(test)]
pub(super) fn set_within_success_settle_hook(hook: impl FnOnce() + 'static) {
    WITHIN_SUCCESS_SETTLE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_within_success_settle_hook() {
    let hook = WITHIN_SUCCESS_SETTLE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

/// Log a gate refusal. A credential park refuses every request for up to an
/// hour and already logged its own WARN when it parked, so its refusals log
/// at DEBUG; every other gate kind stays a WARN.
pub(super) fn log_gate_refusal(
    provider: &str,
    model: &str,
    gate_kind: &'static str,
    gate_err: &Error,
    message: &'static str,
) {
    if gate_kind == CREDENTIAL_PARKED_GATE_KIND {
        tracing::debug!(
            provider = %sanitize_for_log(provider),
            model = %sanitize_for_log(model),
            gate_kind,
            error = ?gate_err,
            "{message}",
        );
    } else {
        tracing::warn!(
            provider = %sanitize_for_log(provider),
            model = %sanitize_for_log(model),
            gate_kind,
            error = ?gate_err,
            "{message}",
        );
    }
}

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

    /// Admit one client dispatch on `state_key` through the real gate and
    /// return the credential generation the admission captured, or `None`
    /// when the gate refused. Test seam for holding an admitted attempt
    /// across a reload; the attempt's rejection is then settled with
    /// [`Self::settle_credential_rejection_for_tests`].
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn admit_for_credential_tests(&self, state_key: &str) -> Option<u64> {
        let (refusal, mut guard) = self.admit_dispatch(state_key, "test");
        guard.release();
        refusal.is_none().then(|| guard.credential_generation())
    }

    /// Settle a terminal rejection `err` of a client attempt admitted under
    /// `generation`, exactly as a dispatch surface does, returning whether it
    /// parked the seat.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn settle_credential_rejection_for_tests(
        &self,
        state_key: &str,
        err: &Error,
        generation: u64,
    ) -> bool {
        self.park_credential_on_rejection(state_key, err, false, generation)
    }

    /// Lift the credential park on every seat this Router gates and start a
    /// new credential generation on each, returning how many were parked. The
    /// generation advances even on a seat with no park, so an attempt admitted
    /// before this call cannot park its seat after it. Clear-all rather than
    /// per-credential: no credential -> state_key map exists for every secret
    /// kind, and a wrongly cleared park costs one request per seat before it
    /// re-parks.
    pub fn clear_all_credential_parks(&self) -> usize {
        self.state
            .values()
            .filter(|state| state.lock().reset_credential_parks())
            .count()
    }

    /// Park the seat keyed by `state_key` when `err` carries a typed
    /// credential reason and the attempt's admission `generation` is still
    /// current, returning whether it parked. A probe never parks: a minimal
    /// availability request is not evidence about the seat a client will use.
    /// Called beside the breaker settle, never inside its debit gate, because
    /// a credential rejection classifies `Auth`, which does not debit. Leaves
    /// the half-open slot to the caller's probe guard.
    pub(super) fn park_credential_on_rejection(
        &self,
        state_key: &str,
        err: &Error,
        is_probe: bool,
        generation: u64,
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
        let parked = state
            .lock()
            .park_credential_in_generation(Instant::now(), reason, generation);
        if parked {
            tracing::warn!(
                state_key = %sanitize_for_log(state_key),
                reason = reason.as_str(),
                parked_for_secs = crate::runtime_state::CREDENTIAL_PARK_BACKSTOP.as_secs(),
                "seat credential rejected; parking the seat",
            );
        } else {
            tracing::debug!(
                state_key = %sanitize_for_log(state_key),
                reason = reason.as_str(),
                "seat credential rejected before a reload lifted every park; not parking",
            );
        }
        parked
    }

    /// [`Self::park_credential_on_rejection`] for a token-count attempt on
    /// `req` to `target`, admitted through `guard`. A count request is a probe
    /// by the same rule a message request is. Runs outside the breaker's debit
    /// gate like the message surfaces: a credential rejection classifies
    /// `Auth`, which never debits.
    pub(super) fn park_counted_seat(
        &self,
        target: &DispatchTarget,
        req: &ChatRequest,
        err: &Error,
        guard: &ProbeSlotGuard,
    ) -> bool {
        let is_probe = is_probe_request(req, &self.policy_for(&req.model));
        let generation = guard.credential_generation();
        self.park_credential_on_rejection(&target.state_key, err, is_probe, generation)
    }

    /// Settle a success through the seat keyed by `state_key`: record the
    /// breaker success when `record_breaker` is set, and lift the credential
    /// park, since a success proves the credential works. One critical
    /// section, so a concurrent rejection cannot park between the two and
    /// then be lifted by a success that preceded it.
    pub(super) fn settle_seat_success(&self, state_key: &str, record_breaker: bool) {
        let Some(state) = self.state.get(state_key) else {
            return;
        };
        let mut state = state.lock();
        if record_breaker {
            state.record_success(Instant::now());
        }
        #[cfg(test)]
        run_within_success_settle_hook();
        state.clear_credential_park();
    }
}
