//! Real-binary checks of the config folder the CLI reads: a config left only
//! in the old macOS folder (`$HOME/Library/Application Support/routectl`)
//! stops every command before dispatch, naming both paths, and an explicit
//! `--config` gets past the check. Also pins that the default lookup follows
//! the shared core rule, where an empty `XDG_CONFIG_HOME` counts as unset.
//!
//! Every spawn removes `XDG_CONFIG_HOME` (the workspace cargo config forces
//! it) and `ROUTECTL_CONFIG`, and points `HOME` and the working directory at a
//! tempdir, so no real config folder is read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
        std::fs::write(&old, "version = 5\n").unwrap();
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_routectl"))
            .args(args)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ROUTECTL_CONFIG")
            .env("HOME", self.path())
            .current_dir(self.path())
            .stdin(Stdio::null())
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
    std::fs::write(&config, "version = 5\n[server]\nprt = 8080\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(["config", "check"])
        .env("XDG_CONFIG_HOME", "")
        .env_remove("ROUTECTL_CONFIG")
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
