//! `/status/doctor` panel: the no-network doctor report plus a reachability
//! summary derived from the live circuit breaker.
//!
//! A `/status` request NEVER dials an upstream. The panel runs only the
//! no-network doctor sections (inventory / version / config / auth / secrets /
//! capability) through [`gather_context_no_network`] +
//! [`build_report_no_network`] -- the `probe` section and its
//! `gather_probe_results` dial are never reached. Reachability is DERIVED from
//! the already-observed circuit phase read once through the read-only facade,
//! not from a fresh probe.
//!
//! The no-network gather is disk I/O (config read, credential-store probe,
//! usage-ledger read), so it runs inside the `spawn_blocking` builder
//! [`guard_panel`] wraps it in, driven to completion with a runtime handle.
//! Config-load/parse errors are ALREADY redacted inside the gather (the shared
//! parse-error redactor), so this panel adds no second redaction copy and
//! surfaces no raw loader string.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::DoctorReport;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::CircuitPhase;

use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
use crate::commands::doctor::{build_report_no_network, gather_context_no_network};

/// Wire-shape version of the doctor panel payload. Reuses the no-network
/// [`DoctorReport`]'s own `schema_version` (2): the panel embeds that report
/// verbatim, so it must not invent a parallel number.
const DOCTOR_SCHEMA_VERSION: u32 = 2;

/// The no-network doctor report plus the circuit-derived reachability summary.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DoctorPanel {
    /// The full no-network report (findings + panels), embedded verbatim.
    report: DoctorReport,
    /// One reachability verdict per dispatch target, folded from its live
    /// circuit phase. Never a fresh dial.
    reachability: Vec<TargetReachability>,
}

/// One dispatch target's reachability, derived from its circuit phase.
#[derive(Debug, Clone, Serialize)]
struct TargetReachability {
    state_key: String,
    /// `reachable` (circuit closed) or `degraded` (open / half-open).
    reachability: &'static str,
}

/// Fold a circuit phase into a reachability verdict. An open OR half-open
/// breaker means the target is currently not freely dispatchable, so both map
/// to `degraded`; only a closed breaker is `reachable`. Owned here so a new
/// breaker phase is a compile error rather than a silent wire change.
const fn reachability_token(circuit: CircuitPhase) -> &'static str {
    match circuit {
        CircuitPhase::Closed => "reachable",
        CircuitPhase::Open | CircuitPhase::HalfOpenReady => "degraded",
    }
}

fn map_reachability(target: RouteTargetStatus) -> TargetReachability {
    TargetReachability {
        state_key: target.state_key,
        reachability: reachability_token(target.gate.circuit),
    }
}

/// Build the panel from an on-disk config path. The gather runs inside the
/// `spawn_blocking` builder (via a runtime handle) so its disk I/O never blocks
/// an async worker; reachability is read from the SAME live router snapshot
/// pinned before the blocking work.
async fn build_from_path(state: &StatusState, config_path: PathBuf) -> Panel<DoctorPanel> {
    let view = state.router.view();
    let handle = tokio::runtime::Handle::current();
    // The snapshot is pinned now, so request time IS the read time.
    let as_of = now_utc_rfc3339();
    guard_panel(
        DOCTOR_SCHEMA_VERSION,
        codes::DOCTOR_UNAVAILABLE,
        move || {
            let ctx = handle.block_on(gather_context_no_network(&config_path));
            let report = build_report_no_network(&ctx);
            let reachability = view
                .route_targets(Instant::now())
                .into_iter()
                .map(map_reachability)
                .collect();
            let schema_version = report.schema_version;
            Panel::available(
                schema_version,
                as_of,
                DoctorPanel {
                    report,
                    reachability,
                },
            )
        },
    )
    .await
}

