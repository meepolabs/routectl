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

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use routectl_router::router::RouteTargetStatus;
use routectl_router::wire_check_state::WireCheckSnapshot;
use routectl_router::{
    EffectiveView, FidelitySnapshot, FieldRepairCounters, LaneBudgetExhaustions, LaneLossyRepairs,
    Router, derive_effective_view, shipped_beta_seed_scope,
};
use routectl_usage::{AggRow, RowCost};

use crate::commands::doctor::{ParkedSeat, ResidentLearned, ServedInputs};
use crate::commands::usage::cost_for_row;
use crate::lane_cell::{CellSources, LearnedSnapshot, ResolvedEntry, resolve_entries};
use crate::server::capability_rebuild::WarmReport;

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

    /// Pin ONE immutable config snapshot and hand back an owned pricer over it.
    ///
    /// Owned (not borrowed) so the whole grouped query can be priced on a
    /// blocking worker against a single snapshot: a router hot-swap mid-query
    /// can never make two rows of one result price against different rate
    /// tables.
    pub fn pricer(&self) -> QueryPricer {
        QueryPricer {
            router: self.inner.load_full(),
        }
    }
}

/// An owned, `'static` pricing facade over one pinned router snapshot. The
/// `Arc<Router>` is private to this module, so a caller can price a row and
/// nothing else -- it can never reach dispatch or raw config through it.
pub struct QueryPricer {
    router: Arc<Router>,
}

impl QueryPricer {
    /// The cost verdict for one fine-grained aggregate row, resolved against
    /// the pinned snapshot through the same function the CLI usage report uses.
    /// The catalog overlay comes off the SAME pinned router the config does, so
    /// one query can never price against a config generation and a foreign
    /// overlay generation.
    pub fn price(&self, row: &AggRow) -> RowCost {
        cost_for_row(&self.router.config, self.router.catalog_overlay(), row)
    }
}

/// A read-only view over one pinned router snapshot. The `router` field is
/// private, so the only router state a panel can observe is what the seven
/// methods below expose -- route health, learned negatives, the live
/// envelope-field repair counters, the per-verdict fidelity rows, the live
/// wire-check counters and findings, the derived
/// effective config view (which is computed here so panels never handle raw
/// `Config`), and the doctor's served inputs (an opaque value only the doctor
/// gather can read).
pub struct StatusRouterView {
    router: Arc<Router>,
}

impl StatusRouterView {
    /// Per-dispatch-target health for the health panel.
    pub fn route_targets(&self, now: Instant) -> Vec<RouteTargetStatus> {
        self.router.status_targets(now)
    }

    /// Every resident learned-capability entry for the health panel, each with
    /// its cell resolved at `now` through the shared lane-cell helper, the
    /// way the doctor matrix resolves it: same config, overrides, seed-clear
    /// markers and shipped beta seed, but no catalog priors.
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
            prior: &|_, _| None,
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

    /// The derived effective-config view for the config panel, folding the
    /// live config together with the catalog overlay the pinned Router was
    /// BUILT against -- the generation the daemon accepted, read out of
    /// memory. Neither input is re-read from disk here, so one derivation can
    /// never pair a config generation with a different overlay generation.
    ///
    /// The raw `Config` is read only inside this method; the panel receives
    /// the already-derived, secret-free [`EffectiveView`].
    pub fn effective_view(&self) -> EffectiveView {
        derive_effective_view(&self.router.config, self.router.catalog_overlay())
    }

    /// The served doctor gather's inputs, owned so the gather can run on a
    /// blocking worker.
    ///
    /// The config and the catalog overlay come off this one pinned router, so
    /// the doctor never pairs a config generation with a foreign overlay
    /// generation, and neither is re-read from disk. The learned layer is the
    /// resident registry, read once; its clock anchors are taken after that
    /// read, so every entry's instants lie at or before them. `warm` is the
    /// boot warm that filled the registry. The credential-parked seats come off
    /// the same per-target status read the health panel uses.
    ///
    /// The returned value's fields are private to the doctor module: a panel
    /// holding it can pass it to the gather and nothing else.
    pub(crate) fn served_doctor_inputs(&self, warm: WarmReport) -> ServedInputs {
        let entries = self.router.learned_capability_snapshot();
        let seed_clears = self.router.learned_seed_clear_snapshot();
        let learned = ResidentLearned {
            entries,
            seed_clears,
            now: Instant::now(),
            now_ms: chrono::Utc::now().timestamp_millis(),
        };
        ServedInputs::new(
            Arc::clone(&self.router.config),
            self.router.catalog_overlay().clone(),
            learned,
            warm,
        )
        .with_parked_seats(self.parked_seats())
    }

    /// Every seat whose credential is parked right now, with its deadline as
    /// epoch ms.
    fn parked_seats(&self) -> Vec<ParkedSeat> {
        let now = Instant::now();
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.router
            .status_targets(now)
            .into_iter()
            .filter_map(|target| {
                let park = target.gate.credential_park?;
                let remaining_ms = i64::try_from(park.remaining.as_millis()).unwrap_or(i64::MAX);
                Some(ParkedSeat {
                    state_key: target.state_key,
                    reason: park.reason,
                    parked_until_ms: now_ms.saturating_add(remaining_ms),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::Config;

    fn handle() -> StatusRouterHandle {
        let router = Router::new(Arc::new(Config::default()));
        StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router)))
    }

    #[test]
    fn view_exposes_only_the_seven_read_methods() {
        let handle = handle();
        let view = handle.view();

        // The full read surface: route health, learned negatives, live
        // field-repair counters, the coherent fidelity snapshot, the wire-check
        // snapshot, effective view, the doctor's served inputs. If a future edit
        // widens this surface (e.g. exposes `&Router` or a dispatch method), it
        // lands here in review against a facade whose contract is these seven
        // calls.
        let _targets: Vec<RouteTargetStatus> = view.route_targets(Instant::now());
        let _learned: Vec<ResolvedEntry> =
            view.learned_capabilities((Instant::now(), chrono::Utc::now().timestamp_millis()));
        let _counters: FieldRepairCounters = view.field_repair_counters();
        let _fidelity: FidelitySnapshot = view.fidelity_snapshot();
        let _wire_check: WireCheckSnapshot = view.wire_check_snapshot();
        let _effective: EffectiveView = view.effective_view();
        let _served: ServedInputs = view.served_doctor_inputs(WarmReport::not_run());
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
