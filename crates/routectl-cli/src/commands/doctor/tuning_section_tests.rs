//! The `tuning` section: one informational finding per resolved-tuning row,
//! on both the CLI report and the served no-network report.

use std::sync::Arc;
use std::time::Instant;

use routectl_router::{CatalogOverlay, TuningTargetKind, resolve_tuning};
use routectl_testkit::ScopedEnv;

use crate::server::capability_rebuild::{WarmOutcome, WarmReport};

use super::render::render_human;
use super::sections::section_tuning;
use super::tests::{ctx, current_version_stamp};
use super::*;

/// Every tier supplies at least one cell: a provider key on `tuned`, a pool
/// key inherited by its untuned member `seat-b`, an explicit `[retry]`
/// global, a model's own first-content override, and the shipped defaults
/// for whatever nothing sets.
fn every_tier_config() -> Config {
    toml::from_str(
        r#"
version = 2

[providers.tuned]
kind = "openai-compat"
base_url = "https://tuned.example/v1"
api_key_ref = "env://K"
circuit_failures = 0
rpm_limit = 30

[providers.seat-b]
kind = "openai-compat"
base_url = "https://seat.example/v1"
api_key_ref = "env://K"

[pools.team]
members = ["seat-b"]
circuit_cooldown_ms = 45000
rpm_limit = 12

[retry]
request_timeout_ms = 900000

[models.slow]
provider = "team"
upstream = "u"
stream_first_byte_timeout_ms = 70000
"#,
    )
    .expect("valid config")
}

fn tuning_findings(findings: &[Finding]) -> Vec<&Finding> {
    findings.iter().filter(|f| f.section == "tuning").collect()
}

/// The CLI report carries one tuning finding per provider entry and per
/// model override, each naming all five cells with the tier that supplied
/// it, and none of them can move the exit code.
///
/// Mutation check: drop the `tuning` row from `SECTIONS` -> red here.
#[test]
fn the_report_names_each_tuning_cell_with_its_value_and_source() {
    let context = ctx(
        every_tier_config(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );

    let report = build_report(&context);

    let rows: Vec<(&str, &str)> = tuning_findings(&report.findings)
        .iter()
        .map(|f| (f.name.as_str(), f.detail.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "model slow#seat-b",
                "circuit_failures=5(default) circuit_cooldown_ms=45000(pool) \
                 rpm_limit=12(pool) request_timeout_ms=900000(global) \
                 stream_first_byte_timeout_ms=70000(model)"
            ),
            (
                "provider seat-b",
                "circuit_failures=5(default) circuit_cooldown_ms=45000(pool) \
                 rpm_limit=12(pool) request_timeout_ms=900000(global) \
                 stream_first_byte_timeout_ms=300000(default)"
            ),
            (
                "provider tuned",
                "circuit_failures=off(provider) circuit_cooldown_ms=60000(default) \
                 rpm_limit=30(provider) request_timeout_ms=900000(global) \
                 stream_first_byte_timeout_ms=300000(default)"
            ),
        ]
    );
    for f in tuning_findings(&report.findings) {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
}

/// The human render titles the section rather than filing it under "Other".
#[test]
fn the_human_render_titles_the_tuning_section() {
    let context = ctx(
        every_tier_config(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );

    let lines = render_human(&build_report(&context));

    let at = lines
        .iter()
        .position(|l| l == "[Resolved tuning]")
        .unwrap_or_else(|| panic!("no tuning title: {lines:?}"));
    assert!(
        lines[at + 1].starts_with("  PASS model slow#seat-b: circuit_failures="),
        "{lines:?}"
    );
}

/// A target name is sanitized like every other section's operator name, so
/// a control character or a cell delimiter cannot forge a finding line.
#[test]
fn a_tuning_target_name_is_sanitized() {
    let context = DoctorContext {
        tuning: vec![TuningRow {
            target: "evil\n  PASS forged=1".to_string(),
            kind: TuningTargetKind::Provider,
            tuning: resolve_tuning(&Config::default(), None, None),
        }],
        ..ctx(Config::default(), None, Vec::new(), Vec::new())
    };

    let findings = section_tuning(&context);

    assert_eq!(findings.len(), 1, "{findings:?}");
    let name = &findings[0].name;
    assert!(name.starts_with("provider evil"), "{name}");
    assert!(!name.contains('\n') && !name.contains('='), "{name}");
}

/// The served `/status/doctor` report (the no-network section list over the
/// daemon's accepted config) carries the tuning section.
///
/// Mutation check: drop the `tuning` row from `NO_NETWORK_SECTIONS` -> red
/// here.
#[tokio::test]
#[serial_test::serial]
async fn the_served_no_network_report_carries_the_tuning_section() {
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let config_path = tmp.path().join("config.toml");
    let inputs = ServedInputs::new(
        Arc::new(every_tier_config()),
        CatalogOverlay::default(),
        ResidentLearned {
            entries: Vec::new(),
            seed_clears: Vec::new(),
            now: Instant::now(),
            now_ms: 1_000,
        },
        WarmReport {
            outcome: WarmOutcome::Replayed,
            summary: None,
            loaded_rows: 0,
        },
    );

    let context =
        gather_context_no_network(&config_path, GatherSources::Served(Box::new(inputs)), &[]).await;
    let report = build_report_no_network(&context);

    let names: Vec<&str> = tuning_findings(&report.findings)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["model slow#seat-b", "provider seat-b", "provider tuned"]
    );
}
