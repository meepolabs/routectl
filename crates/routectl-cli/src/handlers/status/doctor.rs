//! `/status/doctor` panel: the no-network doctor report plus a reachability
//! summary derived from the live circuit breaker and the live
//! capability-write failure counters.
//!
//! A `/status` request NEVER dials an upstream. The panel runs only the
//! no-network doctor sections (inventory / version / config / auth / secrets /
//! capability) through [`gather_context_no_network`] +
//! [`build_report_no_network`] -- the `probe` section and its
//! `gather_probe_results` dial are never reached. Reachability is DERIVED from
//! the last settled dispatch outcome read once through the read-only facade,
//! not from a fresh probe.
//!
//! The gather runs over the daemon's SERVED state
//! ([`GatherSources::Served`]): the config and catalog overlay the running
//! router accepted and its resident learned registry, read through the
//! read-only facade. A poll parses no config file, loads no overlay file, and
//! replays no ledger. What disk I/O remains (the raw config bytes for the
//! validation findings, the credential-store probe) runs inside the
//! `spawn_blocking` builder [`guard_panel`] wraps it in, driven to completion
//! with a runtime handle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_core::failure_class::LastOutcome;
use routectl_router::DoctorReport;
use routectl_router::router::RouteTargetStatus;

use super::field_verdict_log::{FidelityEmission, FidelityGate, log_field_verdict_snapshot};
use super::paid_probe_budget::{
    AccountingGlobals, CapabilityWriteCounters, PaidProbeBudget, accounting_globals,
    paid_probe_budgets,
};
use super::router_view::StatusRouterView;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
use crate::commands::doctor::{GatherSources, build_report_no_network, gather_context_no_network};

/// Wire-shape version of the doctor panel payload. Reuses the no-network
/// [`DoctorReport`]'s own `schema_version` (15): the panel embeds that report
/// verbatim, so it must not invent a parallel number. The panel's own
/// `capability_writes` block joined at 13 alongside the report's matrix
/// change.
pub const DOCTOR_SCHEMA_VERSION: u32 = 15;

/// The no-network doctor report plus the circuit-derived reachability summary
/// and the live capability-write counters.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DoctorPanel {
    /// The full no-network report (findings + panels), embedded verbatim.
    report: DoctorReport,
    /// One reachability verdict per dispatch target, folded from its live
    /// circuit phase. Never a fresh dial.
    reachability: Vec<TargetReachability>,
    /// The daemon's capability-persistence failure counters, read once per
    /// build from the writer's own shared counters.
    capability_writes: CapabilityWriteCounters,
}

/// One dispatch target's reachability, derived from its last settled outcome.
#[derive(Debug, Clone, Serialize)]
struct TargetReachability {
    state_key: String,
    /// `reachable` (last outcome ok), `unknown` (nothing settled yet), or
    /// `degraded` (any failure family / gate refusal).
    reachability: &'static str,
}

/// Fold the last settled outcome into a reachability verdict. A successful
/// last outcome is `reachable`; no settled outcome yet (fresh state /
/// post-restart) is `unknown`; any failure family or gate refusal is
/// `degraded`. Owned here so a new outcome variant is a compile error rather
/// than a silent wire change.
const fn reachability_token(outcome: Option<LastOutcome>) -> &'static str {
    match outcome {
        Some(LastOutcome::Ok) => "reachable",
        None => "unknown",
        Some(_) => "degraded",
    }
}

fn map_reachability(target: RouteTargetStatus) -> TargetReachability {
    TargetReachability {
        state_key: target.state_key,
        reachability: reachability_token(target.gate.last_outcome),
    }
}

/// Fold the gathered no-network report and the pinned router snapshot into
/// the panel data, emitting the shared field-verdict snapshot log along the
/// way. Split out from [`build_from_path`] so the wiring is directly
/// testable without going through the `spawn_blocking` builder.
fn build_panel_data(
    report: DoctorReport,
    view: &StatusRouterView,
    budgets: &[PaidProbeBudget],
    globals: AccountingGlobals,
    capability_writes: CapabilityWriteCounters,
    emission: FidelityEmission,
    gate: &FidelityGate,
) -> DoctorPanel {
    log_field_verdict_snapshot(view, budgets, globals, emission, gate);
    let reachability = view
        .route_targets(Instant::now())
        .into_iter()
        .map(map_reachability)
        .collect();
    DoctorPanel {
        report,
        reachability,
        capability_writes,
    }
}

