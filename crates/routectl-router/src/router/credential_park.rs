//! Account credential park: the router side of an account whose upstream
//! rejected the credential itself. Parks on a typed reason, lifts on a success
//! through any model on the account, and lifts on a reload that may have
//! changed the account's credential. The
//! per-account record is [`crate::runtime_state::AccountCredential`]; the
//! model gate it feeds is [`crate::runtime_state::ProviderState`].
//!
//! Lock order: the account map (`Router::credential_accounts`) before any
//! `ProviderState` slot. Admission and success settlement hold both, in that
//! order, so a park and a lift are each atomic with the model gate they
//! decide. Every other path takes one lock alone: readers copy the park out
//! and release the map before locking a slot, and the breaker, RPM and
//! half-open release paths never touch the map. Nothing takes the map while
//! holding a slot.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use routectl_core::{EligibilityReason, Error, sanitize_for_log};

use super::Router;
use super::runtime_gate::ProbeSlotGuard;
use crate::runtime_state::{
    AccountCredential, AccountReset, MarkOutcome, RefreshSettlement, SuccessSettlement,
    UnusableCause,
};

/// Every credential account's park and generation, keyed by
/// [`crate::seat_pool::credential_account`]. Shared whole across a hot-reload,
/// so an account that leaves the config keeps its (inert) record until the
/// process restarts; the keyspace is bounded by the accounts ever configured.
pub(super) type CredentialAccounts = BTreeMap<String, AccountCredential>;

/// The `gate_kind` a credential-parked refusal is logged under.
pub(super) const CREDENTIAL_PARKED_GATE_KIND: &str = "credential_parked";

/// The `gate_kind` a credential-unusable refusal is logged under.
pub(super) const CREDENTIAL_UNUSABLE_GATE_KIND: &str = "credential_unusable";

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

/// The refusal a credential-parked account answers with, its wait read against
/// `now`, the admission's own instant. A park that has already lapsed refuses
/// only because its single re-test is in flight, whose outcome decides the
/// recovery, so its wait is unknown.
pub(super) fn parked_seat_error(until: Instant, now: Instant) -> Error {
    Error::LocallyUnavailable {
        reason: EligibilityReason::AccountParked,
        retry_after: (until > now).then(|| until.saturating_duration_since(now)),
    }
}

/// The refusal an account with an unusable credential answers with. No wait
/// is known: only a credential change readmits the account.
pub(super) const fn unusable_seat_error() -> Error {
    Error::LocallyUnavailable {
        reason: EligibilityReason::CredentialUnusable,
        retry_after: None,
    }
}

/// Fold a gate refusal into the chain's running error. Any held error that is
/// not a local refusal (a dialed target's real error) is kept. Between local
/// refusals the reason and the wait fold independently: the reason with the
/// higher [`EligibilityReason::priority`] names the refusal, and the wait is
/// the earliest known one, so a refusal with an unknown wait never erases a
/// known one.
pub(super) fn merge_gate_refusal(last_err: &mut Option<Error>, refusal: Error) {
    match (last_err.as_ref(), refusal) {
        (None, refusal) => *last_err = Some(refusal),
        (
            Some(Error::LocallyUnavailable {
                reason: held_reason,
                retry_after: held_wait,
            }),
            Error::LocallyUnavailable {
                reason,
                retry_after,
            },
        ) => {
            let reason = if reason.priority() > held_reason.priority() {
                reason
            } else {
                *held_reason
            };
            let retry_after = match (*held_wait, retry_after) {
                (Some(held), Some(new)) => Some(held.min(new)),
                (held, new) => held.or(new),
            };
            *last_err = Some(Error::LocallyUnavailable {
                reason,
                retry_after,
            });
        }
        _ => {}
    }
}

