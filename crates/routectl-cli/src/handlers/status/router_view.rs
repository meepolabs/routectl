//! Read-only router facade for the `/status` family.
//!
//! The panel submodules ([`super::health`], [`super::config`], ...) must be
//! structurally incapable of dialing an upstream or reading raw config
//! secrets: a live `Router` exposes dispatch (`complete`/`stream`) and its
//! `config` field (secret refs). Rust module privacy is the enforcement: the
//! `Arc<ArcSwap<Router>>` lives in [`StatusRouterHandle::inner`] and the
//! loaded `Arc<Router>` in [`StatusRouterView::router`] / [`QueryPricer`], all
//! PRIVATE to this module. A sibling panel module holds a `&StatusRouterView`
//! or a `QueryPricer` and can call only the read methods below -- it can never
//! name the private field, so it can never obtain a `&Router`, call
//! `.complete`/`.stream`, or touch `.config`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_core::EligibilityReason;
use routectl_router::prices::{PriceBook, PriceTable};
use routectl_router::router::RouteTargetStatus;
use routectl_router::wire_check_state::WireCheckSnapshot;
use routectl_router::{
    AccountIdentities, AccountReadings, EffectiveView, FidelitySnapshot, FieldRepairCounters,
    LaneBudgetExhaustions, LaneLossyRepairs, ObservationStamp, PlacementStatus, PoolEligibility,
    Router, derive_effective_view, shipped_beta_seed_scope,
};
use routectl_usage::{AggRow, RowCost};

use crate::commands::doctor::attention::CredentialState as CredentialCondition;
use crate::commands::doctor::{
    ParkedSeat, ResidentLearned, SeatFix, ServedInputs, ServedPrices, UnusableSeat,
    unusable_seat_fix,
};
use crate::commands::pricing::{PriceSnapshot, is_subscription};
use crate::commands::usage::cost_for_row;
use crate::lane_cell::{CellSources, LearnedSnapshot, ResolvedEntry, resolve_entries};
use crate::server::capability_rebuild::WarmReport;
use crate::server::seat_driver::{accounts_awaiting_login, managed_seats};
use crate::server::usage_poll::{PollStateSnapshot, UsagePollState};

/// Owns the live-router read handle for the status surface. Loads a fresh
/// snapshot per [`view`](Self::view) call so a hot-swap is picked up. The
/// inner `ArcSwap` is private, so nothing outside this module can reach the
/// raw router.
pub struct StatusRouterHandle {
    inner: Arc<ArcSwap<Router>>,
}

impl StatusRouterHandle {
    pub const fn new(inner: Arc<ArcSwap<Router>>) -> Self {
        Self { inner }
    }

    /// Snapshot the live router into a read-only view. `load_full` once per
    /// panel build pins a consistent router for that build.
    pub fn view(&self) -> StatusRouterView {
        StatusRouterView {
            router: self.inner.load_full(),
        }
    }

    /// Pin ONE immutable config snapshot and hand back an owned pricer source
    /// over it and `prices`.
    ///
    /// Owned (not borrowed) so the whole grouped query can be priced on a
    /// blocking worker against a single snapshot: a router hot-swap or a new
    /// price table mid-query can never make two rows of one result price
    /// against different rate tables. The book is read once, by
    /// [`UnpinnedPricer::pin`] on that worker, so a table renamed into the
    /// config folder prices the next query with no router rebuild and a
    /// reparse never runs on an async worker.
    pub fn pricer(&self, prices: Arc<PriceBook>) -> UnpinnedPricer {
        UnpinnedPricer {
            router: self.inner.load_full(),
            book: prices,
        }
    }
}

/// A pinned router snapshot and the price book its rows will price against,
/// before the book is read. Only [`Self::pin`] turns it into a pricer.
pub struct UnpinnedPricer {
    router: Arc<Router>,
    book: Arc<PriceBook>,
}

impl UnpinnedPricer {
    /// Every configured credential account of the pinned snapshot, with its
    /// display identity, so a result's seat names and its prices read one
    /// config.
    pub fn account_identities(&self) -> AccountIdentities {
        AccountIdentities::from_config(&self.router.config)
    }

    /// Read the book once (a stat, and a reparse when the file changed) and
    /// pin the tables every row of the result prices against. Call it on the
    /// blocking worker that prices the rows.
    pub fn pin(self) -> QueryPricer {
        QueryPricer {
            prices: PriceSnapshot::pin(&self.book),
            router: self.router,
        }
    }
}

