use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_router::{CapabilityRebuildSummary, LearnedRegistryEntry};

use super::*;

const LANE: &str = "p#up";

fn resident_negative(at: Instant) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: LANE.to_string(),
        feature_key: "prompt_caching".to_string(),
        verdict: Verdict::LearnedBroken(FailurePhase::F1),
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: at,
        last_seen: at,
        expires_at: at,
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
    }
}

fn learned(entries: Vec<LearnedRegistryEntry>) -> ResidentLearned {
    ResidentLearned {
        entries,
        seed_clears: Vec::new(),
        now: Instant::now(),
        now_ms: 1_000,
    }
}

const fn warm(outcome: WarmOutcome) -> WarmReport {
    WarmReport {
        outcome,
        summary: None,
        loaded_rows: 0,
    }
}

/// The availability class a resident matrix source renders as.
fn class(source: &CapabilityMatrixSource) -> String {
    match source {
        CapabilityMatrixSource::Available { .. } => "available".to_string(),
        CapabilityMatrixSource::Empty { .. } => "empty".to_string(),
        CapabilityMatrixSource::Unavailable(code) => format!("unavailable:{code}"),
    }
}

/// An empty resident registry renders a failed boot warm as unavailable with
/// the warm's own outcome token, and is an honest empty otherwise; a resident
/// entry is always available, whatever the warm did.
///
/// Mutation check: map the empty `Unreadable` arm to `Empty` -> the
/// `empty_after_unreadable_warm` row goes red.
#[test]
fn resident_matrix_availability_follows_the_warm_only_when_empty() {
    let rows: [(&str, bool, WarmOutcome, &str); 4] = [
        (
            "empty_after_unreadable_warm",
            false,
            WarmOutcome::Unreadable("open_failed"),
            "unavailable:unreadable",
        ),
        (
            "empty_after_restate_failed_warm",
            false,
            WarmOutcome::RestateFailed,
            "unavailable:restate_failed",
        ),
        (
            "empty_after_replayed_warm",
            false,
            WarmOutcome::Replayed,
            "empty",
        ),
        (
            "resident_after_unreadable_warm",
            true,
            WarmOutcome::Unreadable("open_failed"),
            "available",
        ),
    ];

    for (name, has_entry, outcome, expected) in rows {
        let entries = if has_entry {
            vec![resident_negative(Instant::now())]
        } else {
            Vec::new()
        };

        let source = resident_matrix(learned(entries), &warm(outcome));

        assert_eq!(class(&source), expected, "row {name}");
    }
}

/// A resident source carries no replay tally: nothing was replayed for the
/// report. The boot warm's tally is carried on the origin instead.
#[test]
fn a_resident_source_carries_no_replay_tally() {
    let available = resident_matrix(
        learned(vec![resident_negative(Instant::now())]),
        &warm(WarmOutcome::Replayed),
    );
    let empty = resident_matrix(learned(Vec::new()), &warm(WarmOutcome::Replayed));

    assert!(matches!(
        available,
        CapabilityMatrixSource::Available { replay: None, .. }
    ));
    assert!(matches!(
        empty,
        CapabilityMatrixSource::Empty { replay: None, .. }
    ));
}

/// The boot warm's tally renders through the same fold the report's own replay
/// uses, labelled with the warm's outcome token.
#[test]
fn the_boot_warm_renders_its_outcome_and_tally() {
    let report = WarmReport {
        outcome: WarmOutcome::Replayed,
        summary: Some(CapabilityRebuildSummary {
            replayed_negative: 2,
            skipped_owner: 1,
            ..CapabilityRebuildSummary::default()
        }),
        loaded_rows: 3,
    };

    let rendered = matrix_warm(&report);

    assert_eq!(rendered.outcome, "replayed");
    let summary = rendered.summary.expect("the warm replayed");
    assert_eq!(summary.loaded_rows, 3);
    assert_eq!(summary.replayed, 2);
    assert_eq!(summary.skipped_owner, 1);
}

/// The body of the free function `name` in `src`: from its signature to the
/// first line that closes it at column zero.
fn fn_body<'s>(src: &'s str, name: &str) -> &'s str {
    let start = src
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("fn {name} not found"));
    let rest = &src[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("fn {name} has no closing brace"));
    &rest[..end]
}

