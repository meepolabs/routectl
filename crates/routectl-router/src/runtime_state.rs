//! Per-model runtime gates (token-bucket rate limiter + passive circuit
//! breaker) and the per-account credential park.
//!
//! The `Router` holds one `ProviderState` per dispatch target's state key:
//! the bare model nickname for a direct model, `nickname#member` for one seat
//! of a pooled model. Both the rate limiter and the breaker are read and
//! updated under that slot's Mutex, so the chain walk asks "should I dispatch
//! to this model right now?" atomically.
//!
//! A credential park is not per model: it belongs to the credential's account
//! (`seat_pool::credential_account`), held in one router-level map of
//! `AccountCredential` records, so a rejection observed through one model
//! refuses every model on the same account. The model gate takes the
//! account's park as an input, which is how a lapsed park re-tests through
//! each model slot's own half-open claim. The same record carries an
//! unusable mark (a credential known not to work), which refuses before the
//! model gate is consulted and never lapses on its own.

use std::time::{Duration, Instant};

use routectl_core::failure_class::LastOutcome;
use routectl_core::{EligibilityReason, UpstreamReason};

use crate::config::ProviderRuntimePolicy;
use crate::config::tuning::{BreakerThreshold, GateTuning, ResolvedTuning};
const RPM_WINDOW_MS: u64 = 60_000;

/// How long a credential park holds before the half-open path re-tests the
/// account, once per model slot on it. Fixed rather than configurable: a false
/// park costs at most this long plus one request per model on the account. A
/// reload lifts a park early only where it may have changed the credential: a
/// credentials reload lifts the OAuth seats whose stored record changed, and
/// an accepted config reload lifts the non-OAuth account of each
/// `[providers]` entry it changed.
pub const CREDENTIAL_PARK_BACKSTOP: Duration = Duration::from_hours(1);

/// An account parked because its upstream rejected the credential itself, with
/// exact evidence of why. Lives outside the breaker: the breaker's trip and
/// reset paths never clear it, so a transient success or a rate-limit park
/// cannot un-park a dead credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialPark {
    /// Why the upstream rejected the credential.
    pub reason: UpstreamReason,
    /// When the park stops refusing outright and the seat becomes eligible
    /// for a single half-open re-test.
    pub until: Instant,
}

impl CredentialPark {
    pub(crate) const fn refusal(self) -> GateDecision {
        GateDecision::CredentialParked {
            reason: self.reason,
            until: self.until,
        }
    }

    /// Whether the park still refuses outright at `now`, before any model
    /// gate is consulted.
    pub(crate) fn is_live_at(self, now: Instant) -> bool {
        now < self.until
    }
}

/// Why an account's credential is known to be unusable. Diagnostic only: it
/// names the class of evidence, never a token, path or value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnusableCause {
    /// No stored login exists for the seat.
    NotLoggedIn,
    /// The credential source exists but cannot be read or parsed.
    Unreadable,
    /// The access token is past its expiry and could not be refreshed.
    Expired,
    /// The upstream refused the refresh token.
    Revoked,
}

impl UnusableCause {
    /// The stable snake_case token for this cause, for logs and status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "not_logged_in",
            Self::Unreadable => "unreadable",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

/// An account whose credential is known to be unusable, and since when.
///
/// Unlike a [`CredentialPark`] it never lapses and never re-tests: only a
/// reload that may have changed the credential, or a successful refresh of an
/// expired one, readmits the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unusable {
    /// Why the credential is unusable.
    pub cause: UnusableCause,
    /// When the account was first marked with this cause.
    pub since: Instant,
}

/// One credential account's park, unusable mark and generation.
///
/// The generation advances on every [`Self::reset`]. An attempt captures it at
/// admission, and its rejection parks (or its success lifts) only while it
/// still matches, so an attempt admitted before a reload can neither re-park
/// the account nor lift a park set after it. A generation-gated unusable mark
/// is fenced the same way, and a successful refresh also starts a new
/// generation, so no outcome observed under the old credential applies after
/// it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AccountCredential {
    park: Option<CredentialPark>,
    unusable: Option<Unusable>,
    generation: u64,
}

impl AccountCredential {
    /// The account's park, if any.
    pub(crate) const fn park(&self) -> Option<CredentialPark> {
        self.park
    }

    /// The account's unusable mark, if any.
    pub(crate) const fn unusable(&self) -> Option<Unusable> {
        self.unusable
    }

    /// Mark the account unusable for `cause` at `now`. With
    /// `Some(generation)` it applies only while that generation is current;
    /// `None` marks unconditionally. A re-mark with the same cause keeps the
    /// original `since` and reports [`MarkOutcome::Unchanged`]; a different
    /// cause replaces the mark whole and reports a transition.
    pub(crate) fn mark_unusable(
        &mut self,
        cause: UnusableCause,
        now: Instant,
        generation: Option<u64>,
    ) -> MarkOutcome {
        if generation.is_some_and(|generation| generation != self.generation) {
            return MarkOutcome::Stale;
        }
        let (since, outcome) = match self.unusable {
            Some(mark) if mark.cause == cause => (mark.since, MarkOutcome::Unchanged),
            _ => (now, MarkOutcome::Transitioned),
        };
        self.unusable = Some(Unusable { cause, since });
        outcome
    }