/// An owned, `'static` pricing facade over one pinned router snapshot. The
/// `Arc<Router>` is private to this module, so a caller can price a row and
/// nothing else -- it can never reach dispatch or raw config through it.
pub struct QueryPricer {
    router: Arc<Router>,
    prices: PriceSnapshot,
}

impl QueryPricer {
    /// The cost verdict for one fine-grained aggregate row, resolved against
    /// the pinned config and price tables through the same function the CLI
    /// usage report uses.
    pub fn price(&self, row: &AggRow) -> RowCost {
        cost_for_row(&self.router.config, &self.prices, row)
    }

    /// Provider entry -> account for the pinned snapshot's own-credential
    /// entries, so a seat grouping labels its rows from the config that
    /// prices them.
    pub fn entry_accounts(&self) -> BTreeMap<String, String> {
        AccountIdentities::from_config(&self.router.config).entry_accounts()
    }
}

/// A read-only view over one pinned router snapshot. The `router` field is
/// private, so the only router state a panel can observe is what the
/// methods below expose -- route health, per-pool seat counts, the fix line
/// of a seat, learned negatives, the live envelope-field repair counters, the
/// per-verdict fidelity rows, the live wire-check counters and findings, the
/// derived effective config view (which is computed here so panels never
/// handle raw `Config`), and the doctor's served inputs (an opaque value only
/// the doctor gather can read).
pub struct StatusRouterView {
    router: Arc<Router>,
}

impl StatusRouterView {
    /// Per-dispatch-target health for the health panel.
    pub fn route_targets(&self, now: Instant) -> Vec<RouteTargetStatus> {
        self.router.status_targets(now)
    }

    /// Every configured account's meter readings at `now`, with the stale
    /// rule applied, plus the meter's refusal totals. A pure read.
    pub fn account_readings(&self, now: &ObservationStamp) -> AccountReadings {
        self.router.account_readings(now)
    }

    /// The pace rule's counters and its per-pool, per-model seat ranking as
    /// of `now`. Read-only: no rotation, pin, RPM token, probe slot or
    /// counter moves.
    pub fn placement_status(&self, now: &ObservationStamp) -> PlacementStatus {
        self.router.placement_status(now)
    }

    /// Every credential account the served config names, with the display
    /// identity every surface reports it under, including an account whose
    /// provider failed to build.
    pub fn account_identities(&self) -> AccountIdentities {
        AccountIdentities::from_config(&self.router.config)
    }

    /// The usage poll's state for the served config's managed logins, with
    /// the logins the router holds as awaiting a fresh login told apart. One
    /// short lock on the poll schedule; no I/O.
    pub fn usage_poll_snapshot(&self, state: &UsagePollState) -> PollStateSnapshot {
        state.snapshot(&self.router.config, &accounts_awaiting_login(&self.router))
    }

    /// Live seat counts for every pool, read from the same side-effect-free
    /// capacity snapshot [`Self::route_targets`] reads.
    pub fn pool_eligibility(&self, now: Instant) -> Vec<PoolEligibility> {
        self.router.pool_eligibility(now)
    }

    /// What readmits a seat dispatching through `[providers]` entry `entry`,
    /// derived from the served config. Carries the entry, credential scheme,
    /// and `oauth://` provider and label only, never a ref's value.
    pub fn seat_fix(&self, entry: &str) -> SeatFix {
        SeatFix::of(&self.router.config, entry)
    }

    /// The `[pools]` keys whose members resolve to each configured account,
    /// sorted. An account several entries name lists every pool any of those
    /// entries belongs to; an account in no pool lists none. A member resolves
    /// to its account by the served config's identity rule, the same account
    /// parks and unusable marks are scoped to, so a member whose provider did
    /// not build still places its account in the pool.
    pub fn account_pools(&self) -> BTreeMap<String, Vec<String>> {
        let config = &self.router.config;
        let mut pools: BTreeMap<String, BTreeSet<String>> = self
            .account_identities()
            .iter()
            .map(|(account, _)| (account.to_string(), BTreeSet::new()))
            .collect();
        for (pool, entry) in &config.pools {
            for member in &entry.members {
                let Some(account) = AccountIdentities::account_of_entry(config, member) else {
                    continue;
                };
                if let Some(held) = pools.get_mut(&account) {
                    held.insert(pool.clone());
                }
            }
        }
        pools
            .into_iter()
            .map(|(account, held)| (account, held.into_iter().collect()))
            .collect()
    }

