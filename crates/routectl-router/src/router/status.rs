//! Route-target status facade for the /status surface.

use std::collections::BTreeMap;
use std::time::Instant;

use routectl_auth::SecretRef;
use routectl_core::EligibilityReason;

use super::Router;
use super::learned_lanes::egress_entries;
use super::placement_record::PlacementTotals;
use crate::config::{Config, SeatSelection};
use crate::quota::freshness::ObservationStamp;
use crate::quota::key::seat_key_for_served_identity;
use crate::quota::placement::{Standing, Tier};
use crate::quota::reduce::RejectionReason;
use crate::quota::store::ObserveRefusal;
use crate::quota::usage_body::{BodyError, UsageVendor};
use crate::quota::view::{AcceptedReading, AccountReading, AccountReadings};
use crate::resolved::ResolvedModel;
use crate::runtime_state::{CapacitySnapshot, CircuitPhase, ProviderGateStatus};
use crate::seat_pool::{SeatTarget, credential_account, seat_identity};
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
    /// The login label of the target's `oauth://<provider>#<label>`
    /// credential: `None` for an unlabeled OAuth seat, any other credential
    /// scheme, and a target forwarding the client's credential. Distinct
    /// from `seat_label`, which names the pool member's provider entry.
    pub login_label: Option<String>,
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
                let login_label =
                    oauth_login_label(target_secret_ref(model, &target.provider_name));
                out.push(RouteTargetStatus {
                    gate: self.gate_status_for(&target.state_key, target.account.as_deref(), now),
                    nickname: model.nickname.clone(),
                    seat_label: model.seats.as_ref().map(|_| target.provider_name.clone()),
                    login_label,
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

    /// Every credential account this router's built dispatch targets name,
    /// with its display identity. A model whose provider failed to build has
    /// no target, so this omits its account: surfaces list
    /// [`AccountIdentities::from_config`] instead. Kept as the oracle that the
    /// config walk resolves accounts the way the factory does.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn account_identities(&self) -> AccountIdentities {
        let mut identities = AccountIdentities::default();
        for model in self.resolved_models.values() {
            for target in self.projection_targets(model) {
                let Some(account) = target.account.as_deref() else {
                    continue;
                };
                let login_label =
                    oauth_login_label(target_secret_ref(model, &target.provider_name));
                identities.add(account, &target.provider_name, login_label);
            }
        }
        identities
    }

    /// Land one polled usage body for `account` (a credential account key,
    /// `provider#label` or the bare provider), fetched under the credential
    /// `generation` acquired before its token was read.
    ///
    /// Refused, in order: [`PolledUsageRejection::NotAdmitted`] when the
    /// account is not configured; [`PolledUsageRejection::Superseded`] when
    /// `generation` is no longer the account's current one;
    /// [`PolledUsageRejection::Body`] when `vendor`'s reducer refuses the
    /// body; [`PolledUsageRejection::Rejected`] when the meter refuses the
    /// observation. Only the meter and its counters change: eligibility, the
    /// breaker and every credential record are untouched.
    pub fn observe_polled_usage(
        &self,
        account: &str,
        vendor: UsageVendor,
        body: &[u8],
        generation: u64,
    ) -> Result<AcceptedReading, PolledUsageRejection> {
        let key = seat_key_for_served_identity(account);
        if self.quota_store.check_admission(&key).is_err() {
            return Err(PolledUsageRejection::NotAdmitted);
        }
        let reduced = vendor.reduce(body, ObservationStamp::now());
        // The merge runs under the generation check's lock, so a re-login
        // cannot land between the check and the merge. Every dispatch
        // admission takes that lock, so a refusal is counted (and its WARN
        // written) only after it is released.
        let outcome = self
            .under_current_generation(account, generation, || match &reduced {
                Ok(observation) => self
                    .quota_store
                    .observe_account_uncounted(&key, observation)
                    .map(|()| AcceptedReading::of(observation))
                    .map_err(PolledUsageRejection::from),
                Err(error) => Err(PolledUsageRejection::Body(*error)),
            })
            .ok_or(PolledUsageRejection::Superseded)?;
        match outcome {
            Err(PolledUsageRejection::Body(error)) => {
                self.quota_store.record_rejection(&key, error.reason());
            }
            Err(PolledUsageRejection::Rejected(reason)) => {
                self.quota_store.record_rejection(&key, reason);
            }
            _ => {}
        }
        outcome
    }

    /// Forget every meter reading held for each of `accounts` (credential
    /// account keys), so a reading taken under a replaced login never
    /// describes the login that replaced it. Only the meter changes.
    pub fn forget_account_readings<'a>(&self, accounts: impl IntoIterator<Item = &'a str>) {
        for account in accounts {
            self.quota_store
                .forget(&seat_key_for_served_identity(account));
        }
    }

    /// Every configured account's meter readings as of `now`, with the stale
    /// rule applied per window, plus the meter's refusal totals. Read-only.
    pub fn account_readings(&self, now: &ObservationStamp) -> AccountReadings {
        self.quota_store.readings(now)
    }

    /// The pace rule's counters since start and, for every pool it places on
    /// and each upstream model routed at that pool, how it would rank each
    /// seat for the next new conversation, all as of `now`.
    ///
    /// Ranked by the same scorer, readings and eligibility filter a birth
    /// uses, so the rank-1 seats are exactly the set a birth draws from
    /// before its headroom pick and rotation. Read-only: no tiebreak
    /// advance, no pin read, no RPM token, no half-open slot, no counter.
    ///
    /// A pool the rule places on is a `sticky-least-loaded` pool of more than
    /// one seat. A pool whose seats have no account the meter reads is listed
    /// too, every seat unread, as a birth there ranks on headroom alone.
    /// When several models route one upstream model at a pool, a seat counts
    /// as available when any of them could dispatch to it, as
    /// [`Self::pool_eligibility`] counts it.
    #[must_use]
    pub fn placement_status(&self, now: &ObservationStamp) -> PlacementStatus {
        let readings = self.quota_store.readings(now).accounts;
        let totals = self.metrics.placement.totals();
        let mut placing: BTreeMap<(&str, &str), Vec<&ResolvedModel>> = BTreeMap::new();
        for model in self.resolved_models.values() {
            let places = model.seats.as_ref().is_some_and(|seats| seats.len() > 1)
                && self.config.seat_selection_for(&model.provider_name)
                    == SeatSelection::StickyLeastLoaded;
            if places {
                placing
                    .entry((model.provider_name.as_str(), model.upstream.as_str()))
                    .or_default()
                    .push(model);
            }
        }
        PlacementStatus {
            births: PlacementBirths::of(&totals),
            moves: PlacementMoves::of(&totals),
            cap_forced_moves: totals.cap_forced_moves,
            pools: placing
                .into_iter()
                .map(|((pool, upstream), models)| PoolPlacement {
                    pool: pool.to_string(),
                    model: upstream.to_string(),
                    seats: self.seat_placements(&models, &readings, now),
                })
                .collect(),
        }
    }

    /// Each seat of the pool `models` route at, ranked for their shared
    /// upstream model. `models` is non-empty and every one names the pool.
    fn seat_placements(
        &self,
        models: &[&ResolvedModel],
        readings: &[AccountReading],
        now: &ObservationStamp,
    ) -> Vec<SeatPlacement> {
        let Some(seats) = models.first().and_then(|model| model.seats.as_deref()) else {
            return Vec::new();
        };
        let snapshots: Vec<CapacitySnapshot> = models
            .iter()
            .map(|model| self.gather_capacity_snapshots(seats, &model.nickname, now.monotonic()))
            .reduce(|kept, next| {
                kept.into_iter()
                    .zip(next)
                    .map(|(kept, next)| most_available(kept, next))
                    .collect()
            })
            .unwrap_or_default();
        let standings = super::sticky::seat_standings(seats, &models[0].upstream, readings, now);
        let ranks = dense_ranks(&snapshots, &standings);
        seats
            .iter()
            .zip(standings)
            .zip(snapshots.iter().zip(ranks))
            .map(|((seat, standing), (snapshot, rank))| SeatPlacement {
                account: seat.account().map(str::to_string),
                rank,
                tier: standing.tier.as_str(),
                score: standing.ranked_score().filter(|score| score.is_finite()),
                reason: snapshot
                    .reason()
                    .map_or(standing.reason.as_str(), EligibilityReason::as_str),
            })
            .collect()
    }

    /// Live seat counts for every pool a model routes at, in
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