    /// Settle a successful refresh of the credential observed under
    /// `generation`. While that generation is current it starts a new one, so
    /// every outcome still in flight from the old credential is fenced out,
    /// and clears an [`UnusableCause::Expired`] mark, which the refresh
    /// disproved. A mark with any other cause came from other evidence and
    /// stays. A stale `generation` changes nothing.
    pub(crate) fn refreshed_in_generation(&mut self, generation: u64) -> RefreshSettlement {
        if generation != self.generation {
            return RefreshSettlement::Stale;
        }
        let cleared_expired = self
            .unusable
            .is_some_and(|mark| mark.cause == UnusableCause::Expired);
        if cleared_expired {
            self.unusable = None;
        }
        self.generation = self.generation.wrapping_add(1);
        RefreshSettlement::Settled {
            cleared_expired,
            generation: self.generation,
        }
    }

    /// The generation an admission captures.
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Park the account until `until`, replacing any earlier park.
    pub(crate) const fn park_until(&mut self, reason: UpstreamReason, until: Instant) {
        self.park = Some(CredentialPark { reason, until });
    }

    /// Park the account for [`CREDENTIAL_PARK_BACKSTOP`] from `now` only when
    /// `generation` is still current, returning whether it parked.
    pub(crate) fn park_in_generation(
        &mut self,
        now: Instant,
        reason: UpstreamReason,
        generation: u64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.park_until(reason, now + CREDENTIAL_PARK_BACKSTOP);
        true
    }

    /// Lift the park only when `generation` is still current, returning
    /// whether it was eligible. A success admitted before a reload proves the
    /// old credential, not the one a rejection under the new generation
    /// parked.
    pub(crate) const fn clear_in_generation(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.park = None;
        true
    }

    /// Lift the park, clear any unusable mark and start a new generation.
    /// Advances even when nothing is parked: the attempt that would park may
    /// still be in flight. Reports what the reset lifted.
    pub(crate) const fn reset(&mut self) -> ResetOutcome {
        let outcome = ResetOutcome {
            park_lifted: self.park.is_some(),
            unusable_cleared: match self.unusable {
                Some(mark) => Some(mark.cause),
                None => None,
            },
        };
        self.park = None;
        self.unusable = None;
        self.generation = self.generation.wrapping_add(1);
        outcome
    }
}

/// What a generation-gated unusable mark did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkOutcome {
    /// The generation was stale; nothing changed.
    Stale,
    /// The account was not unusable for this cause before: it was usable, or
    /// unusable for another cause.
    Transitioned,
    /// The account was already unusable for this cause; `since` is kept.
    Unchanged,
}

impl MarkOutcome {
    /// Whether the mark is in place afterwards.
    pub const fn applied(self) -> bool {
        !matches!(self, Self::Stale)
    }
}

/// What settling a successful credential refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshSettlement {
    /// The observed generation was stale; nothing changed.
    Stale,
    /// The generation advanced to `generation`; `cleared_expired` says
    /// whether an expired mark was cleared.
    Settled {
        /// Whether an [`UnusableCause::Expired`] mark was cleared.
        cleared_expired: bool,
        /// The account's generation after the settlement.
        generation: u64,
    },
}

/// What a credential reset lifted from one account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResetOutcome {
    /// Whether a credential park was lifted.
    pub park_lifted: bool,
    /// The cause of the unusable mark the reset cleared, if any.
    pub unusable_cleared: Option<UnusableCause>,
}

/// One account a credential lift reset, and what the reset lifted from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountReset {
    /// The credential account that was reset.
    pub account: String,
    /// What the reset lifted.
    pub outcome: ResetOutcome,
}

/// Runtime gate for a single model nickname. Created from a config
/// `ProviderRuntimePolicy`; if all knobs are unset, the gate is a
/// transparent no-op.
///
/// Circuit-breaker state machine:
///   Closed --(consecutive_failures hits threshold)--> Open
///   Open --(cooldown elapses, single probe issued)--> HalfOpen-pending
///   HalfOpen-pending --(probe success)--> Closed
///   HalfOpen-pending --(probe failure)--> Open (re-tripped), or Closed
///     when the breaker is disabled
///
/// Only ONE concurrent caller can hold the half-open probe slot. While
/// `half_open_in_flight` is true, other `try_dispatch` callers see
/// `CircuitOpen` even though the cooldown elapsed.
///
/// Every admission captures the breaker `epoch`, and every settlement
/// (success, failure debit, forced open, probe release) carries the epoch its
/// admission captured and applies only while it is still current. The epoch
/// advances on each transition that changes who may settle: closed -> open,
/// the half-open claim, and the claim's settlement to closed or open. A
/// settlement that outlives its state (a stream tail that ends after a newer
/// half-open claim) therefore cannot close, reopen or free the newer one.
#[derive(Debug)]
pub struct ProviderState {
    /// Token bucket: leaky cap of `rpm_limit` requests per 60s window.
    /// `None` means unlimited.
    rpm_capacity: Option<f64>,
    rpm_tokens: f64,
    rpm_last_refill: Instant,