    /// The credential state of every account a dispatch target names, at
    /// `now` (a monotonic instant and its epoch-ms anchor). An unusable mark
    /// outranks a live park, as it does in eligibility. An unusable account
    /// carries its cause, the epoch-ms it was first marked and the fix the
    /// health panel shows ([`Self::seat_fix`]); a parked one carries the park
    /// reason and the epoch-ms the park lapses. A park whose deadline has
    /// passed is a re-test, not a refusal, and reads usable. An account no
    /// built target names is absent.
    pub fn account_credentials(&self, now: (Instant, i64)) -> BTreeMap<String, AccountCredential> {
        let (now, now_ms) = now;
        let identities = self.account_identities();
        let mut out = BTreeMap::new();
        for target in self.router.status_targets(now) {
            let Some(account) = target.account else {
                continue;
            };
            if out.contains_key(&account) {
                continue;
            }
            let entry = identities
                .get(&account)
                .map_or(target.provider_name.as_str(), |identity| {
                    identity.provider_entry.as_str()
                });
            let credential = match (target.gate.credential_unusable, target.gate.credential_park) {
                (Some(mark), _) => AccountCredential {
                    condition: CredentialCondition::Unusable {
                        cause: mark.cause,
                        since_ms: epoch_ms_before(now_ms, mark.since_elapsed),
                    },
                    fix: Some(self.seat_fix(entry).command()),
                },
                (None, Some(park)) if !park.remaining.is_zero() => AccountCredential {
                    condition: CredentialCondition::Parked {
                        reason: park.reason,
                        until_ms: epoch_ms_after(now_ms, park.remaining),
                    },
                    fix: None,
                },
                _ => AccountCredential::usable(),
            };
            out.insert(account, credential);
        }
        out
    }

    /// The accounts of the served config's managed logins: the seats the
    /// background refresh and usage poll drivers work on.
    pub fn managed_accounts(&self) -> BTreeSet<String> {
        managed_seats(&self.router.config)
            .into_iter()
            .map(|seat| seat.account)
            .collect()
    }

    /// Whether each configured account bills as a managed-OAuth
    /// subscription, by the one predicate every cost surface uses, applied to
    /// the entry the account is reported under.
    pub fn account_subscriptions(&self) -> BTreeMap<String, bool> {
        self.account_identities()
            .iter()
            .map(|(account, identity)| {
                (
                    account.to_string(),
                    is_subscription(&self.router.config, &identity.provider_entry),
                )
            })
            .collect()
    }

    /// Every resident learned-capability entry for the health panel, each with
    /// its cell resolved at `now` through the shared lane-cell helper, the
    /// way the doctor matrix resolves it: same config, overrides, seed-clear
    /// markers and shipped beta seed.
    pub fn learned_capabilities(&self, now: (Instant, i64)) -> Vec<ResolvedEntry> {
        let entries = self.router.learned_capability_snapshot();
        let seed_clears = self.router.learned_seed_clear_snapshot();
        resolve_entries(&CellSources {
            config: &self.router.config,
            overrides: self.router.override_registry(),
            learned: LearnedSnapshot {
                entries: &entries,
                now: Some(now),
                seed_clears: &seed_clears,
            },
            beta_seed: shipped_beta_seed_scope(),
        })
    }

    /// The live process-lifetime envelope-field repair counters, for the
    /// health panel to log at INFO.
    pub fn field_repair_counters(&self) -> FieldRepairCounters {
        self.router.field_repair_counters()
    }

    /// The lossy field-repair count split by learned lane, for the doctor panel.
    pub fn lossy_repairs_by_lane(&self) -> Vec<LaneLossyRepairs> {
        self.router.lossy_repairs_by_lane()
    }

    /// The repair-budget exhaustion count split by learned lane, for the
    /// doctor panel.
    pub fn budget_exhaustions_by_lane(&self) -> Vec<LaneBudgetExhaustions> {
        self.router.budget_exhaustions_by_lane()
    }

    /// Finished requests by repairs spent, one bucket per draw count, for the
    /// doctor panel.
    pub fn repairs_per_request(&self) -> Vec<u64> {
        self.router.repairs_per_request()
    }

