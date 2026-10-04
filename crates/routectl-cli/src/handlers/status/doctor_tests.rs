/// The NO-CONFIG doctor branch still emits the fidelity line.
///
/// A daemon serving without an on-disk config path has no doctor report to
/// build, but it has a live router -- and the observability floor is about the
/// router. Staying silent here would mean a config-less daemon satisfied the
/// floor only through `/status/health`, a narrower contract than this surface
/// documents.
///
/// The budget rows are legitimately empty (the caps come from config and there
/// is none), which is why the line is emitted with unavailable budget data
/// rather than withheld: every OTHER field on it comes from state this branch
/// has in full.
///
/// Mutation check: drop the `log_field_verdict_snapshot` call from the `None`
/// arm -> red here.
#[test]
fn the_no_config_branch_still_emits_the_fidelity_snapshot() {
    let router = Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        Config::default(),
    ))));
    let (app, _dir) = AppState::for_test(router);
    // No config path: the branch under test.
    let state = Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()));

    // `capture_events` takes a synchronous closure, so the build is driven on a
    // current-thread runtime inside it. A bare futures executor is not a
    // substitute: the builder arms tokio machinery.
    let mut panel = None;
    let events = routectl_testkit::capture_events(|| {
        panel = Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime")
                .block_on(build(&state)),
        );
    });
    let panel = panel.expect("the build ran");

    assert!(
        panel.unavailable.is_some(),
        "premise: this IS the no-config branch, so the panel is unavailable",
    );
    assert!(
        events
            .iter()
            .any(|e| e.message
                == crate::handlers::status::field_verdict_log::FIDELITY_SNAPSHOT_MESSAGE),
        "and the fidelity line is still emitted: the floor is about the router, \
             which this branch has in full",
    );
}

use super::*;
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use routectl_router::runtime_state::{CircuitPhase, ProviderGateStatus};
use routectl_router::{Config, Router};
use serde_json::Value;
use tower::ServiceExt;

fn state_with_config(config_path: Option<PathBuf>) -> Arc<StatusState> {
    let router = Router::new(Arc::new(Config::default()));
    let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    Arc::new(StatusState::from_app(
        &app,
        config_path,
        DaemonMeta::for_test(),
    ))
}

fn sample_target(last_outcome: Option<LastOutcome>) -> RouteTargetStatus {
    RouteTargetStatus {
        state_key: "opus#seat-a".into(),
        nickname: "opus".into(),
        provider_name: "anthropic".into(),
        upstream: "claude-opus-wire".into(),
        seat_label: Some("seat-a".into()),
        gate: ProviderGateStatus {
            rpm_available: Some(12.0),
            circuit: CircuitPhase::Closed,
            half_open_probe_in_flight: false,
            circuit_open_elapsed: None,
            last_outcome,
            last_outcome_elapsed: None,
        },
    }
}

#[test]
fn reachability_token_maps_the_three_states() {
    assert_eq!(reachability_token(Some(LastOutcome::Ok)), "reachable");
    assert_eq!(reachability_token(None), "unknown");
    assert_eq!(reachability_token(Some(LastOutcome::Http5xx)), "degraded");
    assert_eq!(
        reachability_token(Some(LastOutcome::RateLimited)),
        "degraded"
    );
    assert_eq!(
        reachability_token(Some(LastOutcome::CircuitOpen)),
        "degraded"
    );
}

#[test]
fn map_reachability_derives_from_last_outcome_not_a_dial() {
    let mapped = map_reachability(sample_target(Some(LastOutcome::Http5xx)));
    assert_eq!(mapped.state_key, "opus#seat-a");
    assert_eq!(mapped.reachability, "degraded");
}

/// Wire-shape golden: reachability serializes under its stable key with the
/// unknown state for a never-seen (no settled outcome) target.
#[test]
fn reachability_serializes_unknown_for_never_seen_target() {
    let value = serde_json::to_value(map_reachability(sample_target(None))).unwrap();
    assert_eq!(value["state_key"], Value::from("opus#seat-a"));
    assert_eq!(value["reachability"], Value::from("unknown"));
}

