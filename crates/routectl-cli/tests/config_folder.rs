//! Real-binary checks of the config folder the CLI reads: a config left only
//! in an old folder (`$HOME/Library/Application Support/routectl` or
//! `$APPDATA/routectl`) stops every command before dispatch, naming both
//! paths, and an explicit `--config` gets past the check. Also pins that the
//! default lookup follows the shared core rule, where an empty
//! `XDG_CONFIG_HOME` counts as unset, and that `doctor` lists files left in
//! the old folders.
//!
//! Every spawn removes `XDG_CONFIG_HOME` (the workspace cargo config forces
//! it) and `ROUTECTL_CONFIG`, points `HOME` and the working directory at a
//! tempdir, and either removes `APPDATA` or points it inside that tempdir, so
//! no real config folder is read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use routectl_router::CURRENT_CONFIG_VERSION;

struct Home {
    tmp: tempfile::TempDir,
}

impl Home {
    fn with_stranded_config() -> Self {
        let home = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        let old = home.old_config();
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, format!("version = {CURRENT_CONFIG_VERSION}\n")).unwrap();
        home
    }

    /// A valid config in the new folder, and `planted` in the old folder.
    fn with_new_config_and_old_files(planted: &[&str]) -> Self {
        let home = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        let new = home.new_config();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, format!("version = {CURRENT_CONFIG_VERSION}\n")).unwrap();
        let old_dir = home.old_config().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&old_dir).unwrap();
        for name in planted {
            std::fs::write(old_dir.join(name), b"x").unwrap();
        }
        home
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }

    fn old_config(&self) -> PathBuf {
        self.path()
            .join("Library")
            .join("Application Support")
            .join("routectl")
            .join("config.toml")
    }

    fn new_config(&self) -> PathBuf {
        self.path()
            .join(".config")
            .join("routectl")
            .join("config.toml")
    }

    /// `<home>/ad`, the `APPDATA` value [`Self::run_with_appdata`] passes.
    fn appdata(&self) -> PathBuf {
        self.path().join("ad")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_routectl"));
        command
            .args(args)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ROUTECTL_CONFIG")
            .env("HOME", self.path())
            .current_dir(self.path())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .env_remove("APPDATA")
            .output()
            .expect("spawn routectl binary")
    }

    fn run_with_appdata(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("APPDATA", self.appdata())
            .output()
            .expect("spawn routectl binary")
    }
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_config_only_in_the_old_folder_stops_every_command() {
    let rows: &[(&str, &[&str])] = &[
        ("config check", &["config", "check"]),
        ("serve", &["serve", "--port", "0"]),
        ("doctor", &["doctor"]),
        ("init", &["init", "--scaffold", "--yes"]),
    ];
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(name, args)| stranded_row_failure(args).map(|why| format!("{name}: {why}")))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Why one command did not refuse the stranded config, or `None` when it did.
fn stranded_row_failure(args: &[&str]) -> Option<String> {
    let home = Home::with_stranded_config();

    let out = home.run(args);

    let stderr = stderr_of(&out);
    let (old, new) = (home.old_config(), home.new_config());
    let refused = !out.status.success()
        && stderr.contains(&*old.to_string_lossy())
        && stderr.contains(&*new.to_string_lossy())
        && stderr.contains("routectl moved nothing");
    let mut why = Vec::new();
    if !refused {
        why.push(format!("no refusal (status {}): {stderr}", out.status));
    }
    if new.exists() {
        why.push(format!("created {}", new.display()));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

#[test]
fn an_explicit_config_gets_past_the_old_folder_check() {
    let home = Home::with_stranded_config();
    let old = home.old_config();

    let out = home.run(&["--config", old.to_str().unwrap(), "config", "check"]);

    let stderr = stderr_of(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("moved nothing"), "{stderr}");
    assert!(!stderr.contains("old folder"), "{stderr}");
}

#[test]
fn an_empty_xdg_config_home_counts_as_unset() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let config = home
        .path()
        .join(".config")
        .join("routectl")
        .join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!("version = {CURRENT_CONFIG_VERSION}\n[server]\nprt = 8080\n"),
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(["config", "check"])
        .env("XDG_CONFIG_HOME", "")
        .env_remove("ROUTECTL_CONFIG")
        .env_remove("APPDATA")
        .env("HOME", home.path())
        .current_dir(elsewhere.path())
        .stdin(Stdio::null())
        .output()
        .expect("spawn routectl binary");

    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("did you mean `port`?"),
        "the config under $HOME/.config must be the one read: {stderr}"
    );
}