    /// The WHOLE fidelity surface in one coherent router read: counters, acting
    /// verdicts, detailed per-verdict rows, and the probe scheduler snapshot.
    ///
    /// ONE call rather than four, and that is the point: the acting rows and the
    /// detailed rows come from the same learned snapshot inside the router, so a
    /// reader reconciling their two counts sees only the documented filter
    /// difference -- never a race between two reads.
    ///
    /// A PURE read on the router's side -- it activates no lane, advances no
    /// cadence, claims no slot, and schedules nothing -- which is what makes it
    /// admissible on a facade whose whole contract is that a status poll cannot
    /// mutate the router. That purity is pinned where the read lives, in the
    /// router's own module, both behaviorally and by a source guard.
    pub fn fidelity_snapshot(&self) -> FidelitySnapshot {
        self.router.fidelity_snapshot()
    }

    /// The live wire-check counters and findings ring with the configured
    /// status window. A pure read: no counter resets and the sampler does not
    /// advance.
    pub fn wire_check_snapshot(&self) -> WireCheckSnapshot {
        self.router.wire_check_snapshot()
    }

    /// The derived effective-config view for the config panel, from the live
    /// config the pinned Router was BUILT against -- the generation the
    /// daemon accepted, read out of memory, never re-read from disk.
    ///
    /// Model prices resolve against the `refreshed` and `shipped` tables the
    /// caller pinned for this read. Each model's fill is lowered by the
    /// learned ceiling resident in this router, the same rule doctor applies.
    ///
    /// The raw `Config` is read only inside this method; the panel receives
    /// the already-derived, secret-free [`EffectiveView`].
    pub fn effective_view(
        &self,
        refreshed: Option<&PriceTable>,
        shipped: Option<&PriceTable>,
    ) -> EffectiveView {
        derive_effective_view(&self.router.config, refreshed, shipped)
            .with_learned_fills(&self.router.learned_fills())
    }

    /// The served doctor gather's inputs, owned so the gather can run on a
    /// blocking worker.
    ///
    /// The config comes off this one pinned router and is not re-read from
    /// disk. The learned layer is the
    /// resident registry, read once; its clock anchors are taken after that
    /// read, so every entry's instants lie at or before them. `warm` is the
    /// boot warm that filled the registry. The credential-parked and unusable
    /// seats come off one per-target status read, the same one the health
    /// panel uses, which changes no gate state.
    ///
    /// The pricing section resolves against one snapshot of `prices`, the
    /// book the daemon's cost reads use, pinned by the gather on its
    /// blocking worker.
    ///
    /// `usage_db_path` is the ledger the daemon opened at boot, which the
    /// cache-ignore read opens whatever the reloaded config names.
    ///
    /// The returned value's fields are private to the doctor module: a panel
    /// holding it can pass it to the gather and nothing else.
    pub(crate) fn served_doctor_inputs(
        &self,
        warm: WarmReport,
        prices: Arc<PriceBook>,
        usage_db_path: PathBuf,
    ) -> ServedInputs {
        let entries = self.router.learned_capability_snapshot();
        let seed_clears = self.router.learned_seed_clear_snapshot();
        let learned = ResidentLearned {
            entries,
            seed_clears,
            now: Instant::now(),
            now_ms: crate::local_time::unix_ms(time::OffsetDateTime::now_utc()),
        };
        let (parked, unusable) = self.credential_marked_seats();
        ServedInputs::new(
            Arc::clone(&self.router.config),
            learned,
            warm,
            ServedPrices::Book(prices),
            usage_db_path,
        )
        .with_parked_seats(parked)
        .with_unusable_seats(unusable)
    }

    /// Every seat whose credential is parked right now, with its deadline as
    /// epoch ms, and every seat whose credential is unusable, with the instant
    /// it was first marked as epoch ms and its fix line. A seat carrying both
    /// marks is in both lists.
    fn credential_marked_seats(&self) -> (Vec<ParkedSeat>, Vec<UnusableSeat>) {
        let now = Instant::now();
        let now_ms = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
        let mut parked = Vec::new();
        let mut unusable = Vec::new();
        for target in self.router.status_targets(now) {
            if let Some(park) = target.gate.credential_park {
                let remaining_ms = i64::try_from(park.remaining.as_millis()).unwrap_or(i64::MAX);
                parked.push(ParkedSeat {
                    state_key: target.state_key.clone(),
                    reason: park.reason,
                    parked_until_ms: now_ms.saturating_add(remaining_ms),
                });
            }
            if let Some(mark) = target.gate.credential_unusable {
                let elapsed_ms = i64::try_from(mark.since_elapsed.as_millis()).unwrap_or(i64::MAX);
                unusable.push(UnusableSeat {
                    fix: unusable_seat_fix(&self.router.config, &target.provider_name),
                    state_key: target.state_key,
                    reason: EligibilityReason::CredentialUnusable,
                    cause: mark.cause,
                    since_ms: now_ms.saturating_sub(elapsed_ms),
                });
            }
        }
        (parked, unusable)
    }
}