    /// Circuit-breaker config and state. `None` means disabled.
    circuit_failure_threshold: Option<u32>,
    /// Baseline cooldown from config; the window of every normal
    /// failure-driven trip.
    circuit_cooldown: Duration,
    /// The window `force_open` set for the CURRENT open window, `None`
    /// when the window runs on the baseline. Cleared by the next normal
    /// trip and by every close, so a reload that changes the baseline
    /// moves a baseline window and leaves a forced one alone.
    forced_cooldown: Option<Duration>,
    consecutive_failures: u32,
    circuit_opened_at: Option<Instant>,
    /// True once `try_dispatch` has authorized a half-open probe and
    /// before the result is recorded. Forces other concurrent callers
    /// to wait (return CircuitOpen) so the breaker only sees ONE
    /// probe per cooldown cycle.
    half_open_in_flight: bool,
    /// Advances on every breaker transition; see the type docs. Concurrent
    /// closed-state admissions share one epoch, so their debits count toward
    /// the threshold together.
    epoch: u64,

    /// Kind of the most recent settled dispatch outcome, stamped only by
    /// `record_success_closing` / `record_failure_debit` under the
    /// per-target lock.
    /// Never holds `LastOutcome::CircuitOpen` -- gate refusals are derived
    /// from the circuit phase downstream, not stored here.
    last_outcome: Option<LastOutcome>,
    /// Monotonic instant `last_outcome` was stamped, for elapsed-age
    /// exposure via `gate_status`.
    last_outcome_at: Option<Instant>,
}

/// What one failure debit did to the breaker; see
/// [`ProviderState::record_failure_debit`]. Both fields are `false` for a
/// debit whose epoch is no longer current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureDebit {
    /// The debit tripped (or re-tripped) the breaker.
    pub(crate) tripped: bool,
    /// The debit moved a closed or half-open circuit to open.
    pub(crate) opened: bool,
}

/// What one success did to the breaker; see
/// [`ProviderState::record_success_closing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SuccessSettlement {
    /// The success closed a circuit that was open or half-open.
    pub(crate) closed: bool,
    /// The epoch the caller holds afterwards: the post-close epoch when the
    /// success applied, else the stale epoch it was given. A later settlement
    /// of the same dispatch (a committed stream's tail) carries this one.
    pub(crate) held_epoch: u64,
}

/// One gate admission: the decision, whether this call claimed the single
/// half-open probe slot, and the breaker epoch it was admitted under.
///
/// `claimed_half_open` and `epoch` are facts about the call that produced
/// them, not reads of current state -- see [`ProviderState::admit_under_park`]
/// for why the difference matters.
/// `pub(crate)`: the admission token is a router-internal coordination type
/// with no cross-crate consumer, and `runtime_state` is a public module, so a
/// bare `pub` would widen the crate's surface for nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateAdmission {
    /// Whether the gate admitted the dispatch, and if not, why.
    pub(crate) decision: GateDecision,
    /// `true` only when THIS call set the half-open bit and still holds it on
    /// return. A refusal never claims.
    pub(crate) claimed_half_open: bool,
    /// The breaker epoch every settlement of this admission carries; after
    /// a claim, the post-claim epoch. Meaningless on a refusal, which
    /// settles nothing.
    pub(crate) epoch: u64,
}

impl GateAdmission {
    /// A refusal, which by construction claims nothing and settles nothing.
    pub(crate) const fn refused(decision: GateDecision) -> Self {
        Self {
            decision,
            claimed_half_open: false,
            epoch: 0,
        }
    }
}

/// Who is asking the gate for admission.
///
/// The two callers want DIFFERENT answers when the breaker's cooldown has
/// lapsed, so the distinction has to reach the critical section rather than
/// being applied afterwards by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionKind {
    /// A real client request. Takes the half-open recovery attempt when the
    /// cooldown has lapsed, because settling it is how the breaker learns
    /// whether the lane is healthy for clients.
    ClientDispatch,
    /// A background probe. Refuses a half-open-ready lane WITHOUT claiming the
    /// slot and WITHOUT charging an RPM token.
    ///
    /// The refusal must happen before both side effects, not after. A probe
    /// that claimed and handed back would be correct on the breaker bit alone,
    /// but the RPM debit is NOT handed back: the token is spent inside the same
    /// section, so every declined probe would silently consume a slice of the
    /// operator's rate budget for a request it never sent. Deferring first
    /// leaves both the recovery attempt and the rate budget exactly as they
    /// were.
    BackgroundProbe,
}

