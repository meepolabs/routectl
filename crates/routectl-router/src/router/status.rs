//! Route-target status facade for the /status surface.

use std::time::Instant;

use routectl_core::EligibilityReason;

use super::Router;
use crate::resolved::ResolvedModel;
use crate::runtime_state::{CircuitPhase, ProviderGateStatus};
use crate::seat_pool::SeatTarget;
use crate::state_key::StateKey;

/// One dispatch target's read-only health for the status surface: a
/// non-pooled model (one entry keyed by nickname) or a single seat of a
/// pooled model (one entry per seat, keyed by the seat's `state_key`).
/// The wire mapping is owned by the status module downstream; this is an
/// internal read shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteTargetStatus {
    /// Key into the runtime-state map: the bare nickname for a non-pooled
    /// model or the default seat, `"{nickname}#{label}"` for a labeled seat.
    pub state_key: String,
    /// The model entry's `[models]` table key.
    pub nickname: String,
    /// The provider's `[providers]` table key.
    pub provider_name: String,
    /// Wire value of the `model` field this target dispatches to.
    pub upstream: String,
    /// Seat identity: `None` for a non-pooled model, `Some(member)` for one
    /// member seat of a pool-backed model (the member's `[providers]` table
    /// key).
    pub seat_label: Option<String>,
    /// The credential account the target's park and unusable mark are
    /// scoped to (`provider#label` for an OAuth seat, `#entry` for any other
    /// own credential), or `None` for a target forwarding the client's
    /// credential.
    pub account: Option<String>,
    /// The learned lane this target's capability facts key on, taken from the
    /// dispatch target chain expansion builds. `None` only for a provider
    /// entry carrying the reserved separator, which config validation refuses.
    pub learned_lane: Option<StateKey>,
    /// Non-mutating gate health for this target.
    pub gate: ProviderGateStatus,
}

impl Router {
    /// Number of dispatch seats a resolved model expands to: `Some(n)`
    /// for a pooled (multi-seat) model, `None` for a non-pooled
    /// single-target model (a lone seat collapses to the single-target
    /// path). Read-only introspection over the resolved-model table; the
    /// hot-reload coordinator's tests use it to observe that a credentials
    /// reload re-expanded a bare-pool `oauth://provider` into the new seat
    /// count without reaching into the private `resolved_models` map.
    pub fn seat_count_for(&self, nickname: &str) -> Option<usize> {
        self.resolved_models
            .get(nickname)
            .and_then(|m| m.seats.as_ref())
            .map(|seats| seats.len())
    }

    /// Whether `account`, a credential account key (an OAuth seat key such
    /// as `provider#label`), belongs to a pool member this Router kept as a
    /// placeholder seat because its credential could not be read at build.
    /// Such a seat has no provider, so a fresh credential reaches it only
    /// through a rebuild, never through an in-place lift.
    #[must_use]
    pub fn is_placeholder_account(&self, account: &str) -> bool {
        self.placeholder_accounts().any(|held| held == account)
    }

    /// The credential account of every pool member this Router kept as a
    /// placeholder seat, in pool-report order. A forwarded placeholder has no
    /// account and is not listed.
    pub fn placeholder_accounts(&self) -> impl Iterator<Item = &str> {
        self.pool_reports
            .iter()
            .flat_map(|report| &report.placeholders)
            .filter_map(|placeholder| placeholder.account.as_deref())
    }

    /// Whether any dispatch target of this Router, a pool seat or a direct
    /// model, dispatches on `account`. Read from the targets admission and
    /// status use. A placeholder seat is a target too: callers that need a
    /// built one also check [`Self::is_placeholder_account`].
    #[must_use]
    pub(super) fn has_built_target_on_account(&self, account: &str) -> bool {
        self.resolved_models.values().any(|model| {
            self.projection_targets(model)
                .iter()
                .any(|target| target.account.as_deref() == Some(account))
        })
    }

    /// Non-mutating gate health for the state slot keyed by `state_key`.
    /// Fails safe when no slot exists: reports a circuit-Open,
    /// not-dispatchable gate rather than panicking, so a status view of a
    /// target with no runtime state treats it as unavailable. Like
    /// `capacity_snapshot_for`, this must never go through the
    /// `try_dispatch`-based `breaker_open_for` anti-pattern, which would
    /// claim a half-open probe slot just to read.
    fn gate_status_for(
        &self,
        state_key: &str,
        account: Option<&str>,
        now: Instant,
    ) -> ProviderGateStatus {
        let record = self.account_record(account).unwrap_or_default();
        self.state.get(state_key).map_or(
            ProviderGateStatus {
                reason: Some(EligibilityReason::BreakerOpen),
                rpm_available: None,
                circuit: CircuitPhase::Open,
                half_open_probe_in_flight: false,
                circuit_open_elapsed: None,
                last_outcome: None,
                last_outcome_elapsed: None,
                credential_park: None,
                credential_unusable: None,
            },
            |s| {
                s.lock()
                    .gate_status_under(now, record.park(), record.unusable())
            },
        )
    }

    /// [`Self::gate_status_for`] for tests that need the real breaker state
    /// as an oracle. Non-mutating, like the accessor it wraps -- reading it
    /// never claims a half-open slot.
    #[cfg(test)]
    pub(crate) fn gate_status_for_tests(&self, state_key: &str) -> ProviderGateStatus {
        let account = self.account_for_state_key_in_tests(state_key);
        self.gate_status_for(state_key, account.as_deref(), Instant::now())
    }

