//! Real-binary checks that a path defaulting into the config folder is
//! refused by key, where it is consumed, when no folder resolves.
//!
//! Every spawn removes `HOME`, `XDG_CONFIG_HOME` (the workspace cargo config
//! forces it) and `ROUTECTL_CONFIG`, names its config with `--config`, and
//! runs in a fresh working directory that each test asserts stays empty, so
//! a relative fallback would show up as a file there.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use routectl_router::CURRENT_CONFIG_VERSION;

const BIN: &str = env!("CARGO_BIN_EXE_routectl");

const READY_DEADLINE: Duration = Duration::from_secs(30);

/// A config file outside the working directory, and an empty working
/// directory.
struct Fixture {
    config_dir: tempfile::TempDir,
    cwd: tempfile::TempDir,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let fixture = Self {
            config_dir: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        };
        std::fs::write(
            fixture.config_path(),
            format!("version = {CURRENT_CONFIG_VERSION}\n{body}"),
        )
        .unwrap();
        fixture
    }

    fn config_path(&self) -> PathBuf {
        self.config_dir.path().join("config.toml")
    }

    /// An absolute path inside the config tempdir, for a key set explicitly.
    fn explicit(&self, name: &str) -> String {
        self.config_dir.path().join(name).display().to_string()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .arg("--config")
            .arg(self.config_path())
            .args(args)
            .env_remove("HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ROUTECTL_CONFIG")
            .env("RUST_LOG", "error")
            .current_dir(self.cwd.path())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("spawn routectl binary")
    }

    /// [`Self::run`] for a command that may start serving: killed after
    /// [`READY_DEADLINE`], so a daemon that should have refused fails the
    /// row instead of hanging the suite.
    fn run_bounded(&self, args: &[&str]) -> Output {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn routectl binary");
        let until = Instant::now() + READY_DEADLINE;
        while Instant::now() < until {
            if child.try_wait().expect("poll child").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        child.wait_with_output().expect("reap routectl binary")
    }

    fn cwd_entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.cwd.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
}

fn refusal(key: &str) -> String {
    format!("no config folder for {key}: set HOME, or set {key}")
}