/// Decision returned from the dispatch gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Provider is healthy and within rate limits; dispatch.
    Allow,
    /// Provider exceeded its RPM cap. Treat as a fallbackable failure.
    RateLimited {
        /// Time until the bucket holds a whole token again; `None` when it
        /// never refills (a zero cap).
        retry_in: Option<Duration>,
    },
    /// Circuit is open from prior failures (or a half-open probe is
    /// already in flight). Treat as a fallbackable failure and skip
    /// this provider until the next cooldown cycle.
    CircuitOpen {
        /// Time left in the open window; `None` once the window lapsed and
        /// the slot's single re-test is in flight or deferred, since its
        /// outcome decides when the slot reopens.
        retry_in: Option<Duration>,
    },
    /// The target's account credential is parked (or this slot's re-test is
    /// already in flight). Treat as a fallbackable failure; the deadline is
    /// the wait an account-parked refusal reports.
    CredentialParked {
        /// Why the upstream rejected the credential.
        reason: UpstreamReason,
        /// When the park lapses into a single half-open re-test.
        until: Instant,
    },
    /// The target's account credential is known to be unusable. Treat as a
    /// fallbackable failure; no wait is known, since only a credential change
    /// readmits the account.
    CredentialUnusable {
        /// Why the credential is unusable.
        cause: UnusableCause,
    },
}

impl ProviderState {
    /// Build a provider's runtime state from its policy, seeding the RPM
    /// token bucket and circuit-breaker thresholds. The policy is the only
    /// configured tier; unset keys take the shipped defaults.
    pub fn new(policy: &ProviderRuntimePolicy) -> Self {
        Self::from_gate(ResolvedTuning::from_provider_runtime(policy).gate())
    }

    /// Build a runtime state from resolved gate tuning.
    pub(crate) fn from_gate(gate: GateTuning) -> Self {
        let now = Instant::now();
        let circuit_failure_threshold = match gate.threshold {
            BreakerThreshold::Disabled => None,
            BreakerThreshold::After(n) => Some(n.get()),
        };
        Self {
            rpm_capacity: gate.rpm_limit.map(f64::from),
            rpm_tokens: gate.rpm_limit.map_or(0.0, f64::from),
            rpm_last_refill: now,
            circuit_failure_threshold,
            circuit_cooldown: gate.cooldown,
            forced_cooldown: None,
            consecutive_failures: 0,
            circuit_opened_at: None,
            half_open_in_flight: false,
            epoch: 0,
            last_outcome: None,
            last_outcome_at: None,
        }
    }

    /// Apply newly resolved gate tuning to a live state without resetting
    /// it: failure counters, the open / half-open phase and the last
    /// outcome carry. A window `force_open` set keeps its own length; only
    /// a window running on the old baseline follows the new one. Disabling
    /// the breaker does not close an open circuit: a live window keeps
    /// refusing until it lapses, and the outcome of the one request the
    /// lapsed window then admits closes it. A bucket that becomes limited
    /// starts full; one that stays limited keeps its level, capped at the
    /// new capacity.
    pub(crate) fn retune(&mut self, gate: GateTuning) {
        self.circuit_failure_threshold = match gate.threshold {
            BreakerThreshold::Disabled => None,
            BreakerThreshold::After(n) => Some(n.get()),
        };
        self.circuit_cooldown = gate.cooldown;
        let new_capacity = gate.rpm_limit.map(f64::from);
        match (self.rpm_capacity, new_capacity) {
            (_, None) => {}
            (None, Some(capacity)) => self.rpm_tokens = capacity,
            (Some(_), Some(capacity)) => self.rpm_tokens = self.rpm_tokens.min(capacity),
        }
        self.rpm_capacity = new_capacity;
    }

    /// Decide whether to dispatch right now, discarding the admission's
    /// claim and epoch. Test convenience: production admits through
    /// [`Self::admit_under_park`] and settles under the epoch it returns.
    #[cfg(test)]
    pub(crate) fn try_dispatch(&mut self, now: Instant) -> GateDecision {
        self.try_dispatch_admitting(now).decision
    }

    /// [`Self::admit_under_park`] for a client dispatch on an unparked
    /// account. Test convenience.
    #[cfg(test)]
    pub(crate) fn try_dispatch_admitting(&mut self, now: Instant) -> GateAdmission {
        self.admit_under_park(now, AdmissionKind::ClientDispatch, None)
    }