pub(super) async fn build(state: &StatusState) -> Panel<DoctorPanel> {
    let panel = match state.config_path.clone() {
        Some(config_path) => build_from_path(state, config_path).await,
        None => Panel::unavailable(DOCTOR_SCHEMA_VERSION, codes::NO_CONFIG_PATH),
    };
    state.observability.doctor.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<DoctorPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::AppState;
    use arc_swap::ArcSwap;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use routectl_router::runtime_state::ProviderGateStatus;
    use routectl_router::{Config, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    fn state_with_config(config_path: Option<PathBuf>) -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        Arc::new(StatusState::from_app(&app, config_path))
    }

    fn sample_target(circuit: CircuitPhase) -> RouteTargetStatus {
        RouteTargetStatus {
            state_key: "opus#seat-a".into(),
            nickname: "opus".into(),
            provider_name: "anthropic".into(),
            upstream: "claude-opus-wire".into(),
            seat_label: Some("seat-a".into()),
            gate: ProviderGateStatus {
                rpm_available: Some(12.0),
                circuit,
                half_open_probe_in_flight: false,
                circuit_open_elapsed: None,
                last_outcome: None,
                last_outcome_elapsed: None,
            },
        }
    }

    #[test]
    fn reachability_token_folds_open_and_half_open_to_degraded() {
        assert_eq!(reachability_token(CircuitPhase::Closed), "reachable");
        assert_eq!(reachability_token(CircuitPhase::Open), "degraded");
        assert_eq!(reachability_token(CircuitPhase::HalfOpenReady), "degraded");
    }

    #[test]
    fn map_reachability_derives_from_circuit_phase_not_a_dial() {
        let mapped = map_reachability(sample_target(CircuitPhase::Open));
        assert_eq!(mapped.state_key, "opus#seat-a");
        assert_eq!(mapped.reachability, "degraded");
    }

    /// The production source must never CALL the probe dial: the panel is a
    /// strictly no-network surface. Scans for the call-shaped forms so a
    /// doc-comment mention of the names does not trip the guard.
    #[test]
    fn production_source_never_calls_the_probe_dial() {
        let src = include_str!("doctor.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !production.contains(concat!("gather_probe", "_results(")),
            "doctor panel must not call the probe gather"
        );
        assert!(
            !production.contains(concat!("section", "_probe(")),
            "doctor panel must not render the probe section"
        );
    }

    #[test]
    fn no_config_path_yields_unavailable_panel() {
        let panel = Panel::<DoctorPanel>::unavailable(DOCTOR_SCHEMA_VERSION, codes::NO_CONFIG_PATH);
        assert_eq!(panel.schema_version, 2);
        assert_eq!(panel.unavailable.as_deref(), Some("no_config_path"));
        assert!(panel.data.is_none());
    }

    /// Pins the panel constant to `DoctorReport`'s own schema version, so a
    /// future report bump that is not mirrored here (the unavailable/None paths
    /// carry the constant, the success path carries `report.schema_version`)
    /// fails loudly instead of letting the two envelope versions drift.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panel_constant_tracks_the_no_network_report_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, b"version = 3\n").unwrap();

        let ctx = gather_context_no_network(&config_path).await;
        let report = build_report_no_network(&ctx);
        assert_eq!(report.schema_version, DOCTOR_SCHEMA_VERSION);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_returns_schema_two_report_with_no_probe_section() {
        // A config with an unknown field carrying a secret-shaped value: the
        // typed load fails, and the gather redacts the loader error before it
        // reaches any finding. The report still builds (schema 2, no probe).
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            b"version = 3\nbogus_field = \"literal:sk-live-LEAKED\"\n",
        )
        .unwrap();

        let state = state_with_config(Some(config_path));
        let app = super::super::status_router().with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/status/doctor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["schema_version"], 2);
        assert!(json["unavailable"].is_null());
        let as_of = json["as_of"].as_str().expect("as_of present");
        assert!(chrono::DateTime::parse_from_rfc3339(as_of).is_ok());

        assert_eq!(json["data"]["report"]["schema_version"], 2);
        let findings = json["data"]["report"]["findings"].as_array().unwrap();
        assert!(
            !findings.is_empty(),
            "no-network sections still produce findings"
        );
        for finding in findings {
            assert_ne!(
                finding["section"], "probe",
                "no probe section may appear on a no-network panel"
            );
        }
        // A default router resolves no targets, so reachability is empty --
        // derived, never a dial.
        assert!(json["data"]["reachability"].is_array());

        // Test #7 (redaction): the raw loader error is already redacted inside
        // the gather, so no path / scheme value / secret reaches the payload.
        let text = serde_json::to_string(&json).unwrap();
        for forbidden in ["LEAKED", "literal:", "env://", "file://", "config.toml"] {
            assert!(
                !text.contains(forbidden),
                "doctor payload leaked `{forbidden}`: {text}"
            );
        }
    }
}