/// Why `out` is not a refusal naming `key` that left `fixture`'s working
/// directory empty, or `None` when it is.
fn refusal_failure(fixture: &Fixture, out: &Output, key: &str) -> Option<String> {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut why = Vec::new();
    if out.status.success() || !stderr.contains(&refusal(key)) {
        why.push(format!("status {}: {stderr}", out.status));
    }
    let created = fixture.cwd_entries();
    if !created.is_empty() {
        why.push(format!("created under the cwd: {created:?}"));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

fn server_block(port: u16) -> String {
    format!("[server]\nhost = \"127.0.0.1\"\nport = {port}\n")
}

/// Mutation check: drop the start-up path check in the serve body -> red.
#[test]
fn serve_without_a_config_folder_refuses_the_empty_default_by_key() {
    let rows: &[(&str, &str, &str)] = &[
        ("no [usage] db_path", "", "usage.db_path"),
        ("[mitm] without cert_dir", "MITM", "mitm.cert_dir"),
    ];
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(name, extra, key)| {
            let port = free_port();
            let probe = Fixture::new("");
            let extra = if *extra == "MITM" {
                format!(
                    "[usage]\ndb_path = \"{}\"\n[mitm]\nlisten_port = {}\n",
                    probe.explicit("usage.db"),
                    free_port()
                )
            } else {
                String::new()
            };
            let fixture = Fixture::new(&format!("{}{extra}", server_block(port)));
            let out = fixture.run_bounded(&["serve"]);
            refusal_failure(&fixture, &out, key).map(|why| format!("{name}: {why}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A `routectl serve` child, killed and reaped on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn health_answers(port: u16) -> bool {
    use std::io::Write;
    let Ok(mut socket) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
    let request = format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if socket.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

fn await_serving(daemon: &mut Daemon, port: u16) {
    let until = Instant::now() + READY_DEADLINE;
    while Instant::now() < until {
        if let Some(status) = daemon.0.try_wait().expect("poll child") {
            let mut stderr = String::new();
            if let Some(mut pipe) = daemon.0.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("the daemon exited during startup with {status}: {stderr}");
        }
        if health_answers(port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemon never began serving on port {port}");
}

/// Positive control for the refusal rows: with both keys set the same
/// environment starts and serves.
#[test]
fn serve_without_a_config_folder_starts_when_the_keys_are_set() {
    let port = free_port();
    let probe = Fixture::new("");
    let fixture = Fixture::new(&format!(
        "{}[usage]\ndb_path = \"{}\"\n[mitm]\nlisten_port = {}\ncert_dir = \"{}\"\n",
        server_block(port),
        probe.explicit("usage.db"),
        free_port(),
        probe.explicit("certs"),
    ));
    let child = fixture
        .command(&["serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn routectl serve");
    let mut daemon = Daemon(child);

    await_serving(&mut daemon, port);

    assert!(
        fixture.cwd_entries().is_empty(),
        "{:?}",
        fixture.cwd_entries()
    );
}

/// Mutation check: return `config.usage.db_path` unchecked from the usage
/// db-path helper -> red.
#[test]
fn usage_commands_without_a_config_folder_refuse_the_empty_db_path() {
    let rows: &[(&str, &[&str])] = &[
        ("usage", &["usage", "--all"]),
        (
            "usage --opening-accuracy",
            &["usage", "--all", "--opening-accuracy"],
        ),
        ("usage downgrade", &["usage", "downgrade", "--to", "20"]),
    ];
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(name, args)| {
            let fixture = Fixture::new("");
            let out = fixture.run(args);
            refusal_failure(&fixture, &out, "usage.db_path").map(|why| format!("{name}: {why}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Positive control: `--db` names the database, so no folder is needed.
#[test]
fn usage_with_an_explicit_db_needs_no_config_folder() {
    let fixture = Fixture::new("");
    let db = fixture.explicit("absent.db");

    let out = fixture.run(&["usage", "--all", "--db", &db]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("no usage data yet"), "{stdout}");
}

#[test]
fn rc_env_without_a_config_folder_refuses_the_empty_cert_dir() {
    let fixture = Fixture::new("[mitm]\n");

    let out = fixture.run(&["rc", "env"]);

    assert_eq!(refusal_failure(&fixture, &out, "mitm.cert_dir"), None);
}

/// Mutation check: restore the relative fallback in the refresh entry point
/// -> red (the table lands under the cwd).
#[test]
fn prices_refresh_without_a_config_folder_errors_and_writes_nothing() {
    let fixture = Fixture::new("");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../routectl-router/src/prices/testdata/models_dev_slice.json");

    let out = fixture.run(&["prices", "refresh", "--from", source.to_str().unwrap()]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("no config folder for prices.json"),
        "{stderr}"
    );
    assert!(
        fixture.cwd_entries().is_empty(),
        "{:?}",
        fixture.cwd_entries()
    );
}

/// Plant `name` with `body` where the old relative fallback would have read
/// it: `.config/routectl/` under the working directory.
fn plant_under_cwd(fixture: &Fixture, name: &str, body: &[u8]) {
    let dir = fixture.cwd.path().join(".config").join("routectl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}

/// Mutation check: read the relative fallback in the default price book ->
/// red (the planted table is picked up).
#[test]
fn config_show_effective_without_a_config_folder_reads_no_refreshed_table() {
    let fixture = Fixture::new("");
    plant_under_cwd(
        &fixture,
        "prices.json",
        br#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"anthropic","models":[{"id":"m","input":1.0,"output":2.0}]}]}"#,
    );

    let out = fixture.run(&["config", "show", "--effective"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("refreshed table none"), "{stdout}");
}

/// Mutation check: scan the relative fallback for leftover catalog files in
/// the doctor gather -> red (the planted file is listed).
#[test]
fn doctor_without_a_config_folder_lists_no_leftover_catalog_files() {
    let fixture = Fixture::new("");
    plant_under_cwd(&fixture, "catalog_overlay.json", b"{}");

    let out = fixture.run(&["doctor"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[Config validation]"),
        "doctor rendered: {stdout}"
    );
    assert!(!stdout.contains("leftover catalog files"), "{stdout}");
}