    /// Admit a `kind` caller while the model's account carries `park`.
    ///
    /// A live park refuses outright. A lapsed one makes the slot
    /// half-open-ready exactly as a lapsed breaker cooldown does, so each model
    /// slot on a lapsed account admits one re-test of its own, sharing
    /// `half_open_in_flight` with the breaker.
    ///
    /// One critical section returns the claim and the epoch, which is the
    /// whole point: a caller that admits here and then reads
    /// `half_open_probe_in_flight()` or the epoch separately can observe a
    /// claim or transition a DIFFERENT caller made in between, and settle
    /// state it was never admitted under. Ownership and epoch are therefore
    /// reported by the admission itself and carried, never re-derived from
    /// shared state afterwards.
    pub(crate) fn admit_under_park(
        &mut self,
        now: Instant,
        kind: AdmissionKind,
        park: Option<CredentialPark>,
    ) -> GateAdmission {
        if let Some(park) = park
            && (park.is_live_at(now) || self.half_open_in_flight)
        {
            return GateAdmission::refused(park.refusal());
        }
        let retest_ready = match self.circuit_opened_at {
            Some(opened_at) if now.duration_since(opened_at) < self.active_cooldown() => {
                let retry_in = self
                    .active_cooldown()
                    .saturating_sub(now.duration_since(opened_at));
                return GateAdmission::refused(GateDecision::CircuitOpen {
                    retry_in: Some(retry_in),
                });
            }
            Some(_) => true,
            None => park.is_some(),
        };
        if retest_ready {
            // Cooldown or credential park lapsed. Allow exactly one probe
            // through; other callers are refused until the probe records its
            // outcome. We do NOT clear `circuit_opened_at`, the park, or the
            // failure counter here -- only the settle paths mutate that
            // state, so the gate reopens only after a verified outcome.
            if self.half_open_in_flight {
                return GateAdmission::refused(half_open_refusal(park));
            }
            // A BACKGROUND probe stops here, before claiming and before the
            // RPM check below. Returning now is what leaves the recovery
            // attempt available to the next real request AND leaves the rate
            // budget untouched -- the RPM debit below is not refunded on any
            // later path.
            if matches!(kind, AdmissionKind::BackgroundProbe) {
                return GateAdmission::refused(half_open_refusal(park));
            }
        }

        if let Some(capacity) = self.rpm_capacity {
            self.refill_tokens(now, capacity);
            if self.rpm_tokens < 1.0 {
                // Refused before the claim below, so a rate-limited re-test
                // leaves the half-open slot and the epoch as they were.
                return GateAdmission::refused(GateDecision::RateLimited {
                    retry_in: self.rpm_retry_in(now, capacity),
                });
            }
            self.rpm_tokens -= 1.0;
        }

        if retest_ready {
            // Claimed HERE, in the same critical section that reports it. A
            // lapsed credential park claims with the breaker closed; it is a
            // claim all the same, so it starts a new epoch too.
            self.half_open_in_flight = true;
            self.advance_epoch();
        }
        GateAdmission {
            decision: GateDecision::Allow,
            claimed_half_open: retest_ready,
            epoch: self.epoch,
        }
    }

    /// [`Self::record_success_closing`] at the current epoch, as the owner of
    /// whatever the breaker holds. Test convenience: production settles
    /// carry their admission's epoch.
    #[cfg(test)]
    pub(crate) const fn record_success(&mut self, now: Instant) {
        self.record_success_closing(now, self.epoch);
    }

    /// Mark a dispatch admitted under `epoch` as successful: reset the
    /// failure counter and close an open or half-open circuit. A no-op when
    /// `epoch` is no longer current.
    pub(crate) const fn record_success_closing(
        &mut self,
        now: Instant,
        epoch: u64,
    ) -> SuccessSettlement {
        if epoch != self.epoch {
            return SuccessSettlement {
                closed: false,
                held_epoch: epoch,
            };
        }
        let was_open = self.circuit_opened_at.is_some();
        if was_open || self.half_open_in_flight {
            self.advance_epoch();
        }
        self.consecutive_failures = 0;
        self.circuit_opened_at = None;
        self.half_open_in_flight = false;
        self.forced_cooldown = None;
        self.last_outcome = Some(LastOutcome::Ok);
        self.last_outcome_at = Some(now);
        SuccessSettlement {
            closed: was_open,
            held_epoch: self.epoch,
        }
    }

    /// [`Self::record_failure_debit`] at the current epoch, returning
    /// `tripped`. Test convenience: production settles carry their
    /// admission's epoch.
    #[cfg(test)]
    pub(crate) const fn record_failure(&mut self, now: Instant, outcome: LastOutcome) -> bool {
        self.record_failure_debit(now, outcome, self.epoch).tripped
    }

    /// Debit one failure of a dispatch admitted under `epoch`; a no-op when
    /// `epoch` is no longer current. Trips the breaker when consecutive
    /// failures hit the configured threshold. A failed half-open probe
    /// re-trips it with a fresh `circuit_opened_at = now`; with the breaker
    /// disabled, it closes the circuit instead, so an open circuit left
    /// behind by a reload that turned the breaker off (or by `force_open`)
    /// stops gating once its window has been served. The caller supplies
    /// the derived `outcome` (via `LastOutcome::from_failure_class`); it is
    /// stamped regardless of whether the breaker trips.
    ///
    /// `tripped` is also `true` for a debit at or past the threshold on a
    /// circuit that is already open; `opened` is `true` only when the
    /// circuit was closed before the debit, or the debit is the failure of
    /// the half-open probe.
    pub(crate) const fn record_failure_debit(
        &mut self,
        now: Instant,
        outcome: LastOutcome,
        epoch: u64,
    ) -> FailureDebit {
        if epoch != self.epoch {
            return FailureDebit {
                tripped: false,
                opened: false,
            };
        }
        let was_closed = self.circuit_opened_at.is_none();
        let was_half_open_probe = self.half_open_in_flight;
        self.half_open_in_flight = false;
        self.last_outcome = Some(outcome);
        self.last_outcome_at = Some(now);

        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let tripped = match self.circuit_failure_threshold {
            // Re-trip the breaker if a half-open probe failed (regardless of
            // counter), or if the counter just crossed the threshold for
            // the first time.
            Some(threshold) if was_half_open_probe || self.consecutive_failures >= threshold => {
                self.circuit_opened_at = Some(now);
                // A normal failure-driven trip always uses the baseline
                // cooldown, discarding any custom park set by force_open.
                self.forced_cooldown = None;
                true
            }
            // The probe slot is only claimable once the window has lapsed,
            // so a straggler failure inside a live window never lands here.
            None if was_half_open_probe => {
                self.circuit_opened_at = None;
                self.forced_cooldown = None;
                false
            }
            _ => false,
        };
        let opened = tripped && (was_closed || was_half_open_probe);
        if opened || was_half_open_probe {
            self.advance_epoch();
        }
        FailureDebit { tripped, opened }
    }