/// Of one seat's snapshots under two models, the one a birth could use
/// first: dispatchable and closed, then dispatchable, then the refusal of
/// higher priority.
fn most_available(kept: CapacitySnapshot, next: CapacitySnapshot) -> CapacitySnapshot {
    let availability = |snapshot: &CapacitySnapshot| match snapshot.reason() {
        None if snapshot.circuit == CircuitPhase::Closed => (2, 0),
        None => (1, 0),
        Some(reason) => (0, reason.priority()),
    };
    if availability(&next) > availability(&kept) {
        next
    } else {
        kept
    }
}

/// Each seat's dense rank, `None` for a seat no birth could take now.
///
/// Ranked on what a birth's pick ranks on, in its order: the
/// closed-over-half-open preference, then the pace rule's buckets. Seats in
/// one bucket share a rank. A half-open seat set aside for a closed one
/// keeps a rank, below every closed seat.
fn dense_ranks(snapshots: &[CapacitySnapshot], standings: &[Standing]) -> Vec<Option<usize>> {
    let all: Vec<usize> = (0..snapshots.len()).collect();
    let preferred = crate::seat_pool::eligible_candidates(&all, snapshots);
    let set_aside: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&idx| snapshots[idx].is_dispatchable() && !preferred.contains(&idx))
        .collect();
    let mut ranks = vec![None; snapshots.len()];
    let buckets = crate::quota::placement::ordered_buckets(standings, &preferred)
        .into_iter()
        .chain(crate::quota::placement::ordered_buckets(
            standings, &set_aside,
        ));
    for (level, (_, members)) in buckets.enumerate() {
        for idx in members {
            ranks[idx] = Some(level + 1);
        }
    }
    ranks
}