/// Log a gate refusal. A credential park or an unusable credential refuses
/// every request until it lifts, and its transition is logged where it is
/// set, so their refusals log at DEBUG; every other gate kind stays a WARN.
pub(super) fn log_gate_refusal(
    provider: &str,
    model: &str,
    gate_kind: &'static str,
    gate_err: &Error,
    message: &'static str,
) {
    if gate_kind == CREDENTIAL_PARKED_GATE_KIND || gate_kind == CREDENTIAL_UNUSABLE_GATE_KIND {
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
    /// The credential account of a target egressing through `provider_name`
    /// with OAuth seat identity `seat`. The forwarded fact is read off the
    /// operator's entry through the accessor chain expansion derives
    /// `DispatchTarget::use_forwarded_credential` from, so every reader
    /// (dispatch, status, sticky placement, background probes) names one
    /// account for one target.
    pub(super) fn credential_account_for(
        &self,
        provider_name: &str,
        seat: Option<&str>,
    ) -> Option<String> {
        let forwarded = self
            .config
            .providers
            .get(provider_name)
            .is_some_and(|entry| entry.forwarded_base_url().is_some());
        crate::seat_pool::credential_account(seat, provider_name, forwarded)
    }

    /// The credential account of a non-pooled target, which carries no
    /// precomputed seat account.
    pub(super) fn direct_target_account(&self, target: &super::DispatchTarget) -> Option<Arc<str>> {
        self.credential_account_for(&target.provider_name, target.seat.as_deref())
            .map(Arc::from)
    }

    /// A copy of `account`'s record, read under the account lock and released
    /// before the caller locks any model slot. `None` for no account or an
    /// account with no record yet.
    pub(super) fn account_record(&self, account: Option<&str>) -> Option<AccountCredential> {
        let account = account?;
        self.credential_accounts.lock().get(account).copied()
    }

    /// Register `account` and return its current credential generation. A
    /// consumer that will act on delayed evidence (a refresh, a credential
    /// read) acquires the generation before gathering it and passes it to
    /// [`Self::mark_credential_unusable`] or
    /// [`Self::credential_refreshed_in_generation`]. The record is created
    /// when absent, so a reset landing between the acquire and the outcome
    /// advances this record and the outcome is stale.
    pub fn acquire_credential_generation(&self, account: &str) -> u64 {
        self.credential_accounts
            .lock()
            .entry(account.to_string())
            .or_default()
            .generation()
    }

    /// Mark `account`'s credential unusable for `cause`. With
    /// `Some(generation)` (from [`Self::acquire_credential_generation`]) the
    /// mark applies only while that credential generation is current, so
    /// evidence gathered before a reload or a successful refresh cannot mark
    /// the newer credential; `None` marks unconditionally. The record is
    /// created when absent, so a later reload has a mark to clear. A re-mark
    /// with the same cause keeps the original `since` and reports
    /// [`MarkOutcome::Unchanged`]; a new or different cause reports
    /// [`MarkOutcome::Transitioned`]. The account then refuses every
    /// admission until a reload resets it or a successful refresh clears an
    /// expired mark.
    pub fn mark_credential_unusable(
        &self,
        account: &str,
        cause: UnusableCause,
        generation: Option<u64>,
    ) -> MarkOutcome {
        self.credential_accounts
            .lock()
            .entry(account.to_string())
            .or_default()
            .mark_unusable(cause, Instant::now(), generation)
    }

    /// Settle a successful refresh of `account`'s credential observed under
    /// `generation`, in one critical section: while `generation` is current
    /// the account starts a new generation, fencing out every outcome still
    /// in flight from the old credential, and an expired mark is cleared; a
    /// mark with another cause stays. A stale `generation` changes nothing.
    pub fn credential_refreshed_in_generation(
        &self,
        account: &str,
        generation: u64,
    ) -> RefreshSettlement {
        self.credential_accounts
            .lock()
            .entry(account.to_string())
            .or_default()
            .refreshed_in_generation(generation)
    }

    /// The credential account of the dispatch target keyed by `state_key`,
    /// resolved through the targets chain expansion builds. Test seams only:
    /// dispatch carries the account on its target.
    #[cfg(any(test, feature = "test-utils"))]
    fn account_for_state_key(&self, state_key: &str) -> Option<String> {
        self.resolved_models
            .values()
            .flat_map(|model| self.projection_targets(model))
            .find(|target| target.state_key == state_key)
            .and_then(|target| target.account.as_deref().map(str::to_string))
    }

    /// [`Self::account_for_state_key`] for the router's other test seams.
    #[cfg(test)]
    pub(super) fn account_for_state_key_in_tests(&self, state_key: &str) -> Option<String> {
        self.account_for_state_key(state_key)
    }

    /// Park the credential account of the target keyed by `state_key` until
    /// `until`, returning `false` when no such target or account exists. Test
    /// seam for the reload and dispatch tests that need a parked (or already
    /// lapsed) account without driving an upstream rejection.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn park_credential_for_tests(
        &self,
        state_key: &str,
        reason: routectl_core::UpstreamReason,
        until: Instant,
    ) -> bool {
        let Some(account) = self.account_for_state_key(state_key) else {
            return false;
        };
        self.credential_accounts
            .lock()
            .entry(account)
            .or_default()
            .park_until(reason, until);
        true
    }

    /// [`Self::mark_credential_unusable`] on the account of the target keyed
    /// by `state_key`, unconditionally, returning `false` when no such target
    /// or account exists. Test seam for dispatch and status tests that need
    /// an unusable seat without producing the evidence that marks one.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn mark_credential_unusable_for_tests(
        &self,
        state_key: &str,
        cause: UnusableCause,
    ) -> bool {
        self.account_for_state_key(state_key)
            .is_some_and(|account| {
                self.mark_credential_unusable(&account, cause, None)
                    .applied()
            })
    }

    /// [`Self::lift_credential_parks`] on the account of the target keyed by
    /// `state_key`, returning `false` when no such target or account exists.
    /// Test seam standing in for the reload that changed the credential.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn reset_credential_for_tests(&self, state_key: &str) -> bool {
        self.account_for_state_key(state_key)
            .is_some_and(|account| {
                self.credential_accounts
                    .lock()
                    .get_mut(&account)
                    .map(AccountCredential::reset)
                    .is_some()
            })
    }

    /// The current credential generation of the account of the target keyed
    /// by `state_key`, read without admitting anything. Test seam.
    #[cfg(test)]
    pub(super) fn credential_generation_for_tests(&self, state_key: &str) -> u64 {
        self.account_for_state_key(state_key)
            .and_then(|account| self.credential_accounts.lock().get(&account).copied())
            .map_or(0, |record| record.generation())
    }

    /// Admit one client dispatch on `state_key` through the real gate and
    /// return the credential generation the admission captured, or `None`
    /// when the gate refused. Test seam for holding an admitted attempt
    /// across a reload; the attempt's rejection is then settled with
    /// [`Self::settle_credential_rejection_for_tests`].
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn admit_for_credential_tests(&self, state_key: &str) -> Option<u64> {
        let account = self.account_for_state_key(state_key);
        let (refusal, mut guard) = self.admit_client(state_key, account.as_deref());
        guard.release();
        refusal.is_none().then(|| guard.credential_generation())
    }

    /// Settle a terminal rejection `err` of a client attempt admitted under
    /// `generation`, exactly as a dispatch surface does, returning whether it
    /// parked the account.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn settle_credential_rejection_for_tests(
        &self,
        state_key: &str,
        err: &Error,
        generation: u64,
    ) -> bool {
        let account = self.account_for_state_key(state_key);
        self.park_credential_on_rejection(account.as_deref(), err, false, generation)
    }

    /// Reset each of `accounts`: lift its credential park, clear its unusable
    /// mark and start a new credential generation, returning one
    /// [`AccountReset`] per account reset, in the order named. The generation
    /// advances even on an account with no park or mark, so an attempt
    /// admitted (or a generation acquired) before this call cannot park or
    /// mark its account after it. An account with no record has neither an
    /// attempt in flight nor an acquired generation, so it is skipped rather
    /// than given one.
    pub fn lift_credential_parks<I>(&self, accounts: I) -> Vec<AccountReset>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut records = self.credential_accounts.lock();
        accounts
            .into_iter()
            .filter_map(|account| {
                let account = account.as_ref();
                records.get_mut(account).map(|record| AccountReset {
                    account: account.to_string(),
                    outcome: record.reset(),
                })
            })
            .collect()
    }

    /// [`Self::lift_credential_parks`] over the OAuth seats among `seats`,
    /// skipping any name shaped like a `[providers]` entry account. A
    /// credentials-file key is not validated against the seat grammar, so a
    /// key starting with the separator would otherwise name an entry account,
    /// whose credential the credentials file does not hold.
    pub fn lift_oauth_credential_parks<I>(&self, seats: I) -> Vec<AccountReset>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        self.lift_credential_parks(
            seats
                .into_iter()
                .filter(|seat| !crate::seat_pool::is_entry_account(seat.as_ref())),
        )
    }

    /// [`Self::lift_credential_parks`] over every account keyed by a
    /// `[providers]` entry rather than an OAuth seat. A config reload
    /// re-resolves those credentials on rebuild; an OAuth seat's credential
    /// lives in the credentials file, whose own reload lifts it.
    pub fn lift_entry_credential_parks(&self) -> Vec<AccountReset> {
        self.credential_accounts
            .lock()
            .iter_mut()
            .filter(|(account, _)| crate::seat_pool::is_entry_account(account))
            .map(|(account, record)| AccountReset {
                account: account.clone(),
                outcome: record.reset(),
            })
            .collect()
    }

    /// Park `account` when `err` carries a typed credential reason and the
    /// attempt's admission `generation` is still current, returning whether
    /// it parked. A probe never parks: a minimal availability request is not
    /// evidence about the account a client will use. No account (a forwarded
    /// credential) never parks. Called beside the breaker settle, never inside
    /// its debit gate, because a credential rejection classifies `Auth`, which
    /// does not debit. Leaves the half-open slot to the caller's probe guard.
    pub(super) fn park_credential_on_rejection(
        &self,
        account: Option<&str>,
        err: &Error,
        is_probe: bool,
        generation: u64,
    ) -> bool {
        if is_probe {
            return false;
        }
        let (Some(reason), Some(account)) = (err.upstream_reason(), account) else {
            return false;
        };
        let parked = self
            .credential_accounts
            .lock()
            .entry(account.to_string())
            .or_default()
            .park_in_generation(Instant::now(), reason, generation);
        if parked {
            tracing::warn!(
                account = %sanitize_for_log(account),
                reason = reason.as_str(),
                parked_for_secs = crate::runtime_state::CREDENTIAL_PARK_BACKSTOP.as_secs(),
                "seat credential rejected; parking the seat",
            );
        } else {
            tracing::debug!(
                account = %sanitize_for_log(account),
                reason = reason.as_str(),
                "seat credential rejected before a reload lifted its park; not parking",
            );
        }
        parked
    }

    /// Settle a success through the model slot keyed by `state_key` on
    /// `account` for the attempt admitted through `guard`: record the breaker
    /// success when `record_breaker` is set, and lift the account's park when
    /// the admission's generation is still current, since a success proves the
    /// credential it was admitted under. One critical section over both locks,
    /// in the module's lock order, so a concurrent rejection cannot park
    /// between the two and then be lifted by a success that preceded it.
    ///
    /// The breaker success applies only under the guard's admission epoch.
    /// Returns the breaker epoch the attempt holds afterwards, read in the
    /// same critical section, for the settlements still to come (a committed
    /// stream's tail).
    pub(super) fn settle_seat_success(
        &self,
        state_key: &str,
        account: Option<&str>,
        record_breaker: bool,
        guard: &ProbeSlotGuard,
    ) -> u64 {
        self.settle_seat_success_in_generation(
            state_key,
            account,
            record_breaker,
            guard.breaker_epoch(),
            guard.credential_generation(),
        )
    }

    /// [`Self::settle_seat_success`] for an attempt admitted under credential
    /// `generation`, settling the breaker as the owner of its current epoch.
    /// Test seam for a success held across a reload.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn settle_seat_success_for_tests(
        &self,
        state_key: &str,
        record_breaker: bool,
        generation: u64,
    ) {
        let account = self.account_for_state_key(state_key);
        let epoch = self
            .state
            .get(state_key)
            .map_or(0, |state| state.lock().epoch());
        self.settle_seat_success_in_generation(
            state_key,
            account.as_deref(),
            record_breaker,
            epoch,
            generation,
        );
    }

    fn settle_seat_success_in_generation(
        &self,
        state_key: &str,
        account: Option<&str>,
        record_breaker: bool,
        breaker_epoch: u64,
        generation: u64,
    ) -> u64 {
        let Some(state) = self.state.get(state_key) else {
            return breaker_epoch;
        };
        let (closed, held_epoch) = {
            let mut accounts = self.credential_accounts.lock();
            let mut state = state.lock();
            let settled = if record_breaker {
                state.record_success_closing(Instant::now(), breaker_epoch)
            } else {
                SuccessSettlement {
                    closed: false,
                    held_epoch: breaker_epoch,
                }
            };
            #[cfg(test)]
            run_within_success_settle_hook();
            if let Some(record) = account.and_then(|account| accounts.get_mut(account)) {
                record.clear_in_generation(generation);
            }
            (settled.closed, settled.held_epoch)
        };
        if closed {
            super::breaker_transition::log_closed(state_key);
        }
        held_epoch
    }
}

#[cfg(test)]
#[path = "credential_park_generation_tests.rs"]
mod credential_park_generation_tests;

#[cfg(test)]
#[path = "credential_account_park_tests.rs"]
mod credential_account_park_tests;

#[cfg(test)]
#[path = "credential_park_lift_tests.rs"]
mod credential_park_lift_tests;

#[cfg(test)]
#[path = "credential_park_slotless_tests.rs"]
mod credential_park_slotless_tests;

#[cfg(test)]
#[path = "credential_park_merge_tests.rs"]
mod merge_tests;

#[cfg(test)]
#[path = "credential_unusable_tests.rs"]
mod credential_unusable_tests;