    /// [`Self::force_open_in_epoch`] at the current epoch. Test convenience:
    /// production settles carry their admission's epoch.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn force_open(&mut self, now: Instant, cooldown: Duration) {
        self.force_open_in_epoch(now, cooldown, self.epoch);
    }

    /// Park the provider immediately for `cooldown`, bypassing the
    /// consecutive-failure threshold, for a dispatch admitted under `epoch`;
    /// a no-op returning `false` when `epoch` is no longer current. Used
    /// when an upstream sent an explicit reset hint (e.g. a rate-limit reset
    /// that is hours away): a single such signal should open the circuit at
    /// once rather than waiting for `circuit_failure_threshold` failures to
    /// accumulate. The custom `cooldown` governs only THIS open window; the
    /// next normal trip or a successful close restores the baseline.
    /// Recovery after the cooldown still flows through the single half-open
    /// probe.
    ///
    /// The caller is responsible for clamping `cooldown` to any
    /// configured ceiling before calling; this honors whatever it is given.
    /// The consecutive-failure counter is left untouched -- the open state
    /// is driven by `circuit_opened_at` + the forced window, independent of
    /// the counter. An applied park frees the half-open slot: at the
    /// current epoch the caller is the slot's owner, if anyone is.
    pub(crate) const fn force_open_in_epoch(
        &mut self,
        now: Instant,
        cooldown: Duration,
        epoch: u64,
    ) -> bool {
        if epoch != self.epoch {
            return false;
        }
        self.circuit_opened_at = Some(now);
        self.forced_cooldown = Some(cooldown);
        self.half_open_in_flight = false;
        self.advance_epoch();
        true
    }

    /// [`Self::release_probe_slot_in_epoch`] at the current epoch. Test
    /// convenience: production releases carry their admission's epoch.
    #[cfg(test)]
    pub(crate) const fn release_probe_slot(&mut self) {
        self.release_probe_slot_in_epoch(self.epoch);
    }

    /// Release a half-open probe slot claimed under `epoch` WITHOUT
    /// recording an outcome; a no-op when `epoch` is no longer current, so a
    /// claim taken since cannot be freed by an earlier holder. Used when the
    /// dispatch path declines to count an attempt as a provider fault (e.g.
    /// a probe fast-fail on a transient 429/529, or a non-fallbackable
    /// client error) but must still free the slot it claimed so the next
    /// probe can proceed after cooldown. Deliberately does NOT touch
    /// `consecutive_failures`, `circuit_opened_at` or the epoch: releasing a
    /// claimed slot is not a failure observation, and the next claim starts
    /// its own epoch.
    pub(crate) const fn release_probe_slot_in_epoch(&mut self, epoch: u64) {
        if epoch == self.epoch {
            self.half_open_in_flight = false;
        }
    }

    /// The current breaker epoch.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    const fn advance_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// True while a half-open probe is in flight for this provider.
    pub const fn half_open_probe_in_flight(&self) -> bool {
        self.half_open_in_flight
    }

    /// The window governing the current open circuit: the forced one when
    /// `force_open` set it, else the baseline.
    const fn active_cooldown(&self) -> Duration {
        match self.forced_cooldown {
            Some(cooldown) => cooldown,
            None => self.circuit_cooldown,
        }
    }

    #[cfg(test)]
    pub(crate) const fn consecutive_failures_for_tests(&self) -> u32 {
        self.consecutive_failures
    }

    fn refill_tokens(&mut self, now: Instant, capacity: f64) {
        if now.duration_since(self.rpm_last_refill).as_millis() == 0 {
            // No measurable time elapsed; leave the anchor untouched
            // (matches the pre-refactor early return).
            return;
        }
        self.rpm_tokens = self.projected_tokens(now, capacity);
        self.rpm_last_refill = now;
    }

    /// Pure, non-mutating projection of the bucket level at `now`. Shared
    /// by `refill_tokens` (which stores the result) and `capacity_snapshot`
    /// (which does not), so the leak arithmetic never diverges. `elapsed`
    /// at or below zero yields the current level unchanged.
    fn projected_tokens(&self, now: Instant, capacity: f64) -> f64 {
        let elapsed_ms = now.duration_since(self.rpm_last_refill).as_millis() as f64;
        if elapsed_ms <= 0.0 {
            return self.rpm_tokens;
        }
        let refill_rate_per_ms = capacity / RPM_WINDOW_MS as f64;
        refill_rate_per_ms
            .mul_add(elapsed_ms, self.rpm_tokens)
            .min(capacity)
    }

    /// Time from `now` until the bucket holds one whole token, rounded up
    /// to the millisecond. `None` for a zero cap, which never refills.
    fn rpm_retry_in(&self, now: Instant, capacity: f64) -> Option<Duration> {
        if capacity <= 0.0 {
            return None;
        }
        let deficit = (1.0 - self.projected_tokens(now, capacity)).max(0.0);
        let wait_ms = (deficit * RPM_WINDOW_MS as f64 / capacity).ceil();
        Some(Duration::from_millis(wait_ms as u64))
    }

    /// [`Self::capacity_snapshot_under`] for an account with no park and no
    /// unusable mark.
    #[cfg(test)]
    pub(crate) fn capacity_snapshot(&self, now: Instant) -> CapacitySnapshot {
        self.capacity_snapshot_under(now, None, None)
    }

    /// Read the gate's capacity WITHOUT mutating it, for a model whose
    /// account carries `park` and `unusable`. Takes `&self`, so the borrow
    /// checker forbids touching the token bucket or the half-open probe
    /// slot; it must never call `try_dispatch` or `refill_tokens`.
    /// Projects the lazily-refilled bucket level at `now` and classifies
    /// the breaker phase, mirroring what `try_dispatch` would observe.
    pub(crate) fn capacity_snapshot_under(
        &self,
        now: Instant,
        park: Option<CredentialPark>,
        unusable: Option<Unusable>,
    ) -> CapacitySnapshot {
        let rpm_available = self
            .rpm_capacity
            .map(|capacity| self.projected_tokens(now, capacity));

        let circuit = match self.circuit_opened_at {
            None => CircuitPhase::Closed,
            Some(opened_at) => {
                let cooldown_elapsed = now.duration_since(opened_at) >= self.active_cooldown();
                if !cooldown_elapsed || self.half_open_in_flight {
                    CircuitPhase::Open
                } else {
                    CircuitPhase::HalfOpenReady
                }
            }
        };
        let parked = park.is_some_and(|park| park.is_live_at(now) || self.half_open_in_flight);
        let credential = if unusable.is_some() {
            CredentialState::Unusable
        } else if parked {
            CredentialState::Parked
        } else {
            CredentialState::Usable
        };

        CapacitySnapshot {
            rpm_available,
            circuit,
            credential,
        }
    }

    /// [`Self::gate_status_under`] for an account with no park and no
    /// unusable mark.
    #[cfg(test)]
    pub(crate) fn gate_status(&self, now: Instant) -> ProviderGateStatus {
        self.gate_status_under(now, None, None)
    }

    /// Read the gate's health for a status view WITHOUT mutating it, for a
    /// model whose account carries `park` and `unusable`. Takes `&self`, so
    /// the borrow checker forbids touching the token bucket or the half-open
    /// probe slot; it must never call `try_dispatch` or `refill_tokens`.
    /// Mirrors [`Self::capacity_snapshot_under`] (same projected bucket
    /// level, same circuit-phase classification) and additionally surfaces
    /// the half-open-probe-in-flight bool explicitly, rather than folding it
    /// into `CircuitPhase::Open` the way the phase classification does.
    pub(crate) fn gate_status_under(
        &self,
        now: Instant,
        park: Option<CredentialPark>,
        unusable: Option<Unusable>,
    ) -> ProviderGateStatus {
        let snapshot = self.capacity_snapshot_under(now, park, unusable);
        ProviderGateStatus {
            reason: snapshot.reason(),
            rpm_available: snapshot.rpm_available,
            circuit: snapshot.circuit,
            half_open_probe_in_flight: self.half_open_in_flight,
            circuit_open_elapsed: self
                .circuit_opened_at
                .map(|opened_at| now.duration_since(opened_at)),
            last_outcome: self.last_outcome,
            last_outcome_elapsed: self.last_outcome_at.map(|at| now.duration_since(at)),
            credential_park: park.map(|park| CredentialParkStatus {
                reason: park.reason,
                remaining: park.until.saturating_duration_since(now),
            }),
            credential_unusable: unusable.map(|mark| CredentialUnusableStatus {
                cause: mark.cause,
                since_elapsed: now.saturating_duration_since(mark.since),
            }),
        }
    }
}