    /// Read-only health of every dispatch target, for the status surface.
    /// Iterates the resolved-model table and emits one [`RouteTargetStatus`]
    /// per dispatch target: one entry per seat for a pooled (seat-backed)
    /// model, one entry keyed by the nickname for a non-pooled model. The
    /// targets are the ones chain expansion builds, so each entry's
    /// `state_key` and `learned_lane` are dispatch's own. Each entry's gate is read via the `&self`-borrow `gate_status_for`, which
    /// never claims a half-open probe slot; a target with no state slot
    /// fails safe to circuit-Open rather than panicking. A parked account is
    /// reported on every target of the account, one row each.
    pub fn status_targets(&self, now: Instant) -> Vec<RouteTargetStatus> {
        let mut out = Vec::new();
        for model in self.resolved_models.values() {
            for target in self.projection_targets(model) {
                out.push(RouteTargetStatus {
                    gate: self.gate_status_for(&target.state_key, target.account.as_deref(), now),
                    nickname: model.nickname.clone(),
                    seat_label: model.seats.as_ref().map(|_| target.provider_name.clone()),
                    account: target.account.as_deref().map(str::to_string),
                    provider_name: target.provider_name,
                    upstream: target.upstream,
                    learned_lane: target.learned_lane,
                    state_key: target.state_key,
                });
            }
        }
        out
    }

    /// Live seat counts for every pool a selectable model routes at, in
    /// pool-name order, read from the same side-effect-free capacity
    /// snapshot sticky placement reads: no RPM token is charged and no
    /// half-open slot is claimed, however often it is called.
    ///
    /// A pool named by several models has one breaker and RPM bucket per
    /// (model, seat), so a seat counts as eligible when it is dispatchable
    /// for any model routed at the pool; when every one of those models
    /// refuses it, its reason is the highest-priority one they report. A
    /// placeholder seat is never eligible, and a seat with no state slot
    /// reads as breaker-open, as the status view reads it.
    #[must_use]
    pub fn pool_eligibility(&self, now: Instant) -> Vec<PoolEligibility> {
        self.pool_reports
            .iter()
            .filter_map(|report| {
                let models: Vec<&ResolvedModel> = report
                    .models
                    .iter()
                    .filter_map(|nickname| self.resolved_models.get(nickname))
                    .map(AsRef::as_ref)
                    .collect();
                let seats = models.first()?.seats.as_ref()?;
                let mut counts = PoolEligibility {
                    pool: report.pool.clone(),
                    configured: seats.len(),
                    eligible: 0,
                    by_reason: Vec::new(),
                };
                for seat in seats.iter() {
                    match self.seat_ineligibility(seat, &models, now) {
                        None => counts.eligible += 1,
                        Some(reason) => counts.add_reason(reason),
                    }
                }
                Some(counts)
            })
            .collect()
    }

    /// Why no model among `models` can dispatch to `seat`, the
    /// highest-priority reason they report, or `None` when at least one
    /// model would admit it.
    fn seat_ineligibility(
        &self,
        seat: &SeatTarget,
        models: &[&ResolvedModel],
        now: Instant,
    ) -> Option<EligibilityReason> {
        if seat.is_placeholder() {
            return Some(EligibilityReason::CredentialUnusable);
        }
        let mut highest: Option<EligibilityReason> = None;
        for model in models {
            let reason = self
                .capacity_snapshot_for(&seat.state_key_for(&model.nickname), seat.account(), now)
                .map_or(Some(EligibilityReason::BreakerOpen), |snapshot| {
                    snapshot.reason()
                })?;
            if highest.is_none_or(|h| reason.priority() > h.priority()) {
                highest = Some(reason);
            }
        }
        highest
    }
}

/// One pool's live seat counts: how many seats it has, how many a request
/// could dispatch to right now, and why each of the others cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolEligibility {
    /// The `[pools]` table key.
    pub pool: String,
    /// How many seats the pool compiled, built or placeholder.
    pub configured: usize,
    /// How many seats are dispatchable now.
    pub eligible: usize,
    /// The non-eligible seats by reason, highest priority first, nonzero
    /// counts only. Only target-state reasons appear (unusable, parked,
    /// breaker open, rate limited).
    pub by_reason: Vec<(EligibilityReason, usize)>,
}

impl PoolEligibility {
    /// The count of seats not eligible for `reason`.
    #[must_use]
    pub fn count_for(&self, reason: EligibilityReason) -> usize {
        self.by_reason
            .iter()
            .find(|(r, _)| *r == reason)
            .map_or(0, |(_, n)| *n)
    }

    fn add_reason(&mut self, reason: EligibilityReason) {
        match self.by_reason.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, n)) => *n += 1,
            None => {
                self.by_reason.push((reason, 1));
                self.by_reason
                    .sort_by_key(|(r, _)| std::cmp::Reverse(r.priority()));
            }
        }
    }
}

/// Seat counts summed over every pool, for the metrics snapshot line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PoolSeatTotals {
    pub(super) configured: u64,
    pub(super) eligible: u64,
    pub(super) credential_unusable: u64,
    pub(super) account_parked: u64,
    pub(super) breaker_open: u64,
    pub(super) rate_limited: u64,
}

impl PoolSeatTotals {
    pub(super) fn of(pools: &[PoolEligibility]) -> Self {
        let sum = |count: fn(&PoolEligibility) -> usize| -> u64 {
            pools.iter().map(|p| count(p) as u64).sum()
        };
        Self {
            configured: sum(|p| p.configured),
            eligible: sum(|p| p.eligible),
            credential_unusable: sum(|p| p.count_for(EligibilityReason::CredentialUnusable)),
            account_parked: sum(|p| p.count_for(EligibilityReason::AccountParked)),
            breaker_open: sum(|p| p.count_for(EligibilityReason::BreakerOpen)),
            rate_limited: sum(|p| p.count_for(EligibilityReason::RateLimited)),
        }
    }
}
