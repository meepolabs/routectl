//! The `config` section's line naming catalog files routectl no longer reads,
//! and the gather that looks for them in the config folder.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use routectl_testkit::ScopedEnv;

use crate::commands::pricing::PriceSnapshot;
use crate::server::capability_rebuild::WarmReport;

use super::gather::gather_leftover_catalog_files;
use super::render::render_human;
use super::sections::section_config;
use super::tests::{ctx, current_version_stamp};
use super::*;

const ALL_FOUR: [&str; 4] = [
    "catalog_overlay.json",
    "catalog_overlay.json.lock",
    "catalog_state.json",
    "catalog_import_state.json",
];

/// Files that live in the config folder and are still read. A gather that
/// listed every entry would name these.
const STILL_READ: [&str; 3] = ["config.toml", "usage.db", "prices.json"];

fn plant(dir: &Path, names: &[&str]) {
    std::fs::create_dir_all(dir).expect("create folder");
    for name in names {
        std::fs::write(dir.join(name), b"x").expect("plant file");
    }
}

fn context_with(present: &[&'static str]) -> DoctorContext {
    DoctorContext {
        leftover_catalog_files: present.to_vec(),
        ..ctx(
            Config::default(),
            Some(&current_version_stamp()),
            Vec::new(),
            Vec::new(),
        )
    }
}

fn leftover_findings(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.name == "leftover catalog files")
        .collect()
}

/// The gather names exactly the listed files present, in list order, and
/// nothing else the folder holds. Each single-file row is its own source; the
/// all-four row is the positive control that every name is looked for.
///
/// Mutation check: drop the presence filter in
/// `gather_leftover_catalog_files` -> the `none` row goes red.
#[test]
fn gather_lists_exactly_the_present_leftover_files() {
    let rows: [(&str, &[&str]); 7] = [
        ("none", &[]),
        ("overlay", &["catalog_overlay.json"]),
        ("overlay lock", &["catalog_overlay.json.lock"]),
        ("state", &["catalog_state.json"]),
        ("import state", &["catalog_import_state.json"]),
        (
            "two, planted out of order",
            &["catalog_import_state.json", "catalog_overlay.json"],
        ),
        ("all four", &ALL_FOUR),
    ];

    for (name, planted) in rows {
        // Arrange
        let tmp = tempfile::tempdir().expect("tempdir");
        plant(tmp.path(), &STILL_READ);
        plant(tmp.path(), planted);

        // Act
        let found = gather_leftover_catalog_files(tmp.path());

        // Assert
        let expected: Vec<&str> = ALL_FOUR
            .into_iter()
            .filter(|file| planted.contains(file))
            .collect();
        assert_eq!(found, expected, "row {name}");
    }
}

/// A missing config folder holds no leftovers.
#[test]
fn gather_of_a_missing_folder_finds_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let found = gather_leftover_catalog_files(&tmp.path().join("absent"));

    assert!(found.is_empty(), "{found:?}");
}