/// The refusal a half-open-ready gate gives a caller that cannot take the
/// re-test: the account's credential park when one is set (its refusal is
/// `account_parked`), else the breaker.
fn half_open_refusal(park: Option<CredentialPark>) -> GateDecision {
    park.map_or(
        GateDecision::CircuitOpen { retry_in: None },
        CredentialPark::refusal,
    )
}

/// Status view of a credential park: the reason and how long until the park
/// lapses into a half-open re-test (zero once it has lapsed). Monotonic, like
/// the other gate ages; the DTO layer converts it to an epoch-ms deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialParkStatus {
    /// Why the upstream rejected the credential.
    pub reason: UpstreamReason,
    /// Time left before the park lapses; zero once lapsed.
    pub remaining: Duration,
}

/// Status view of an unusable mark: the cause and how long ago it was first
/// set. Monotonic; the DTO layer converts it to an epoch-ms instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialUnusableStatus {
    /// Why the credential is unusable.
    pub cause: UnusableCause,
    /// Time since the account was first marked with this cause.
    pub since_elapsed: Duration,
}

/// Non-mutating health view of a `ProviderState` for the status surface.
/// Carries the same projected RPM level and circuit phase as
/// [`CapacitySnapshot`], plus the half-open-probe-in-flight bool surfaced
/// explicitly (the phase classification folds an in-flight probe into
/// `CircuitPhase::Open`; this field exposes it on its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderGateStatus {
    /// Why admission would refuse a dispatch right now, from the same
    /// capacity snapshot placement reads ([`CapacitySnapshot::reason`]);
    /// `None` when the target is dispatchable.
    pub reason: Option<EligibilityReason>,
    /// Projected available RPM tokens right now. `None` when the policy is
    /// unlimited; otherwise the lazily-refilled bucket level computed
    /// without storing it back.
    pub rpm_available: Option<f64>,
    /// Read-only circuit-breaker phase.
    pub circuit: CircuitPhase,
    /// Whether a half-open probe is currently claimed by a live dispatch.
    pub half_open_probe_in_flight: bool,
    /// Elapsed time since the circuit last opened; `None` when the circuit
    /// is closed (never tripped, or closed by a successful probe). Monotonic
    /// -- the DTO layer converts it to an epoch-ms `open_since` downstream.
    pub circuit_open_elapsed: Option<Duration>,
    /// Kind of the most recent settled outcome; `None` when no dispatch has
    /// settled yet (fresh state / post-restart). Never `CircuitOpen` -- that
    /// variant is derived from the circuit phase at DTO-build time.
    pub last_outcome: Option<LastOutcome>,
    /// Elapsed time since `last_outcome` was stamped; `None` when no dispatch
    /// has settled yet.
    pub last_outcome_elapsed: Option<Duration>,
    /// The target's account credential park, if any: `None` when the account
    /// is not parked.
    pub credential_park: Option<CredentialParkStatus>,
    /// The target's account unusable mark, if any: `None` when the
    /// credential is not known to be unusable.
    pub credential_unusable: Option<CredentialUnusableStatus>,
}

