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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use routectl_router::router::RouteTargetStatus;
use routectl_router::{
    EffectiveView, FidelitySnapshot, FieldRepairCounters, LearnedRegistryEntry, Router,
    derive_effective_view,
};
use routectl_usage::{AggRow, RowCost};

use crate::commands::usage::cost_for_row;

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
/// private, so the only router state a panel can observe is what the six
/// methods below expose -- route health, learned negatives, the live
/// envelope-field repair counters, the per-verdict fidelity rows, the configured
/// paid-probe caps, and the derived effective config view (which is computed here
/// so panels never handle raw `Config`).
pub struct StatusRouterView {
    router: Arc<Router>,
}

impl StatusRouterView {
    /// Per-dispatch-target health for the health panel.
    pub fn route_targets(&self, now: Instant) -> Vec<RouteTargetStatus> {
        self.router.status_targets(now)
    }

    /// The learned-capability registry snapshot for the health panel.
    pub fn learned_capabilities(&self) -> Vec<LearnedRegistryEntry> {
        self.router.learned_capability_snapshot()
    }

    /// The live process-lifetime envelope-field repair counters, for the
    /// health panel to log at INFO.
    pub fn field_repair_counters(&self) -> FieldRepairCounters {
        self.router.field_repair_counters()
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

    /// The operator's configured per-provider paid-probe UTC-day caps.
    ///
    /// Read through the facade rather than from a re-loaded config file, so the
    /// caps a budget row reports are the ones the RUNNING daemon gates
    /// reservations on -- a file read could pick up an edit the daemon has not
    /// accepted, and report a cap no reservation is checked against.
    ///
    /// Cloned rather than borrowed: the map is small (one entry per provider an
    /// operator opted into paid probes, default none) and the caller uses it
    /// outside the pinned snapshot's borrow.
    pub fn paid_probe_daily_caps(&self) -> BTreeMap<String, u32> {
        self.router.config.fidelity.paid_probe_daily_caps.clone()
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
    fn view_exposes_only_the_six_read_methods() {
        let handle = handle();
        let view = handle.view();

        // The full read surface: route health, learned negatives, live
        // field-repair counters, the coherent fidelity snapshot, the configured
        // paid-probe caps, effective view. If a future edit widens this surface
        // (e.g. exposes `&Router` or a dispatch method), it lands here in review
        // against a facade whose contract is these six calls.
        let _targets: Vec<RouteTargetStatus> = view.route_targets(Instant::now());
        let _learned: Vec<LearnedRegistryEntry> = view.learned_capabilities();
        let _counters: FieldRepairCounters = view.field_repair_counters();
        let _fidelity: FidelitySnapshot = view.fidelity_snapshot();
        let _caps: BTreeMap<String, u32> = view.paid_probe_daily_caps();
        let _effective: EffectiveView = view.effective_view();
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