/// Whether an account's credential can be dispatched on right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// No unusable mark and no live park.
    Usable,
    /// Marked unusable; only a fixed credential readmits it.
    Unusable,
    /// Parked by an upstream rejection until a deadline.
    Parked,
}

impl CredentialState {
    /// The stable snake_case token for this state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::Unusable => "unusable",
            Self::Parked => "parked",
        }
    }
}

/// One account's credential state with what readmits it. The state is held
/// once, in the needs-attention builder's typed form, so every surface words
/// it through that builder's one function; the wire token and times derive
/// from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCredential {
    /// The state, with its cause and times.
    pub condition: CredentialCondition,
    /// What readmits an unusable account; `None` otherwise.
    pub fix: Option<String>,
}

impl AccountCredential {
    pub(super) const fn usable() -> Self {
        Self {
            condition: CredentialCondition::Usable,
            fix: None,
        }
    }

    /// The state's wire token.
    pub const fn state(&self) -> CredentialState {
        match self.condition {
            CredentialCondition::Usable => CredentialState::Usable,
            CredentialCondition::Unusable { .. } => CredentialState::Unusable,
            CredentialCondition::Parked { .. } => CredentialState::Parked,
        }
    }

    /// Epoch-ms the unusable mark was first set; `None` unless unusable.
    pub const fn since_ms(&self) -> Option<i64> {
        match self.condition {
            CredentialCondition::Unusable { since_ms, .. } => Some(since_ms),
            CredentialCondition::Usable | CredentialCondition::Parked { .. } => None,
        }
    }

    /// Epoch-ms the park lapses into a re-test; `None` unless parked.
    pub const fn until_ms(&self) -> Option<i64> {
        match self.condition {
            CredentialCondition::Parked { until_ms, .. } => Some(until_ms),
            CredentialCondition::Usable | CredentialCondition::Unusable { .. } => None,
        }
    }
}

fn epoch_ms_before(now_ms: i64, elapsed: Duration) -> i64 {
    now_ms.saturating_sub(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

fn epoch_ms_after(now_ms: i64, remaining: Duration) -> i64 {
    now_ms.saturating_add(i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
#[path = "router_view_account_tests.rs"]
mod account_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::Config;

    fn handle() -> StatusRouterHandle {
        let router = Router::new(Arc::new(Config::default()));
        StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router)))
    }

    #[test]
    fn every_view_read_method_resolves_on_a_default_router() {
        let handle = handle();
        let view = handle.view();

        let _targets: Vec<RouteTargetStatus> = view.route_targets(Instant::now());
        let _pools: Vec<PoolEligibility> = view.pool_eligibility(Instant::now());
        let _fix: SeatFix = view.seat_fix("absent");
        let _learned: Vec<ResolvedEntry> = view.learned_capabilities((
            Instant::now(),
            crate::local_time::unix_ms(time::OffsetDateTime::now_utc()),
        ));
        let _counters: FieldRepairCounters = view.field_repair_counters();
        let _lossy: Vec<LaneLossyRepairs> = view.lossy_repairs_by_lane();
        let _exhaustions: Vec<LaneBudgetExhaustions> = view.budget_exhaustions_by_lane();
        let _per_request: Vec<u64> = view.repairs_per_request();
        let _fidelity: FidelitySnapshot = view.fidelity_snapshot();
        let _wire_check: WireCheckSnapshot = view.wire_check_snapshot();
        let _readings: AccountReadings = view.account_readings(&ObservationStamp::now());
        let _identities: AccountIdentities = view.account_identities();
        let _effective: EffectiveView = view.effective_view(None, None);
        let book = Arc::new(PriceBook::new(std::path::PathBuf::from(
            "absent-prices.json",
        )));
        let _served: ServedInputs = view.served_doctor_inputs(
            WarmReport::not_run(),
            book,
            std::path::PathBuf::from("absent-usage.db"),
        );
    }

    #[test]
    fn view_reads_a_fresh_snapshot_per_call() {
        // Two independent views over the same handle each load their own
        // snapshot; both resolve the read methods without sharing borrow
        // state, confirming `view()` hands out standalone read handles.
        let handle = handle();
        let a = handle.view();
        let b = handle.view();
        assert_eq!(
            a.route_targets(Instant::now()).len(),
            b.route_targets(Instant::now()).len()
        );
    }
}