/// The served gather never loads the overlay file, never parses the config
/// file, and never replays the ledger: those calls live only behind the
/// disk-only layer function, which the served arm does not reach.
///
/// The positive control proves the scan sees the calls where they are.
#[test]
fn the_served_path_names_no_disk_load_or_replay() {
    let disk_only = [
        concat!("load_overlay", "_default"),
        concat!("parse_config", "_only"),
        concat!("rebuild_capabilities", "_into"),
        concat!("gather_capability", "_matrix"),
    ];
    let gather = include_str!("gather.rs");
    let served = include_str!("served.rs");
    let shared_body = fn_body(gather, "gather_context_no_network");
    let disk_body = fn_body(gather, "disk_layers");

    for token in disk_only {
        assert!(
            !served.contains(token),
            "the served layer module names `{token}`"
        );
        assert!(
            !shared_body.contains(token),
            "the shared gather body names `{token}`; it belongs behind the disk layer"
        );
    }
    assert!(
        shared_body.contains("GatherSources::Served(inputs) => served_layers(inputs)"),
        "the served arm takes the served layers"
    );
    for token in &disk_only[..2] {
        assert!(
            disk_body.contains(token),
            "positive control: the disk layer still names `{token}`"
        );
    }
}

/// The version finding of a served gather describes the config the daemon
/// accepted, not the file on disk: a later edit the daemon rejected (here,
/// broken TOML, which the raw preflight reads as legacy v1) must not turn
/// into a passing "v1" line. The disk gather of the same file still fails.
///
/// Mutation check: make `section_version` preflight `raw_config` even when an
/// accepted version is set -> the served assertion goes red.
#[tokio::test]
#[serial_test::serial]
async fn a_served_version_finding_reads_the_accepted_config_not_the_disk_file() {
    use routectl_router::{CURRENT_CONFIG_VERSION, Finding, Status};
    use routectl_testkit::ScopedEnv;

    use crate::commands::doctor::{build_report_no_network, gather_context_no_network};

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(&config_path, b"version = [unterminated\n").unwrap();
    let accepted = Config {
        version: CURRENT_CONFIG_VERSION,
        ..Config::default()
    };
    let inputs = ServedInputs::new(
        Arc::new(accepted),
        CatalogOverlay::default(),
        learned(Vec::new()),
        warm(WarmOutcome::Replayed),
    );
    let version_finding = |findings: Vec<Finding>| {
        findings
            .into_iter()
            .find(|f| f.section == "version")
            .expect("a version finding")
    };

    let served = version_finding(
        build_report_no_network(
            &gather_context_no_network(&config_path, GatherSources::Served(Box::new(inputs))).await,
        )
        .findings,
    );
    let disk = version_finding(
        build_report_no_network(
            &gather_context_no_network(&config_path, GatherSources::Disk).await,
        )
        .findings,
    );

    assert_eq!(served.status, Status::Pass, "{served:?}");
    assert!(
        served
            .detail
            .starts_with(&format!("config schema v{CURRENT_CONFIG_VERSION};")),
        "{served:?}"
    );
    assert_eq!(disk.status, Status::Fail, "{disk:?}");
    assert!(
        disk.detail.starts_with("config could not be loaded"),
        "{disk:?}"
    );
}

/// An `unreadable` boot warm carries its failure class next to the coarse
/// outcome token; no other outcome carries one.
///
/// Mutation check: drop the class (`None`) in `matrix_warm` -> the
/// `unreadable` row goes red.
#[test]
fn only_an_unreadable_warm_carries_a_failure_class() {
    let rows: [(&str, WarmOutcome, Option<&str>); 3] = [
        (
            "unreadable",
            WarmOutcome::Unreadable("open_failed"),
            Some("open_failed"),
        ),
        ("restate_failed", WarmOutcome::RestateFailed, None),
        ("replayed", WarmOutcome::Replayed, None),
    ];

    for (name, outcome, expected) in rows {
        let rendered = matrix_warm(&warm(outcome));

        assert_eq!(rendered.outcome, name, "row {name}");
        assert_eq!(rendered.class.as_deref(), expected, "row {name}");
    }
}
