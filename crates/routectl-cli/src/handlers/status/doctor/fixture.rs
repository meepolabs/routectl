//! Test-only fully populated `/status/doctor` panel: a no-network report in
//! the router's own DTOs, beside reachability folded by this panel's mapper
//! and lane counters read off a live router.
//!
//! The report is assembled from its DTOs rather than gathered: the real
//! gather probes the operator's credential store, which a fixture must never
//! read.

use routectl_router::router::RouteTargetStatus;
use routectl_router::{
    CapabilityMatrixPanel, Config, DoctorPanels, DoctorReport, Finding, MatrixAvailability,
    MatrixCell, MatrixLane, MatrixNicknameAction, MatrixReplaySummary, MatrixSource, MatrixWarm,
    Router, Status, WouldTrimPanel, seed_distinct_fidelity_counters_for_tests,
};
use serde_json::Value;

use super::super::panel_fixtures::{
    FIXTURE_NOW_MS, fixture_as_of, fixture_router, fixture_targets,
};
use super::super::usage_health::{CapabilityWriteCounters, LedgerHealth, WindowObservation};
use super::{DOCTOR_SCHEMA_VERSION, DoctorPanel, Panel, map_reachability};

/// The serialized populated doctor envelope.
pub fn populated_doctor() -> Value {
    serde_json::to_value(populated_doctor_panel()).expect("the doctor panel serializes")
}

/// A report holding a finding of each severity, with and without a
/// remediation, a would-trim panel and a capability matrix with a mixed
/// cell, beside reachability of every token, nonzero counters and an
/// incomplete observation block.
pub fn populated_doctor_panel() -> Panel<DoctorPanel> {
    let router = fixture_router();
    seed_distinct_fidelity_counters_for_tests(&router);
    let dto = DoctorPanel {
        report: report(),
        reachability: fixture_targets()
            .into_iter()
            .map(map_reachability)
            .collect(),
        capability_writes: CapabilityWriteCounters {
            capability_events_dropped_full: 2,
            write_errors: 1,
            capability_writer_unavailable: 1,
        },
        lossy_repairs_by_lane: router.lossy_repairs_by_lane(),
        budget_exhaustions_by_lane: router.budget_exhaustions_by_lane(),
        repairs_per_request: router.repairs_per_request(),
        ledger_health: LedgerHealth {
            dropped_full: 4,
            dropped_closed: 1,
            write_errors: 1,
        },
        observation: Some(observation()),
    };
    Panel::available(DOCTOR_SCHEMA_VERSION, fixture_as_of(), dto)
}

/// A clean no-network report (every check passed, no matrix) stamped
/// `as_of`, beside the reachability of `targets`, a fresh router's lane
/// counters over `config` and zero loss counters.
pub fn clean_doctor_panel(
    config: Config,
    targets: Vec<RouteTargetStatus>,
    as_of: String,
) -> Panel<DoctorPanel> {
    let router = Router::new(std::sync::Arc::new(config));
    let dto = DoctorPanel {
        report: DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            findings: vec![finding("version", "binary version", Status::Pass, None)],
            panels: DoctorPanels::default(),
        },
        reachability: targets.into_iter().map(map_reachability).collect(),
        capability_writes: CapabilityWriteCounters {
            capability_events_dropped_full: 0,
            write_errors: 0,
            capability_writer_unavailable: 0,
        },
        lossy_repairs_by_lane: router.lossy_repairs_by_lane(),
        budget_exhaustions_by_lane: router.budget_exhaustions_by_lane(),
        repairs_per_request: router.repairs_per_request(),
        ledger_health: LedgerHealth::default(),
        observation: None,
    };
    Panel::available(DOCTOR_SCHEMA_VERSION, as_of, dto)
}

fn report() -> DoctorReport {
    DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        findings: vec![
            finding(
                "auth",
                "seat anthropic#a",
                Status::Fail,
                Some("routectl login anthropic --label a"),
            ),
            finding(
                "config",
                "old config folder",
                Status::Warn,
                Some("remove the old folder"),
            ),
            finding("version", "binary version", Status::Pass, None),
        ],
        panels: DoctorPanels {
            would_trim: Some(WouldTrimPanel {
                candidate_requests: 12,
                would_trim_tokens: 48_000,
                verdict_met: 5,
                verdict_unmet: 4,
                verdict_cold: 2,
                verdict_unpriced: 1,
            }),
            capability_matrix: Some(matrix()),
        },
    }
}

