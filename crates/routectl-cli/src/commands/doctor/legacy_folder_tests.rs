//! The `config` section's finding for files left in the old config folder,
//! and the gather that lists that folder.

use std::io::ErrorKind;
use std::path::Path;

use crate::config_folder::{
    LEGACY_APPDATA_DISPLAY, LEGACY_LIBRARY_DISPLAY, LegacyConfigDir, legacy_config_dir,
};

use super::gather::gather_legacy_folder;
use super::render::render_human;
use super::sections::section_config;
use super::tests::{ctx, current_version_stamp, token_record};
use super::*;

fn context_with(folder: LegacyFolder) -> DoctorContext {
    DoctorContext {
        legacy_folder: folder,
        ..ctx(
            Config::default(),
            Some(&current_version_stamp()),
            Vec::new(),
            Vec::new(),
        )
    }
}

fn entries(names: &[&str], more: usize) -> LegacyFolder {
    LegacyFolder::Entries {
        display: LEGACY_LIBRARY_DISPLAY,
        names: names.iter().map(|name| (*name).to_string()).collect(),
        more,
        has_config: names.contains(&"config.toml"),
    }
}

fn library_folder(home: &Path) -> LegacyConfigDir {
    legacy_config_dir(Some(home.as_os_str()), None).expect("home is set")
}

fn legacy_findings(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.name == "legacy folder")
        .collect()
}

fn the_legacy_finding(folder: LegacyFolder) -> Finding {
    let findings = section_config(&context_with(folder));
    let legacy = legacy_findings(&findings);
    assert_eq!(legacy.len(), 1, "exactly one legacy finding: {findings:?}");
    legacy[0].clone()
}

/// Mutation check: drop the `legacy_folder_finding` call from
/// `section_config` -> red here.
#[test]
fn entries_render_one_config_warn_naming_the_folder_by_tilde() {
    let finding = the_legacy_finding(entries(&["catalog_state.json", "usage.db"], 0));

    assert_eq!(finding.section, "config");
    assert_eq!(finding.status, Status::Warn);
    assert!(
        finding.detail.contains(LEGACY_LIBRARY_DISPLAY),
        "{}",
        finding.detail
    );
    assert!(
        finding.detail.contains("catalog_state.json, usage.db"),
        "{}",
        finding.detail
    );
    assert!(
        finding.detail.contains("routectl does not read them"),
        "{}",
        finding.detail
    );
    assert!(
        !finding.detail.contains("not the config in use"),
        "no config.toml, no in-use clause: {}",
        finding.detail
    );
    let remediation = finding.remediation.expect("remediation");
    assert!(
        remediation.contains(LEGACY_LIBRARY_DISPLAY),
        "{remediation}"
    );
}

#[test]
fn entries_with_config_toml_say_that_copy_is_not_in_use() {
    let finding = the_legacy_finding(entries(&["config.toml", "usage.db"], 0));

    assert!(
        finding
            .detail
            .contains("its config.toml is not the config in use"),
        "{}",
        finding.detail
    );
}

#[test]
fn entries_past_the_cap_list_eight_names_and_count_the_rest() {
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let finding = the_legacy_finding(entries(&names, 3));

    assert!(
        finding
            .detail
            .contains("a, b, c, d, e, f, g, h and 3 more;"),
        "{}",
        finding.detail
    );
}

#[test]
fn an_uninspectable_folder_is_its_own_warn() {
    let finding = the_legacy_finding(LegacyFolder::InspectError {
        display: LEGACY_LIBRARY_DISPLAY,
        kind: ErrorKind::PermissionDenied,
    });

    assert_eq!(finding.status, Status::Warn);
    assert!(
        finding.detail.contains("could not list"),
        "{}",
        finding.detail
    );
    assert!(
        finding.detail.contains(LEGACY_LIBRARY_DISPLAY),
        "{}",
        finding.detail
    );
}

#[test]
fn an_absent_folder_renders_no_legacy_finding() {
    let findings = section_config(&context_with(LegacyFolder::Absent));

    assert!(legacy_findings(&findings).is_empty(), "{findings:?}");
    assert!(
        !findings.is_empty(),
        "positive control: the config section still renders its own findings"
    );
}

