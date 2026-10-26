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
use routectl_router::router::RouteTargetStatus;
use routectl_router::{DoctorReport, LaneBudgetExhaustions, LaneLossyRepairs};

use super::field_verdict_log::{FidelityEmission, FidelityGate, log_field_verdict_snapshot};
use super::router_view::StatusRouterView;
use super::usage_health::CapabilityWriteCounters;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
use crate::commands::doctor::{GatherSources, build_report_no_network, gather_context_no_network};

/// Wire-shape version of the doctor panel payload. Reuses the no-network
/// [`DoctorReport`]'s own `schema_version` (22): the panel embeds that report
/// verbatim, so it must not invent a parallel number. The panel's own
/// `capability_writes` block joined at 13 alongside the report's matrix
/// change, its `lossy_repairs_by_lane` list at 18, and its
/// `budget_exhaustions_by_lane` list and `repairs_per_request` buckets at 19.
/// At 20 the embedded report's `config` section gained its old-folder finding.
/// At 21 the embedded report's `knobs` section lost its config state. At 22
/// every capability matrix cell gained the learned verdict's `value`.
pub const DOCTOR_SCHEMA_VERSION: u32 = 22;

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
    /// The process-lifetime lossy field-repair count per learned lane: at
    /// most 256 named lanes plus the `other` overflow bucket.
    lossy_repairs_by_lane: Vec<LaneLossyRepairs>,
    /// The process-lifetime count of rejections whose proposed repair the
    /// request's repair allowance could not pay, per learned lane: at most 256
    /// named lanes plus the `other` overflow bucket.
    budget_exhaustions_by_lane: Vec<LaneBudgetExhaustions>,
    /// Finished requests by the number of repairs each spent: index `n`
    /// counts the requests that drew exactly `n`, from zero to the whole
    /// per-request allowance.
    repairs_per_request: Vec<u64>,
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
    writer_degraded: bool,
    capability_writes: CapabilityWriteCounters,
    emission: FidelityEmission,
    gate: &FidelityGate,
) -> DoctorPanel {
    log_field_verdict_snapshot(view, writer_degraded, emission, gate);
    let reachability = view
        .route_targets(Instant::now())
        .into_iter()
        .map(map_reachability)
        .collect();
    DoctorPanel {
        report,
        reachability,
        capability_writes,
        lossy_repairs_by_lane: view.lossy_repairs_by_lane(),
        budget_exhaustions_by_lane: view.budget_exhaustions_by_lane(),
        repairs_per_request: view.repairs_per_request(),
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
    let reload_failure = state
        .daemon_meta
        .reload_failure(chrono::Utc::now().timestamp_millis());
    let served = view
        .served_doctor_inputs(*state.warm_report())
        .with_reload_failure(reload_failure);
    // Counter reads, not I/O, so these stay out here beside the router snapshot.
    let writer_degraded = state.usage_health.writer_degraded();
    let capability_writes = state.usage_health.capability_writes();
    let gate = Arc::clone(&state.fidelity_gate);
    let legacy_dirs = state.legacy_config_dirs().to_vec();
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
                &legacy_dirs,
            ));
            let report = build_report_no_network(&ctx);
            let schema_version = report.schema_version;
            let data = build_panel_data(
                report,
                &view,
                writer_degraded,
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
            // emitted here too: every field on the line (the counters, the verdict
            // rows, the probe state, the writer health) comes from state this branch
            // has in full.
            //
            // The alternative -- staying silent -- would mean a config-less daemon
            // satisfied the floor only through `/status/health`, which is a narrower
            // contract than the one this surface documents.
            log_field_verdict_snapshot(
                &state.router.view(),
                state.usage_health.writer_degraded(),
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

#[cfg(test)]
#[path = "doctor_park_tests.rs"]
mod park_tests;