/// Read-only classification of the circuit breaker, mirroring the decision
/// `try_dispatch` would make from the same state -- without mutating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitPhase {
    /// `circuit_opened_at == None`. `try_dispatch` would fall through the
    /// breaker and (RPM permitting) return `Allow`.
    Closed,
    /// Breaker is currently blocking: still within cooldown
    /// (`now - opened_at < active_cooldown`), OR cooldown elapsed but a
    /// half-open probe is already in flight. `try_dispatch` would return
    /// `CircuitOpen`.
    Open,
    /// Cooldown elapsed AND no probe is in flight: the next `try_dispatch`
    /// would claim the half-open probe slot and (RPM permitting) return
    /// `Allow`.
    HalfOpenReady,
}

/// Whether a target's account credential would let a dispatch through, as a
/// snapshot sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// Neither an unusable mark nor a live park refuses the dispatch.
    Usable,
    /// The account carries an unusable mark.
    Unusable,
    /// The account's park has not lapsed, or its single re-test is already
    /// in flight.
    Parked,
}

/// Non-mutating view of a `ProviderState`'s capacity at a given instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacitySnapshot {
    /// Projected available RPM tokens right now. `None` when the policy is
    /// unlimited; otherwise the lazily-refilled bucket level computed
    /// without storing it back.
    pub rpm_available: Option<f64>,
    /// Read-only circuit-breaker phase.
    pub circuit: CircuitPhase,
    /// Whether the account's credential would refuse the dispatch, and why.
    pub credential: CredentialState,
}

impl CapacitySnapshot {
    /// The reason admission would refuse a dispatch from this state, by
    /// priority: an unusable credential, a parked account, an open breaker
    /// (an in-flight half-open re-test reads as open), then an empty RPM
    /// bucket. `None` when nothing refuses. A snapshot yields only these
    /// target-state reasons, never a request-only one.
    pub fn reason(&self) -> Option<EligibilityReason> {
        match self.credential {
            CredentialState::Unusable => return Some(EligibilityReason::CredentialUnusable),
            CredentialState::Parked => return Some(EligibilityReason::AccountParked),
            CredentialState::Usable => {}
        }
        if self.circuit == CircuitPhase::Open {
            return Some(EligibilityReason::BreakerOpen);
        }
        match self.rpm_available {
            Some(tokens) if tokens < 1.0 => Some(EligibilityReason::RateLimited),
            _ => None,
        }
    }

    /// Predict whether admission would return `Allow` from this state,
    /// without mutating anything: [`Self::reason`] names no refusal.
    pub fn is_dispatchable(&self) -> bool {
        self.reason().is_none()
    }
}

#[cfg(test)]
#[path = "runtime_state_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "runtime_state_credential_park_tests.rs"]
mod credential_park_tests;

#[cfg(test)]
#[path = "runtime_state_default_breaker_tests.rs"]
mod default_breaker_tests;

#[cfg(test)]
#[path = "runtime_state_epoch_tests.rs"]
mod epoch_tests;

#[cfg(test)]
#[path = "runtime_state_retry_in_tests.rs"]
mod retry_in_tests;

#[cfg(test)]
#[path = "runtime_state_unusable_tests.rs"]
mod unusable_tests;