/// The credential reference a target of `model` dispatching through
/// `provider_name` authenticates with: the member seat's own for a pooled
/// model, the model's for a single-target one.
fn target_secret_ref<'m>(model: &'m ResolvedModel, provider_name: &str) -> Option<&'m SecretRef> {
    match model.seats.as_deref() {
        Some(seats) => seats
            .iter()
            .find(|seat| seat.provider_name == provider_name)?
            .auth_secret_ref
            .as_ref(),
        None => model.auth_secret_ref.as_ref(),
    }
}

fn oauth_login_label(secret_ref: Option<&SecretRef>) -> Option<String> {
    match secret_ref {
        Some(SecretRef::OAuth { label, .. }) => label.clone(),
        _ => None,
    }
}

/// One credential account's display identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    /// The `[providers]` entry the account is reported under: for an account
    /// several entries name, the lexicographically smallest of them, so
    /// every surface agrees whatever the model order.
    pub provider_entry: String,
    /// The `<label>` of an `oauth://<provider>#<label>` credential; `None`
    /// for an unlabeled login and any other credential.
    pub login_label: Option<String>,
}

/// Credential accounts keyed by account, each with its display identity.
/// The one place the provider-entry rule lives: every surface that names an
/// account reads its entry and label from here, never from the key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountIdentities(BTreeMap<String, AccountIdentity>);

impl AccountIdentities {
    /// The accounts `config`'s models dispatch on, walked the way the factory
    /// resolves them (a model's own entry, or each configured member of the
    /// pool it names). The configured set, whether or not each provider
    /// built: every surface that lists or names accounts (the status panels,
    /// the usage poll, the CLI report) reads it from here, so an account
    /// whose login is missing is still listed everywhere it is polled.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut identities = Self::default();
        for model in config.models.values() {
            for entry_name in egress_entries(config, &model.provider) {
                let Some(entry) = config.providers.get(entry_name) else {
                    continue;
                };
                let secret_ref = entry
                    .api_key_ref()
                    .and_then(|uri| SecretRef::parse(uri).ok());
                let seat = seat_identity(secret_ref.as_ref());
                let forwarded = entry.forwarded_base_url().is_some();
                if let Some(account) = credential_account(seat.as_deref(), entry_name, forwarded) {
                    identities.add(&account, entry_name, oauth_login_label(secret_ref.as_ref()));
                }
            }
        }
        identities
    }

    fn add(&mut self, account: &str, entry: &str, login_label: Option<String>) {
        match self.0.get_mut(account) {
            Some(kept) => {
                if entry < kept.provider_entry.as_str() {
                    entry.clone_into(&mut kept.provider_entry);
                }
                if kept.login_label.is_none() {
                    kept.login_label = login_label;
                }
            }
            None => {
                self.0.insert(
                    account.to_string(),
                    AccountIdentity {
                        provider_entry: entry.to_string(),
                        login_label,
                    },
                );
            }
        }
    }

    /// `account`'s identity, or `None` for an account not listed.
    #[must_use]
    pub fn get(&self, account: &str) -> Option<&AccountIdentity> {
        self.0.get(account)
    }

    /// Whether `account` is listed.
    #[must_use]
    pub fn contains(&self, account: &str) -> bool {
        self.0.contains_key(account)
    }

    /// Every account with its identity, in account order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AccountIdentity)> {
        self.0
            .iter()
            .map(|(account, identity)| (account.as_str(), identity))
    }
}