fn finding(section: &'static str, name: &str, status: Status, fix: Option<&str>) -> Finding {
    Finding {
        section,
        name: name.to_string(),
        status,
        detail: format!("{name}: fixture detail"),
        remediation: fix.map(str::to_string),
    }
}

/// A resident matrix over two lanes: a learned negative with its value and
/// timestamps, an unknown cell, and a mixed cell carrying each nickname's
/// own resolution.
fn matrix() -> CapabilityMatrixPanel {
    let learned = MatrixCell {
        verdict: "broken",
        supported: Some(false),
        source: Some("live"),
        layer: Some("learned"),
        overrode_layer: Some("seed"),
        action: "route_away",
        age_ms: Some(5_000),
        stale: false,
        first_seen_ms: Some(FIXTURE_NOW_MS - 600_000),
        last_seen_ms: Some(FIXTURE_NOW_MS - 5_000),
        expires_at_ms: Some(FIXTURE_NOW_MS + 3_600_000),
        value: Some("ceiling:32000".to_string()),
        nickname_actions: Vec::new(),
    };
    let unknown = MatrixCell {
        verdict: "unknown",
        supported: None,
        source: None,
        layer: None,
        overrode_layer: None,
        action: "none",
        age_ms: None,
        stale: false,
        first_seen_ms: None,
        last_seen_ms: None,
        expires_at_ms: None,
        value: None,
        nickname_actions: Vec::new(),
    };
    let mixed = MatrixCell {
        verdict: "mixed",
        action: "mixed",
        stale: true,
        nickname_actions: vec![
            MatrixNicknameAction {
                nickname: "seat-sonnet".to_string(),
                verdict: "forced_supported",
                layer: Some("override"),
                overrode_layer: Some("learned"),
                action: "allow",
            },
            MatrixNicknameAction {
                nickname: "sonnet".to_string(),
                verdict: "unknown",
                layer: None,
                overrode_layer: None,
                action: "none",
            },
        ],
        ..unknown.clone()
    };
    CapabilityMatrixPanel {
        availability: MatrixAvailability::Available,
        source: MatrixSource::Resident,
        columns: vec!["web_search".to_string(), "computer_use".to_string()],
        lanes: vec![
            MatrixLane {
                lane: "compat#glm-5".to_string(),
                nicknames: vec!["glm".to_string()],
                provider_kind: "openai-compat",
                routed: true,
                cells: vec![learned, unknown],
            },
            MatrixLane {
                lane: "retired#old-model".to_string(),
                nicknames: vec!["seat-sonnet".to_string(), "sonnet".to_string()],
                provider_kind: "",
                routed: false,
                cells: vec![mixed.clone(), mixed],
            },
        ],
        replay: Some(replay_summary()),
        warm: Some(MatrixWarm {
            outcome: "unreadable".to_string(),
            class: Some("open_failed".to_string()),
            summary: Some(replay_summary()),
        }),
    }
}

const fn replay_summary() -> MatrixReplaySummary {
    MatrixReplaySummary {
        loaded_rows: 40,
        replayed: 33,
        skipped_vocab: 1,
        skipped_owner: 2,
        skipped_revision: 0,
        skipped_lane: 1,
        skipped_unknown: 2,
        skipped_namespace: 1,
    }
}

const fn observation() -> WindowObservation {
    WindowObservation {
        from_ms: FIXTURE_NOW_MS - 43_200_000,
        to_ms: FIXTURE_NOW_MS + 1,
        wire_losses: 2,
        translation_drops: 1,
        local_refusals: 1,
        field_repairs: 3,
        missing_terminals: 1,
        request_caused_failures: 1,
        excluded_endpoint: 1,
        excluded_model: 2,
        excluded_target: 1,
        excluded_probe_run: 1,
        gap_rows: 1,
        gap_dropped: 4,
        complete: false,
    }
}
