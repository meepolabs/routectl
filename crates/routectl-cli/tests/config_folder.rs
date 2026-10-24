//! Real-binary checks of the config folder the CLI reads: a config left only
//! in an old folder (`$HOME/Library/Application Support/routectl` or
//! `$APPDATA/routectl`) stops every command before dispatch, naming both
//! paths, and an explicit `--config` or `ROUTECTL_CONFIG` gets past the
//! check. Also pins that the default lookup follows the shared core rule,
//! where an empty `XDG_CONFIG_HOME` counts as unset, and that `doctor` lists
//! files left in the old folders, and that a running `serve` lists them on
//! its served doctor panel.
//!
//! Every spawn removes `XDG_CONFIG_HOME` (the workspace cargo config forces
//! it) and `ROUTECTL_CONFIG` (one row then sets it to a path inside the
//! tempdir), points `HOME` and the working directory at a
//! tempdir, and either removes `APPDATA` or points it inside that tempdir, so
//! no real config folder is read or written.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use routectl_router::CURRENT_CONFIG_VERSION;
use serde_json::Value;

/// Give-up bound for the daemon to begin serving.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// Per-request socket timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

/// How an explicit-config row names the stranded config.
enum ExplicitConfig {
    Flag,
    Env,
}

#[test]
fn an_explicit_config_gets_past_the_old_folder_check() {
    let rows = [
        ("--config", ExplicitConfig::Flag),
        ("ROUTECTL_CONFIG", ExplicitConfig::Env),
    ];
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(name, how)| explicit_row_failure(how).map(|why| format!("{name}: {why}")))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Why `config check` with the stranded config named explicitly did not pass
/// cleanly, or `None` when it did.
fn explicit_row_failure(how: &ExplicitConfig) -> Option<String> {
    let home = Home::with_stranded_config();
    let old = home.old_config();
    let mut command = match how {
        ExplicitConfig::Flag => {
            home.command(&["--config", old.to_str().unwrap(), "config", "check"])
        }
        ExplicitConfig::Env => {
            let mut command = home.command(&["config", "check"]);
            command.env("ROUTECTL_CONFIG", &old);
            command
        }
    };

    let out = command
        .env_remove("APPDATA")
        .output()
        .expect("spawn routectl binary");

    let stderr = stderr_of(&out);
    let passed =
        out.status.success() && !stderr.contains("moved nothing") && !stderr.contains("old folder");
    (!passed).then(|| format!("status {}: {stderr}", out.status))
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

/// A `routectl serve` child, killed and reaped by its own handle on drop so a
/// failing assertion cannot leak a daemon holding the port.
struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn await_serving(&mut self) {
        let until = Instant::now() + READY_DEADLINE;
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("the daemon exited during startup with {status}: {stderr}");
            }
            if matches!(try_get(self.port, "/health"), Some((200, _))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the daemon never began serving on port {}", self.port);
    }
}

/// One HTTP/1.0 GET over a plain socket: `(status, body)`, or `None` when the
/// daemon is not answering yet.
fn try_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    socket.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    socket.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map_or_else(String::new, |(_, body)| body.to_string());
    Some((status, body))
}

/// A loopback port the OS handed out as free a moment ago.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// The served doctor panel lists the old folders `serve` read at start.
/// Mutation check: drop the `.with_legacy_config_dirs(...)` call where the
/// serve path builds its status state -> red here.
#[test]
fn served_doctor_names_a_file_left_in_the_appdata_folder() {
    let home = Home::with_new_config_and_old_files(&[]);
    let appdata_dir = home.appdata().join("routectl");
    std::fs::create_dir_all(&appdata_dir).unwrap();
    std::fs::write(appdata_dir.join("usage.db"), b"x").unwrap();
    let port = free_port();
    let child = home
        .command(&[
            "--config",
            home.new_config().to_str().unwrap(),
            "serve",
            "--port",
            &port.to_string(),
        ])
        .env("APPDATA", home.appdata())
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn routectl serve");
    let mut daemon = Daemon { child, port };
    daemon.await_serving();

    let (status, body) = try_get(port, "/status/doctor").expect("/status/doctor answers");

    assert_eq!(status, 200, "{body}");
    let panel: Value = serde_json::from_str(&body).expect("doctor panel is JSON");
    assert!(panel["unavailable"].is_null(), "{panel}");
    let findings = panel["data"]["report"]["findings"]
        .as_array()
        .expect("findings array");
    let legacy: Vec<&Value> = findings
        .iter()
        .filter(|f| f["name"] == "legacy folder")
        .collect();
    assert_eq!(legacy.len(), 1, "{findings:?}");
    let detail = legacy[0]["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("%APPDATA%\\routectl still holds usage.db"),
        "{detail}"
    );
    assert!(
        !body.contains(&*home.path().to_string_lossy()),
        "no absolute path anywhere in the panel: {body}"
    );
}