/// A config that does not parse still shows the old folder: the section's
/// early return for a broken config must not hide it.
#[test]
fn a_broken_config_still_shows_the_legacy_finding() {
    let context = DoctorContext {
        config_load_error: Some("config could not be parsed".to_string()),
        ..context_with(entries(&["config.toml"], 0))
    };

    let findings = section_config(&context);

    assert_eq!(legacy_findings(&findings).len(), 1, "{findings:?}");
}

/// Mutation check: list the raw basename in `legacy_entries_detail` instead
/// of its sanitized form -> red here.
#[test]
fn a_hostile_basename_renders_without_control_bytes() {
    let hostile = "evil\n\u{1b}[31m  PASS forged: all clear";
    assert!(
        hostile.contains('\n') && hostile.contains("\u{1b}["),
        "positive control: the fixture carries the bytes being filtered"
    );
    let context = context_with(entries(&[hostile], 0));

    let lines = render_human(&build_report(&context));

    let line = lines
        .iter()
        .find(|line| line.contains("legacy folder"))
        .expect("the legacy finding renders");
    assert!(line.contains("evil"), "{line}");
    assert!(!line.contains('\n') && !line.contains('\u{1b}'), "{line:?}");
    assert!(
        !lines.iter().any(|line| line.starts_with("  PASS forged")),
        "a basename forged a finding line: {lines:?}"
    );
}

#[test]
fn a_report_whose_only_issue_is_the_legacy_warn_exits_zero() {
    let logged_in = DoctorContext {
        seats: vec![("anthropic".to_string(), token_record(u64::MAX))],
        ..context_with(entries(&["config.toml"], 0))
    };
    let report = build_report(&logged_in);

    let issues: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|f| f.status != Status::Pass)
        .collect();
    assert_eq!(issues.len(), 1, "fixture must carry one issue: {issues:?}");
    assert_eq!(issues[0].name, "legacy folder");
    assert_eq!(issues[0].status, Status::Warn);
    assert_eq!(overall_exit(&report.findings), 0);
}

fn plant(dir: &Path, names: &[&str]) {
    std::fs::create_dir_all(dir).expect("create folder");
    for name in names {
        std::fs::write(dir.join(name), b"x").expect("plant file");
    }
}

/// Mutation check: make `gather_legacy_folder` return `LegacyFolder::Absent`
/// unconditionally -> red here.
#[test]
fn gather_lists_the_old_folder_under_home_sorted() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    plant(&legacy.path, &["usage.db", "config.toml"]);
    let in_use = home.path().join(".config/routectl/config.toml");

    let folder = gather_legacy_folder(Some(&legacy), &in_use);

    assert_eq!(folder, entries(&["config.toml", "usage.db"], 0));
    let finding = the_legacy_finding(folder);
    let home_text = home.path().to_string_lossy().into_owned();
    assert!(
        !finding.detail.contains(&home_text),
        "no absolute home path in the detail: {}",
        finding.detail
    );
    assert!(
        !finding
            .remediation
            .as_deref()
            .unwrap_or_default()
            .contains(&home_text),
        "no absolute home path in the remediation"
    );
}

#[test]
fn gather_with_no_old_folder_is_absent() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    let in_use = home.path().join(".config/routectl/config.toml");

    assert_eq!(
        gather_legacy_folder(Some(&legacy), &in_use),
        LegacyFolder::Absent
    );
    assert_eq!(gather_legacy_folder(None, &in_use), LegacyFolder::Absent);
}

#[test]
fn gather_caps_names_at_eight_and_counts_the_rest() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    let names = ["j", "i", "h", "g", "f", "e", "d", "c", "b", "a", "k"];
    plant(&legacy.path, &names);

    let folder = gather_legacy_folder(Some(&legacy), &home.path().join("in-use.toml"));

    assert_eq!(
        folder,
        entries(&["a", "b", "c", "d", "e", "f", "g", "h"], 3)
    );
}

/// `--config` naming the old folder's config makes that folder the one in
/// use, so nothing there is residue.
#[test]
fn gather_treats_an_old_folder_holding_the_config_in_use_as_absent() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    plant(&legacy.path, &["config.toml", "usage.db"]);

    let folder = gather_legacy_folder(Some(&legacy), &legacy.path.join("config.toml"));

    assert_eq!(folder, LegacyFolder::Absent);
}

