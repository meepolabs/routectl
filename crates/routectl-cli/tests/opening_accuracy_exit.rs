//! `routectl usage --opening-accuracy` driven as a real process over a temp
//! ledger: the exit status and report text for each verdict. Rows are
//! written with the producer's own label constants; the child's config root
//! is a tempdir and the ledger is passed with `--db`, so no run can reach a
//! real config, ledger or network.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use routectl_cli::handlers::opening_diagnostics::{key, terminal};
use routectl_router::CURRENT_CONFIG_VERSION;
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_routectl");

/// `n` anchored rows, `misses` of them 100% over, on the given evidence.
fn seed(path: &Path, n: usize, misses: usize, verified: bool) {
    let db = routectl_usage::open(path).expect("open ledger");
    for i in 0..n {
        let opening = if i < misses { 2_000 } else { 1_000 };
        let extra = json!({
            key::OPENING_PRESENT: true,
            key::OPENING_SOURCE: "anchor",
            key::OPENING_REASON: "anchor_hit",
            key::OPENING_PROVISIONAL: false,
            key::OPENING_INPUT: opening,
            key::TERMINAL_SOURCE: terminal::EXPLICIT_FINAL,
            key::TERMINAL_INPUT: 1_000,
            key::TERMINAL_VENDOR_VERIFIED: verified,
        });
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, provider_kind, provider, model, upstream, stream, \
                 outcome, latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 extra) VALUES (1000, 1000, ?1, 'anthropic', 'req', 'al', 'anthropic-api', \
                 'anth', 'opus', 'claude-opus-4-7', 1, 'ok', 5, 0, 1, 1, 0, ?2)",
                rusqlite::params![format!("r{i}"), extra.to_string()],
            )
            .expect("insert");
    }
}

fn run(dir: &Path, db: &Path, window: &[&str]) -> Output {
    let config = dir.join("config.toml");
    std::fs::write(&config, format!("version = {CURRENT_CONFIG_VERSION}\n")).expect("config");
    let xdg = dir.join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg");
    Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .args(["usage", "--opening-accuracy"])
        .args(window)
        .arg("--db")
        .arg(db)
        .env("XDG_CONFIG_HOME", &xdg)
        .env_remove("ROUTECTL_CONFIG")
        .stdin(Stdio::null())
        .output()
        .expect("spawn routectl")
}

fn case(n: usize, misses: usize, verified: bool) -> (Output, String) {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("usage.db");
    if n > 0 {
        seed(&db, n, misses, verified);
    } else {
        drop(routectl_usage::open(&db).expect("empty ledger"));
    }
    let out = run(dir.path(), &db, &["--all"]);
    let stdout = String::from_utf8(out.stdout.clone()).expect("utf8");
    (out, stdout)
}

fn verdict_line(stdout: &str) -> &str {
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("verdict: "))
        .collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    lines[0]
}

#[test]
fn a_clean_direct_pass_exits_zero_and_still_prints_the_provenance_notice() {
    // Act
    let (out, stdout) = case(20, 1, true);

    // Assert
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        verdict_line(&stdout).starts_with("verdict: PASS "),
        "{stdout}"
    );
    assert!(
        stdout.contains("not an authorization to deploy"),
        "{stdout}"
    );
}

#[test]
fn a_pass_on_relayed_terminals_exits_nonzero_as_backend_pending() {
    // Act
    let (out, stdout) = case(20, 1, false);

    // Assert
    assert_eq!(out.status.code(), Some(4), "{stdout}");
    assert!(
        verdict_line(&stdout).starts_with("verdict: BACKEND_PENDING "),
        "{stdout}"
    );
}

#[test]
fn a_numerical_fail_exits_nonzero() {
    // Act
    let (out, stdout) = case(20, 2, true);

    // Assert
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        verdict_line(&stdout).starts_with("verdict: FAIL "),
        "{stdout}"
    );
}

#[test]
fn an_empty_ledger_is_insufficient_and_exits_nonzero() {
    // Act
    let (out, stdout) = case(0, 0, true);

    // Assert
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        verdict_line(&stdout).starts_with("verdict: INSUFFICIENT "),
        "{stdout}"
    );
}

#[test]
fn a_missing_ledger_exits_nonzero_and_creates_nothing() {
    // Arrange
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("absent.db");

    // Act
    let out = run(dir.path(), &db, &["--all"]);

    // Assert
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stdout}");
    assert!(
        verdict_line(&stdout).starts_with("verdict: NO_LEDGER "),
        "{stdout}"
    );
    assert!(!db.exists());
}

#[test]
fn an_unreadable_ledger_or_missing_window_is_an_error_exit() {
    // Arrange
    let dir = tempfile::tempdir().expect("dir");
    let junk = dir.path().join("junk.db");
    std::fs::write(&junk, b"not a database at all, just some bytes....").expect("junk");

    // Act
    let unreadable = run(dir.path(), &junk, &["--all"]);
    let no_window = run(dir.path(), &junk, &[]);

    // Assert
    assert_eq!(unreadable.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&unreadable.stderr).contains("usage db unavailable"));
    assert_eq!(no_window.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&no_window.stderr).contains("explicit window"));
}