/// The production source must never CALL the probe dial: the panel is a
/// strictly no-network surface. Scans for the call-shaped forms so a
/// doc-comment mention of the names does not trip the guard.
#[test]
fn production_source_never_calls_the_probe_dial() {
    let production = include_str!("doctor.rs");
    assert!(
        !production.contains(concat!("gather_probe", "_results(")),
        "doctor panel must not call the probe gather"
    );
    assert!(
        !production.contains(concat!("section", "_probe(")),
        "doctor panel must not render the probe section"
    );
}

/// `build_panel_data` wires the shared field-verdict snapshot log into
/// the doctor build path -- the log's own behavior is pinned by
/// `field_verdict_log`'s tests; this pins only that the doctor build
/// actually calls it. Exercised directly rather than through
/// `build_from_path`: that entry point always runs its build inside
/// `guard_panel`'s `spawn_blocking`, a thread `capture_events` cannot see.
#[tokio::test]
async fn build_panel_data_emits_the_field_verdict_snapshot_log() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
    )
    .unwrap();
    let state = state_with_config(Some(config_path.clone()));
    let view = state.router.view();
    let ctx = gather_context_no_network(&config_path).await;
    let report = build_report_no_network(&ctx);

    let events = routectl_testkit::capture_events(|| {
        build_panel_data(
            report,
            &view,
            &[],
            AccountingGlobals {
                writer_degraded: false,
                consumed_unauthorized_total: 0,
            },
            FidelityEmission::always(),
        );
    });

    assert!(
        events
            .iter()
            .any(|e| e.message == "envelope field verdict snapshot"),
        "doctor panel build must emit the field verdict snapshot log"
    );
}

#[test]
fn no_config_path_yields_unavailable_panel() {
    let panel = Panel::<DoctorPanel>::unavailable(DOCTOR_SCHEMA_VERSION, codes::NO_CONFIG_PATH);
    assert_eq!(panel.schema_version, 12);
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
    std::fs::write(
        &config_path,
        format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
    )
    .unwrap();

    let ctx = gather_context_no_network(&config_path).await;
    let report = build_report_no_network(&ctx);
    assert_eq!(report.schema_version, DOCTOR_SCHEMA_VERSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_returns_report_with_no_probe_section() {
    // A config with an unknown field carrying a secret-shaped value: the
    // typed load fails, and the gather redacts the loader error before it
    // reaches any finding. The report still builds (schema 3, no probe).
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {}\nbogus_field = \"literal:sk-live-LEAKED\"\n",
            routectl_router::CURRENT_CONFIG_VERSION
        ),
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

    assert_eq!(json["schema_version"], 12);
    assert!(json["unavailable"].is_null());
    let as_of = json["as_of"].as_str().expect("as_of present");
    assert!(chrono::DateTime::parse_from_rfc3339(as_of).is_ok());

    assert_eq!(json["data"]["report"]["schema_version"], 12);
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

    // REDACTION: the raw loader error is already redacted inside
    // the gather, so no path / scheme value / secret reaches the payload.
    let text = serde_json::to_string(&json).unwrap();
    for forbidden in ["LEAKED", "literal:", "env://", "file://", "config.toml"] {
        assert!(
            !text.contains(forbidden),
            "doctor payload leaked `{forbidden}`: {text}"
        );
    }
}

/// The `/status/doctor` panel embeds the no-network doctor report, which
/// carries the catalog-freshness section. The freshness rows must surface
/// on this JSON surface exactly as they do on the CLI `doctor` output --
/// the baked-catalog row is present unconditionally (no overlay, no import
/// needed), so a status consumer sees the same catalog-freshness signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_panel_embeds_catalog_freshness_rows() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {}\n", routectl_router::CURRENT_CONFIG_VERSION),
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

    let findings = json["data"]["report"]["findings"]
        .as_array()
        .expect("findings array");
    assert!(
        findings.iter().any(|f| f["section"] == "freshness"),
        "status doctor panel must embed the catalog-freshness section"
    );
    assert!(
        findings
            .iter()
            .any(|f| { f["section"] == "freshness" && f["name"] == "baked catalog" }),
        "the baked-catalog freshness row must be present unconditionally"
    );
}