/// Why a polled usage body did not reach the account meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolledUsageRejection {
    /// The account is not one the loaded config serves; nothing was read or
    /// stored.
    NotAdmitted,
    /// The account's credential changed after the poll began, so the body
    /// describes a login that is no longer current; nothing was stored.
    Superseded,
    /// The body did not reduce to an observation; counted, nothing stored.
    Body(BodyError),
    /// The meter refused the observation; counted, nothing stored.
    Rejected(RejectionReason),
}

impl From<ObserveRefusal> for PolledUsageRejection {
    fn from(refusal: ObserveRefusal) -> Self {
        match refusal {
            ObserveRefusal::NotAdmitted => Self::NotAdmitted,
            ObserveRefusal::Rejected(reason) => Self::Rejected(reason),
        }
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

/// The pace rule's view for the status surface: its counters since the
/// process started and its ranking of every pool it places on.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacementStatus {
    /// Won births, keyless placements included, by the placed seat's tier.
    pub births: PlacementBirths,
    /// Won moves by the trigger that moved the conversation.
    pub moves: PlacementMoves,
    /// `breaker_open` moves whose old seat stood `low_room` at the move.
    pub cap_forced_moves: u64,
    /// One entry per pool and upstream model, in pool then model order.
    pub pools: Vec<PoolPlacement>,
}

/// Won births by the placed seat's tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementBirths {
    /// Births on a `roomy` seat.
    pub roomy: u64,
    /// Births on a `low_room` seat.
    pub low_room: u64,
    /// Births on an `unread` seat.
    pub unread: u64,
}

impl PlacementBirths {
    fn of(totals: &PlacementTotals) -> Self {
        Self {
            roomy: totals.births_of(Tier::Roomy),
            low_room: totals.births_of(Tier::LowRoom),
            unread: totals.births_of(Tier::Unread),
        }
    }
}

/// Won moves by trigger: the three refusals a move leaves a seat for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementMoves {
    /// Moves off a seat whose credential is unusable.
    pub credential_unusable: u64,
    /// Moves off a parked account.
    pub account_parked: u64,
    /// Moves off an open breaker.
    pub breaker_open: u64,
}

impl PlacementMoves {
    fn of(totals: &PlacementTotals) -> Self {
        Self {
            credential_unusable: totals.moves_of(EligibilityReason::CredentialUnusable),
            account_parked: totals.moves_of(EligibilityReason::AccountParked),
            breaker_open: totals.moves_of(EligibilityReason::BreakerOpen),
        }
    }
}

/// How the pace rule ranks one pool's seats for one upstream model.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolPlacement {
    /// The `[pools]` table key.
    pub pool: String,
    /// The upstream model id the ranking is for.
    pub model: String,
    /// One entry per seat, in the pool's member order.
    pub seats: Vec<SeatPlacement>,
}

/// One seat's place in its pool's ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatPlacement {
    /// The seat's credential account key; `None` only for a member that
    /// forwards the client's credential.
    pub account: Option<String>,
    /// Dense rank from 1 (best), shared by tied seats; `None` while no
    /// request could be dispatched to the seat.
    pub rank: Option<usize>,
    /// Tier token: `roomy`, `low_room` or `unread`.
    pub tier: &'static str,
    /// The score the rank uses, rounded to two decimals; `None` for an
    /// unread seat.
    pub score: Option<f64>,
    /// What decided the seat's place: the eligibility reason token while it
    /// is unavailable, else the standing's reason token (`overage`,
    /// `no_cap`, `no_reading`, `stale_reading`, `low_room`, `pace`,
    /// `unstarted`).
    pub reason: &'static str,
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

#[cfg(test)]
#[path = "status_login_label_tests.rs"]
mod login_label_tests;

#[cfg(test)]
#[path = "account_readings_tests.rs"]
mod account_readings_tests;

#[cfg(test)]
#[path = "account_identity_tests.rs"]
mod account_identity_tests;

#[cfg(test)]
#[path = "placement_status_tests.rs"]
mod placement_status_tests;