/// Build the panel for a daemon with an on-disk config path. The gather's
/// served inputs and reachability are read from the SAME live router snapshot,
/// pinned before the blocking work; the gather runs inside the
/// `spawn_blocking` builder (via a runtime handle) so its remaining disk I/O
/// never blocks an async worker.
async fn build_from_path(
    state: &StatusState,
    config_path: PathBuf,
    emission: FidelityEmission,
) -> Panel<DoctorPanel> {
    let view = state.router.view();
    let served = view.served_doctor_inputs(*state.warm_report());
    // Captured here; the ledger OPEN runs inside the blocking closure below, beside
    // the gather's own disk I/O -- SQLite work on an async worker blocks that
    // worker, and `guard_panel` is what puts it on a blocking thread under a
    // cancellation-survivable permit.
    let caps = view.paid_probe_daily_caps();
    let db_path = state.usage_db_path.clone();
    // Counter reads, not I/O, so these stay out here beside the router snapshot.
    let globals = accounting_globals(&state.usage_health);
    let capability_writes = state.usage_health.capability_writes();
    let gate = Arc::clone(&state.fidelity_gate);
    let handle = tokio::runtime::Handle::current();
    // The snapshot is pinned now, so request time IS the read time.
    let as_of = now_utc_rfc3339();
    // The daemon's own fidelity observer, when a test installed one -- attached at
    // the builder rather than by the caller, for the reason `health` states.
    #[cfg(test)]
    let emission = emission.with_daemon_observer(state);
    guard_panel(
        &state.builder_capacity,
        DOCTOR_SCHEMA_VERSION,
        codes::DOCTOR_UNAVAILABLE,
        move || {
            let ctx = handle.block_on(gather_context_no_network(
                &config_path,
                GatherSources::Served(Box::new(served)),
            ));
            let report = build_report_no_network(&ctx);
            let schema_version = report.schema_version;
            let budgets =
                paid_probe_budgets(&caps, &db_path, chrono::Utc::now().timestamp_millis());
            let data = build_panel_data(
                report,
                &view,
                &budgets,
                globals,
                capability_writes,
                emission,
                &gate,
            );
            Panel::available(schema_version, as_of, data)
        },
    )
    .await
}

pub(super) async fn build(state: &StatusState) -> Panel<DoctorPanel> {
    build_with_emission(state, FidelityEmission::always()).await
}

/// The panel build, with the caller stating whether THIS build emits the shared
/// fidelity line. See `health::build_with_emission` for why the choice is the
/// caller's.
pub(super) async fn build_with_emission(
    state: &StatusState,
    emission: FidelityEmission,
) -> Panel<DoctorPanel> {
    let panel = match state.config_path.clone() {
        Some(config_path) => build_from_path(state, config_path, emission).await,
        None => {
            // A daemon serving without an on-disk config path has no doctor REPORT
            // to build, but it still has a live router -- and the observability
            // floor is about the router, not the report. So the fidelity line is
            // emitted here too, with UNAVAILABLE budget data: the caps come from
            // config and there is none to read, while every other field on the line
            // (the counters, the verdict rows, the probe state, the writer health)
            // comes from state this branch has in full.
            //
            // The alternative -- staying silent -- would mean a config-less daemon
            // satisfied the floor only through `/status/health`, which is a narrower
            // contract than the one this surface documents.
            log_field_verdict_snapshot(
                &state.router.view(),
                &[],
                accounting_globals(&state.usage_health),
                #[cfg(test)]
                emission.with_daemon_observer(state),
                #[cfg(not(test))]
                emission,
                &state.fidelity_gate,
            );
            Panel::unavailable(DOCTOR_SCHEMA_VERSION, codes::NO_CONFIG_PATH)
        }
    };
    state.observability.doctor.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<DoctorPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