/// No leftover files, no line; otherwise one Pass line in the `config`
/// section listing exactly the present files and pointing to the refresh
/// command.
///
/// Mutation check: drop the leftover finding from `section_config` -> the
/// `two` and `all four` rows go red.
#[test]
fn section_config_renders_one_pass_line_naming_the_present_files() {
    let rows: [(&str, &[&'static str], Option<&str>); 3] = [
        ("none", &[], None),
        (
            "two",
            &["catalog_overlay.json", "catalog_state.json"],
            Some(
                "the config folder still holds catalog_overlay.json, catalog_state.json; \
                 routectl no longer reads them",
            ),
        ),
        (
            "all four",
            &ALL_FOUR,
            Some(
                "the config folder still holds catalog_overlay.json, \
                 catalog_overlay.json.lock, catalog_state.json, catalog_import_state.json; \
                 routectl no longer reads them",
            ),
        ),
    ];

    for (name, present, expected_detail) in rows {
        let findings = section_config(&context_with(present));

        let leftover = leftover_findings(&findings);
        match expected_detail {
            None => assert!(leftover.is_empty(), "row {name}: {findings:?}"),
            Some(detail) => {
                assert_eq!(leftover.len(), 1, "row {name}: {findings:?}");
                let finding = leftover[0];
                assert_eq!(finding.section, "config", "row {name}");
                assert_eq!(finding.status, Status::Pass, "row {name}");
                assert_eq!(finding.detail, detail, "row {name}");
                assert!(
                    finding
                        .remediation
                        .as_deref()
                        .is_some_and(|r| r.contains("`routectl prices refresh`")),
                    "row {name}: {finding:?}"
                );
            }
        }
    }
}

/// The line is advisory: a report carrying it exits exactly as the same
/// report without it does, on a passing report and on a failing one (a config
/// newer than this binary).
///
/// Mutation check: make the leftover finding `Status::Fail` -> the `passing`
/// row goes red.
#[test]
fn leftover_line_never_changes_the_exit_code() {
    let rows: [(&str, String, i32); 2] = [
        ("passing", current_version_stamp(), 0),
        ("failing", "version = 999999\n".to_string(), 1),
    ];

    for (name, raw_config, expected) in rows {
        let report = |present: &[&'static str]| {
            build_report(&DoctorContext {
                raw_config: Some(raw_config.clone()),
                ..context_with(present)
            })
        };
        let with = report(&ALL_FOUR);
        let without = report(&[]);

        assert_eq!(
            leftover_findings(&with.findings).len(),
            1,
            "row {name}: premise, the report carries the line"
        );
        assert_eq!(overall_exit(&with.findings), expected, "row {name}");
        assert_eq!(overall_exit(&without.findings), expected, "row {name}");
    }
}

/// The line renders on the human report under the config section.
#[test]
fn human_report_carries_the_leftover_line() {
    let lines = render_human(&build_report(&context_with(&["catalog_state.json"])));

    assert!(
        lines.iter().any(|l| l
            == "  PASS leftover catalog files: the config folder still holds \
                catalog_state.json; routectl no longer reads them"),
        "{lines:?}"
    );
}

/// Both gathers look in the config folder the config-dir rule names, so the
/// CLI and `/status/doctor` report the same files.
///
/// Mutation check: gather leftovers from an empty list in
/// `gather_context_no_network` -> both rows go red.
#[tokio::test]
#[serial_test::serial]
async fn both_gathers_look_in_the_config_folder() {
    for served in [false, true] {
        // Arrange
        let tmp = tempfile::tempdir().expect("tempdir");
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        plant(
            &cfg_dir,
            &["catalog_overlay.json.lock", "catalog_state.json"],
        );
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, current_version_stamp()).expect("write config");
        let sources = if served {
            GatherSources::Served(Box::new(ServedInputs::new(
                Arc::new(Config::default()),
                ResidentLearned {
                    entries: Vec::new(),
                    seed_clears: Vec::new(),
                    now: Instant::now(),
                    now_ms: 1_000,
                },
                WarmReport::not_run(),
                super::ServedPrices::Pinned(PriceSnapshot::default()),
                Config::default().usage.db_path,
            )))
        } else {
            GatherSources::Disk
        };

        // Act
        let context = gather_context_no_network(&config_path, sources, &[]).await;

        // Assert
        assert_eq!(
            context.leftover_catalog_files,
            vec!["catalog_overlay.json.lock", "catalog_state.json"],
            "served={served}"
        );
    }
}

/// A leftover overlay file is not read: one that no longer parses leaves the
/// version finding passing and names the file only as a leftover. The
/// premise row proves the fixture is not even valid JSON.
#[tokio::test]
#[serial_test::serial]
async fn a_corrupt_leftover_overlay_does_not_fail_the_disk_gather() {
    // Arrange
    let tmp = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir");
    let overlay = cfg_dir.join("catalog_overlay.json");
    std::fs::write(&overlay, b"{ not json").expect("write overlay");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&overlay).expect("read"))
            .is_err(),
        "premise: the file is not valid JSON"
    );
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(&config_path, current_version_stamp()).expect("write config");

    // Act
    let context = gather_context_no_network(&config_path, GatherSources::Disk, &[]).await;
    let report = build_report_no_network(&context);

    // Assert
    assert_eq!(context.config_load_error, None);
    let version = report
        .findings
        .iter()
        .find(|f| f.section == "version")
        .expect("a version finding");
    assert_eq!(version.status, Status::Pass, "{version:?}");
    assert_eq!(context.leftover_catalog_files, vec!["catalog_overlay.json"]);
    assert_eq!(overall_exit(&report.findings), 0, "{:?}", report.findings);
}