/// The old folder's path is a regular file, so listing it fails for every
/// uid, root included.
#[test]
fn gather_reports_an_unlistable_folder_as_an_inspect_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    std::fs::create_dir_all(legacy.path.parent().expect("parent")).expect("create parent");
    std::fs::write(&legacy.path, b"x").expect("plant a file where the folder goes");

    let folder = gather_legacy_folder(Some(&legacy), &home.path().join("in-use.toml"));

    assert_eq!(
        folder,
        LegacyFolder::InspectError {
            display: LEGACY_LIBRARY_DISPLAY,
            kind: ErrorKind::NotADirectory,
        }
    );
}

/// Mutation check: drop the `names.is_empty()` early return in
/// `gather_legacy_folder` -> red here.
#[test]
fn gather_with_an_empty_old_folder_is_absent() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    plant(&legacy.path, &[]);
    assert!(legacy.path.is_dir(), "positive control: the folder exists");

    let folder = gather_legacy_folder(Some(&legacy), &home.path().join("in-use.toml"));

    assert_eq!(folder, LegacyFolder::Absent);
}

/// `.config.tmp.*` sorts before `config.toml`, so nine of them push it past
/// the cap. Mutation check: compute `has_config` after the names are
/// truncated -> red here.
#[test]
fn gather_keeps_a_config_toml_pushed_past_the_cap() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    let temps: Vec<String> = (0..9).map(|n| format!(".config.tmp.{n}")).collect();
    let mut planted: Vec<&str> = temps.iter().map(String::as_str).collect();
    planted.push("config.toml");
    plant(&legacy.path, &planted);

    let folder = gather_legacy_folder(Some(&legacy), &home.path().join("in-use.toml"));

    let LegacyFolder::Entries {
        names,
        more,
        has_config,
        ..
    } = &folder
    else {
        panic!("expected entries: {folder:?}");
    };
    assert!(
        !names.iter().any(|name| name == "config.toml"),
        "positive control: config.toml is past the cap: {names:?}"
    );
    assert_eq!(*more, 2);
    assert!(*has_config);
    let finding = the_legacy_finding(folder);
    assert!(
        finding
            .detail
            .contains("its config.toml is not the config in use"),
        "{}",
        finding.detail
    );
}

/// The old Windows folder is named by its variable in the finding, never by
/// the macOS name and never by its absolute path. Mutation check: render a
/// fixed folder name in `legacy_folder_finding` -> red here.
#[test]
fn gather_names_an_appdata_folder_by_its_variable() {
    let home = tempfile::tempdir().expect("tempdir");
    let appdata = tempfile::tempdir().expect("tempdir");
    let legacy = legacy_config_dir(
        Some(home.path().as_os_str()),
        Some(appdata.path().as_os_str()),
    )
    .expect("appdata is set");
    plant(&legacy.path, &["usage.db"]);

    let folder = gather_legacy_folder(Some(&legacy), &home.path().join("in-use.toml"));
    let finding = the_legacy_finding(folder);

    let remediation = finding.remediation.clone().expect("remediation");
    let appdata_text = appdata.path().to_string_lossy().into_owned();
    for text in [&finding.detail, &remediation] {
        assert!(text.contains(LEGACY_APPDATA_DISPLAY), "{text}");
        assert!(!text.contains(LEGACY_LIBRARY_DISPLAY), "{text}");
        assert!(
            !text.contains(&appdata_text),
            "absolute path leaked: {text}"
        );
    }
    assert!(finding.detail.contains("usage.db"), "{}", finding.detail);
}

/// A default config that is a symlink into the old folder makes that folder
/// the one in use.
#[cfg(unix)]
#[test]
fn gather_treats_a_config_symlinked_into_the_old_folder_as_absent() {
    let home = tempfile::tempdir().expect("tempdir");
    let legacy = library_folder(home.path());
    plant(&legacy.path, &["config.toml", "usage.db"]);
    let new_dir = home.path().join(".config/routectl");
    std::fs::create_dir_all(&new_dir).expect("create new folder");
    let in_use = new_dir.join("config.toml");
    std::os::unix::fs::symlink(legacy.path.join("config.toml"), &in_use).expect("symlink");

    let folder = gather_legacy_folder(Some(&legacy), &in_use);

    assert_eq!(folder, LegacyFolder::Absent);
}