/// Each legacy-folder finding `doctor --config <new config>` renders, as its
/// line joined with the `fix:` line under it.
fn doctor_legacy_lines(home: &Home) -> Vec<String> {
    legacy_lines_of(&home.run(&["doctor", "--config", home.new_config().to_str().unwrap()]))
}

fn legacy_lines_of(out: &Output) -> Vec<String> {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("[Config validation]"),
        "positive control: doctor rendered its config section: {stdout}\n{}",
        stderr_of(out)
    );
    let output = format!("{stdout}{}", stderr_of(out));
    let lines: Vec<&str> = output.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("legacy folder"))
        .map(|(at, line)| match lines.get(at + 1) {
            Some(next) if next.trim_start().starts_with("fix:") => format!("{line}\n{next}"),
            _ => (*line).to_owned(),
        })
        .collect()
}

/// Mutation check: replace the legacy gather in the doctor context gather
/// with `LegacyFolder::Absent` -> red here.
#[test]
fn doctor_lists_a_file_left_in_the_old_folder_by_its_home_free_name() {
    let home = Home::with_new_config_and_old_files(&["usage.db"]);

    let lines = doctor_legacy_lines(&home);

    assert_eq!(lines.len(), 1, "one legacy finding: {lines:?}");
    let line = &lines[0];
    assert!(line.contains("WARN"), "{line}");
    assert!(
        line.contains("~/Library/Application Support/routectl"),
        "{line}"
    );
    assert!(line.contains("usage.db"), "{line}");
    assert!(
        line.contains("fix: move"),
        "the remediation is checked too: {line}"
    );
    assert!(
        !line.contains(&*home.path().to_string_lossy()),
        "no absolute path in the finding: {line}"
    );
}

#[test]
fn doctor_with_an_empty_old_folder_shows_no_legacy_finding() {
    let home = Home::with_new_config_and_old_files(&[]);

    let lines = doctor_legacy_lines(&home);

    assert!(lines.is_empty(), "{lines:?}");
}

/// The refusal reads `APPDATA` itself: a config only in `$APPDATA/routectl`
/// stops the command, naming that file. Mutation check: change the
/// `"APPDATA"` literal in `legacy_config_dirs_from_env` -> red here.
#[test]
fn a_config_only_in_the_appdata_folder_stops_the_command() {
    let home = Home {
        tmp: tempfile::tempdir().unwrap(),
    };
    let old = home.appdata().join("routectl").join("config.toml");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, format!("version = {CURRENT_CONFIG_VERSION}\n")).unwrap();

    let out = home.run_with_appdata(&["config", "check"]);

    let stderr = stderr_of(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains(&*old.to_string_lossy()), "{stderr}");
    assert!(
        stderr.contains(&*home.new_config().to_string_lossy()),
        "{stderr}"
    );
    assert!(!home.new_config().exists(), "nothing was created");
}

/// `doctor` names a file left under `$APPDATA/routectl` by its variable.
/// Mutation check: change the `"APPDATA"` literal in
/// `legacy_config_dirs_from_env` -> red here.
#[test]
fn doctor_names_a_file_left_in_the_appdata_folder_by_its_variable() {
    let home = Home::with_new_config_and_old_files(&[]);
    let appdata_dir = home.appdata().join("routectl");
    std::fs::create_dir_all(&appdata_dir).unwrap();
    std::fs::write(appdata_dir.join("usage.db"), b"x").unwrap();

    let out = home.run_with_appdata(&["doctor", "--config", home.new_config().to_str().unwrap()]);
    let lines = legacy_lines_of(&out);

    assert_eq!(lines.len(), 1, "one legacy finding: {lines:?}");
    let line = &lines[0];
    assert!(
        line.contains("%APPDATA%\\routectl still holds usage.db"),
        "{line}"
    );
    assert!(
        !line.contains(&*home.path().to_string_lossy()),
        "no absolute path in the finding: {line}"
    );
}
