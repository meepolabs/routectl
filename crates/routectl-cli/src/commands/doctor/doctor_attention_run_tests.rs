//! `routectl doctor` with the daemon's needs-attention list: the block prints
//! above the checks or one reason line does, `--json` gains the `attention`
//! member beside an unchanged report, and the exit code is the findings' alone.
//!
//! Each case runs in a child invocation of this test binary whose environment
//! (a scratch `XDG_CONFIG_HOME`, and `HTTP_PROXY` for the proxy case) is set
//! when the child starts, before any runtime exists. The parent never changes
//! its own environment, so no other test in this binary can observe it.

use std::io::{Read, Write};
use std::net::TcpListener as StdListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use routectl_router::CURRENT_CONFIG_VERSION;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::attention::Attention;
use super::daemon_attention::ATTENTION_PATH;
use super::{build_report, gather_context, overall_exit, run_within};

/// Set only in a child's environment, naming the one case it runs.
const CHILD_ENV: &str = "ROUTECTL_DOCTOR_ATTENTION_RUN_CHILD";
/// Printed by a child after its case passed, so the parent can tell a child
/// that ran the case from one whose filter selected nothing.
const CHILD_PASSED: &str = "doctor-attention-run-child-passed";
/// Read timeout for every case that is not about the timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The first check section's title line.
const FIRST_SECTION: &str = "[Provider activation]";

// --- Child-process harness ---

type Case = fn(PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Run `case` in a child of this test binary with a fresh scratch config
/// home and `extra_env`, or, inside that child, run it here.
fn in_child(test: &'static str, extra_env: &[(&str, String)], case: Case) {
    if std::env::var_os(CHILD_ENV).is_some_and(|named| named == test) {
        let xdg = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("scratch XDG set"));
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(case(xdg));
        println!("\n{CHILD_PASSED}");
        return;
    }
    let scratch = tempfile::tempdir().expect("scratch config home");
    let module = module_path!()
        .split_once("::")
        .map_or("", |(_crate, rest)| rest);
    let program = std::env::current_exe().expect("the running test binary");
    let mut command = std::process::Command::new(&program);
    command
        .args([
            &format!("{module}::{test}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, test)
        .env("XDG_CONFIG_HOME", scratch.path())
        .env_remove("ROUTECTL_CONFIG")
        .env_remove("ROUTECTL_LOG");
    for proxy in [
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(proxy);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let run = command
        .output()
        .unwrap_or_else(|why| panic!("run child {}: {why}", program.display()));
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success() && stdout.lines().any(|line| line == CHILD_PASSED),
        "the {test} child must pass and print its marker; status {}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        run.status
    );
}

// --- Fixtures ---

/// A config whose daemon address is `host:port`, with its usage ledger in
/// `dir`, plus `extra` TOML.
fn write_config(dir: &Path, host: &str, port: u16, extra: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("config dir");
    let path = dir.join("config.toml");
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\n\
         host = \"{host}\"\n\
         port = {port}\n\
         [usage]\n\
         db_path = \"{}\"\n\
         {extra}",
        dir.join("usage.db").display()
    );
    std::fs::write(&path, text).expect("write config");
    path
}

/// A model naming a provider the config does not define: a config
/// validation Fail on a config that still parses.
const FAILING_EXTRA: &str = "[models.m]\nprovider = \"nowhere\"\nupstream = \"x\"\n";

/// A config that does not load at all.
fn write_unloadable_config(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).expect("config dir");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!("version = {CURRENT_CONFIG_VERSION}\nbogus_key = true\n"),
    )
    .expect("write config");
    path
}

fn envelope(data: Value) -> Value {
    json!({
        "schema_version": 1,
        "as_of": "2026-11-20T10:00:00Z",
        "unavailable": null,
        "data": data,
    })
}

/// The list a daemon with nothing listed serves: the builder's own verdict
/// over zero items.
fn healthy_envelope() -> Value {
    let attention = Attention::from_items(Vec::new());
    envelope(json!({
        "verdict": attention.verdict,
        "counts": attention.counts,
        "coverage": {"doctor": "current"},
        "items": attention.items,
    }))
}

/// A one-item broken list whose `cause` is `cause`.
fn one_item_envelope(cause: &str) -> Value {
    envelope(json!({
        "verdict": {"severity": "broken", "text": "pool seats: no usable seat", "target": "accounts"},
        "counts": {"broken": 1, "degraded": 0, "recovered": 0, "doctor_fail": 0},
        "coverage": {"doctor": "current"},
        "items": [{
            "key": "pool_empty:seats",
            "kind": "pool_empty",
            "severity": "broken",
            "state": "active",
            "subject": "pool seats",
            "cause": cause,
            "raw_token": "pool_empty (seats)",
            "fix": null,
            "remediation": null,
        }],
    }))
}

async fn mock_serving(response: ResponseTemplate) -> MockServer {
    let daemon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ATTENTION_PATH))
        .respond_with(response)
        .mount(&daemon)
        .await;
    daemon
}

fn port_of(daemon: &MockServer) -> u16 {
    daemon.address().port()
}

/// The findings-only view of `config_path`: a gather and report with no
/// daemon read at all.
struct Baseline {
    exit: i32,
    findings: Value,
    panels: Value,
}

async fn daemon_less(config_path: &Path) -> Baseline {
    let ctx = gather_context(config_path, &[], &crate::local_time::LocalZone::load()).await;
    let report = build_report(&ctx);
    let value = serde_json::to_value(&report).expect("report serializes");
    Baseline {
        exit: overall_exit(&report.findings),
        findings: value["findings"].clone(),
        panels: value["panels"].clone(),
    }
}

async fn human(config_path: &Path, timeout: Duration) -> (String, i32) {
    let output = run_within(config_path, &[], false, timeout)
        .await
        .expect("renders");
    (output.stdout, output.exit)
}

async fn json_run(config_path: &Path) -> (String, Value, i32) {
    let output = run_within(config_path, &[], true, READ_TIMEOUT)
        .await
        .expect("serializes");
    let value = serde_json::from_str(&output.stdout).expect("stdout is JSON");
    (output.stdout, value, output.exit)
}

fn lines_with<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    text.lines().filter(|line| line.contains(needle)).collect()
}

/// A listener that accepts one connection and never answers.
fn silent_listener() -> (u16, std::thread::JoinHandle<()>) {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let holder = std::thread::spawn(move || {
        let held = listener.accept();
        std::thread::sleep(Duration::from_secs(3));
        drop(held);
    });
    (port, holder)
}

/// A port nothing listens on.
fn refused_port() -> u16 {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

// --- Exit code ---

/// What the configured address answers, per exit-table row.
enum Daemon {
    Refused,
    Silent,
    Serving(ResponseTemplate),
    NonLoopbackHost,
    UnloadableConfig,
}

struct ExitRow {
    name: &'static str,
    daemon: Daemon,
    extra: &'static str,
    /// The reason token, or `None` for a served list.
    reason: Option<&'static str>,
}

fn exit_rows() -> Vec<ExitRow> {
    let json_200 = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    let mut schema_two = healthy_envelope();
    schema_two["schema_version"] = json!(2);
    let row = |name, daemon, reason| ExitRow {
        name,
        daemon,
        extra: "",
        reason,
    };
    vec![
        row("refused", Daemon::Refused, Some("not_running")),
        row("timeout", Daemon::Silent, Some("timeout")),
        row(
            "404 from an older daemon",
            Daemon::Serving(ResponseTemplate::new(404)),
            Some("not_served"),
        ),
        row(
            "401",
            Daemon::Serving(ResponseTemplate::new(401)),
            Some("auth_refused"),
        ),
        row(
            "503",
            Daemon::Serving(ResponseTemplate::new(503)),
            Some("busy"),
        ),
        row(
            "500",
            Daemon::Serving(ResponseTemplate::new(500)),
            Some("unrecognized"),
        ),
        row(
            "302 not followed",
            Daemon::Serving(
                ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:9/"),
            ),
            Some("unrecognized"),
        ),
        row(
            "not json",
            Daemon::Serving(ResponseTemplate::new(200).set_body_string("<html>")),
            Some("unrecognized"),
        ),
        row(
            "schema_version 2",
            Daemon::Serving(json_200(schema_two)),
            Some("unrecognized"),
        ),
        row(
            "unavailable envelope",
            Daemon::Serving(json_200(json!({
                "schema_version": 1, "as_of": null, "data": null, "unavailable": "db_unavailable",
            }))),
            Some("unrecognized"),
        ),
        row(
            "non-loopback host",
            Daemon::NonLoopbackHost,
            Some("no_loopback_address"),
        ),
        row(
            "config does not load",
            Daemon::UnloadableConfig,
            Some("config_unreadable"),
        ),
        row(
            "broken verdict, no Fail",
            Daemon::Serving(json_200(crate::handlers::status::populated_attention())),
            None,
        ),
        ExitRow {
            name: "healthy verdict over a failing config",
            daemon: Daemon::Serving(json_200(healthy_envelope())),
            extra: FAILING_EXTRA,
            reason: None,
        },
    ]
}

/// Where a row's config points, and what must stay alive while it runs.
struct RowSetup {
    config_path: PathBuf,
    timeout: Duration,
    _mock: Option<MockServer>,
    _holder: Option<std::thread::JoinHandle<()>>,
}

async fn set_up(row: ExitRow, dir: &Path) -> RowSetup {
    let plain = |config_path| RowSetup {
        config_path,
        timeout: READ_TIMEOUT,
        _mock: None,
        _holder: None,
    };
    match row.daemon {
        Daemon::Refused => plain(write_config(dir, "127.0.0.1", refused_port(), row.extra)),
        Daemon::Silent => {
            let (port, holder) = silent_listener();
            RowSetup {
                timeout: Duration::from_millis(200),
                _holder: Some(holder),
                ..plain(write_config(dir, "127.0.0.1", port, row.extra))
            }
        }
        Daemon::Serving(response) => {
            let mock = mock_serving(response).await;
            let config_path = write_config(dir, "127.0.0.1", port_of(&mock), row.extra);
            RowSetup {
                _mock: Some(mock),
                ..plain(config_path)
            }
        }
        Daemon::NonLoopbackHost => plain(write_config(dir, "10.20.30.40", 9, row.extra)),
        Daemon::UnloadableConfig => plain(write_unloadable_config(dir)),
    }
}

#[test]
fn every_attention_outcome_exits_as_the_findings_alone_do() {
    in_child(
        "every_attention_outcome_exits_as_the_findings_alone_do",
        &[],
        |xdg| {
            Box::pin(async move {
                for (n, row) in exit_rows().into_iter().enumerate() {
                    // Arrange
                    let name = row.name;
                    let reason = row.reason;
                    let setup = set_up(row, &xdg.join(format!("row{n}"))).await;
                    let baseline = daemon_less(&setup.config_path).await;

                    // Act
                    let (stdout, exit) = human(&setup.config_path, setup.timeout).await;

                    // Assert
                    assert_eq!(exit, baseline.exit, "{name}: exit\n{stdout}");
                    match reason {
                        Some(token) => assert_eq!(
                            lines_with(&stdout, &format!("({token}")).len(),
                            1,
                            "{name}: one reason line\n{stdout}"
                        ),
                        None => assert!(
                            stdout.contains("  source: daemon at "),
                            "{name}: a served list\n{stdout}"
                        ),
                    }
                    match name {
                        "broken verdict, no Fail" => {
                            assert!(stdout.contains("  verdict: broken: "), "{stdout}");
                            assert_eq!(baseline.exit, 0, "{name}: no Fail finding");
                        }
                        "healthy verdict over a failing config" | "config does not load" => {
                            assert_eq!(baseline.exit, 1, "{name}: a Fail finding");
                        }
                        _ => {}
                    }
                }
            })
        },
    );
}

// --- Human output ---

#[test]
fn the_served_list_prints_above_the_checks_and_summary_stays_last() {
    in_child(
        "the_served_list_prints_above_the_checks_and_summary_stays_last",
        &[],
        |xdg| {
            Box::pin(async move {
                // Arrange
                let served = crate::handlers::status::populated_attention();
                let mock = mock_serving(ResponseTemplate::new(200).set_body_json(&served)).await;
                let config_path = write_config(&xdg, "127.0.0.1", port_of(&mock), "");

                // Act
                let (stdout, _) = human(&config_path, READ_TIMEOUT).await;

                // Assert
                let lines: Vec<&str> = stdout.lines().collect();
                let block = lines.iter().position(|l| *l == "[Needs attention]");
                let first_check = lines.iter().position(|l| *l == FIRST_SECTION);
                assert!(
                    matches!((block, first_check), (Some(b), Some(c)) if b < c),
                    "{stdout}"
                );
                assert!(
                    lines.last().is_some_and(|l| l.starts_with("summary:")),
                    "{stdout}"
                );
                let raw_lines = lines_with(&stdout, "      raw: ").len();
                let items = served["data"]["items"].as_array().expect("items").len();
                assert_eq!(raw_lines, items, "every served item printed");
            })
        },
    );
}

#[test]
fn offline_output_names_its_reason_once_and_never_a_verdict() {
    in_child(
        "offline_output_names_its_reason_once_and_never_a_verdict",
        &[],
        |xdg| {
            Box::pin(async move {
                // Arrange
                let offline = mock_serving(ResponseTemplate::new(404)).await;
                let online =
                    mock_serving(ResponseTemplate::new(200).set_body_json(healthy_envelope()))
                        .await;
                let offline_path =
                    write_config(&xdg.join("offline"), "127.0.0.1", port_of(&offline), "");
                let online_path =
                    write_config(&xdg.join("online"), "127.0.0.1", port_of(&online), "");

                // Act
                let (offline_out, _) = human(&offline_path, READ_TIMEOUT).await;
                let (online_out, _) = human(&online_path, READ_TIMEOUT).await;

                // Assert
                assert!(!offline_out.contains("All routes healthy"), "{offline_out}");
                assert!(!offline_out.contains("verdict:"), "{offline_out}");
                let reason_lines = lines_with(&offline_out, "not_served");
                assert_eq!(reason_lines.len(), 1, "{offline_out}");
                assert!(
                    reason_lines[0].contains("needs the running daemon"),
                    "{offline_out}"
                );
                assert!(
                    online_out.contains("All routes healthy"),
                    "control: {online_out}"
                );
            })
        },
    );
}

// --- JSON ---

#[test]
fn json_adds_attention_beside_an_unchanged_report() {
    in_child(
        "json_adds_attention_beside_an_unchanged_report",
        &[],
        |xdg| {
            Box::pin(async move {
                // Arrange
                let served = crate::handlers::status::populated_attention();
                let online = mock_serving(ResponseTemplate::new(200).set_body_json(&served)).await;
                let offline = mock_serving(ResponseTemplate::new(404)).await;
                for (name, port, expect_online) in [
                    ("online", port_of(&online), true),
                    ("offline", port_of(&offline), false),
                ] {
                    let config_path = write_config(&xdg.join(name), "127.0.0.1", port, "");
                    let baseline = daemon_less(&config_path).await;

                    // Act
                    let (_, value, exit) = json_run(&config_path).await;

                    // Assert
                    let keys: Vec<&str> = value
                        .as_object()
                        .expect("an object")
                        .keys()
                        .map(String::as_str)
                        .collect();
                    let mut sorted = keys.clone();
                    sorted.sort_unstable();
                    assert_eq!(
                        sorted,
                        ["attention", "findings", "panels", "schema_version"],
                        "{name}"
                    );
                    assert_eq!(value["schema_version"], json!(32), "{name}");
                    assert_eq!(value["findings"], baseline.findings, "{name}");
                    assert_eq!(value["panels"], baseline.panels, "{name}");
                    assert_eq!(exit, baseline.exit, "{name}");
                    let attention = &value["attention"];
                    if expect_online {
                        assert_eq!(attention["source"], json!("daemon"));
                        assert_eq!(attention["reason"], Value::Null);
                        assert_eq!(attention["schema_version"], json!(1));
                        assert_eq!(attention["as_of"], served["as_of"]);
                        assert_eq!(attention["data"], served["data"]);
                        assert_eq!(
                            attention["address"],
                            json!(format!("{}{ATTENTION_PATH}", online.uri()))
                        );
                    } else {
                        assert_eq!(attention["source"], json!("unavailable"));
                        assert_eq!(attention["reason"], json!("not_served"));
                        assert_eq!(attention["data"], Value::Null);
                        assert_eq!(attention["schema_version"], Value::Null);
                        assert_eq!(attention["as_of"], Value::Null);
                    }
                }
            })
        },
    );
}

#[test]
fn json_for_an_unloadable_config_reads_config_unreadable_with_no_address() {
    in_child(
        "json_for_an_unloadable_config_reads_config_unreadable_with_no_address",
        &[],
        |xdg| {
            Box::pin(async move {
                // Arrange
                let config_path = write_unloadable_config(&xdg);

                // Act
                let (_, value, exit) = json_run(&config_path).await;

                // Assert
                assert_eq!(value["attention"]["source"], json!("unavailable"));
                assert_eq!(value["attention"]["reason"], json!("config_unreadable"));
                assert_eq!(value["attention"]["address"], Value::Null);
                assert_eq!(exit, 1);
            })
        },
    );
}

#[test]
fn json_stdout_is_ascii_and_round_trips_planted_controls() {
    in_child(
        "json_stdout_is_ascii_and_round_trips_planted_controls",
        &[],
        |xdg| {
            Box::pin(async move {
                for (name, cause, escape) in [
                    ("ESC", "a\u{1b}[2Jb", "\\u001b"),
                    ("DEL", "a\u{7f}b", "\\u007f"),
                    ("C1 CSI", "a\u{9b}b", "\\u009b"),
                    ("bidi override", "a\u{202e}b", "\\u202e"),
                    ("line separator", "a\u{2028}b", "\\u2028"),
                    ("astral", "a\u{1f600}b", "\\ud83d\\ude00"),
                    (
                        "all together",
                        "a\u{1b}[2J\u{9b}\u{202e}b",
                        "\\u001b[2J\\u009b\\u202e",
                    ),
                ] {
                    // Arrange
                    let mock = mock_serving(
                        ResponseTemplate::new(200).set_body_json(one_item_envelope(cause)),
                    )
                    .await;
                    let config_path = write_config(
                        &xdg.join(name.replace(' ', "_")),
                        "127.0.0.1",
                        port_of(&mock),
                        "",
                    );

                    // Act
                    let (stdout, value, _) = json_run(&config_path).await;

                    // Assert
                    assert!(
                        stdout.bytes().all(|b| b < 0x7f),
                        "{name}: stdout must be printable ASCII"
                    );
                    assert!(stdout.contains(escape), "{name}: {escape} in stdout");
                    assert_eq!(
                        value["attention"]["data"]["items"][0]["cause"],
                        json!(cause),
                        "{name}: round trip"
                    );
                }
            })
        },
    );
}

// --- Against a real daemon ---

#[test]
fn a_hermetic_daemon_lists_the_same_items_doctor_prints() {
    in_child(
        "a_hermetic_daemon_lists_the_same_items_doctor_prints",
        &[],
        |xdg| Box::pin(hermetic_daemon_case(xdg)),
    );
}

/// One seat pooled behind a model, with no credential stored: the pool has
/// no usable account.
const EMPTY_POOL_EXTRA: &str = "[providers.seat-a]\n\
     format = \"anthropic-messages\"\n\
     base_url = \"https://api.anthropic.com\"\n\
     credential = \"oauth://anthropic#seat-a\"\n\
     auth = \"bearer\"\n\
     [pools.alpha]\n\
     members = [\"seat-a\"]\n\
     [models.opus]\n\
     provider = \"alpha\"\n\
     upstream = \"claude-opus-4-7\"\n";

async fn hermetic_daemon_case(xdg: PathBuf) {
    // Arrange
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let config_path = write_config(&xdg.join("daemon"), "127.0.0.1", port, EMPTY_POOL_EXTRA);
    let config = crate::server::parse_config_only(&config_path).expect("config parses");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    let daemon = tokio::spawn(crate::server::serve_on_listener_with_secrets(
        Arc::new(config),
        listener,
        None,
        Some(secrets),
    ));
    let base = format!("http://127.0.0.1:{port}");
    await_health(&base).await;

    // Act
    let (stdout, _) = human(&config_path, READ_TIMEOUT).await;
    let (_, value, _) = json_run(&config_path).await;
    let served: Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .get(format!("{base}{ATTENTION_PATH}"))
        .send()
        .await
        .expect("the daemon answers")
        .json()
        .await
        .expect("JSON");

    // Assert
    let served_items = served["data"]["items"].as_array().expect("served items");
    assert!(
        !served_items.is_empty(),
        "control: the daemon lists something"
    );
    let field = |items: &[Value], name: &str| -> Vec<String> {
        items
            .iter()
            .map(|item| item[name].as_str().expect("a string").to_string())
            .collect()
    };
    let printed_raw: Vec<String> = lines_with(&stdout, "      raw: ")
        .iter()
        .map(|line| line.trim_start().trim_start_matches("raw: ").to_string())
        .collect();
    assert_eq!(printed_raw, field(served_items, "raw_token"), "{stdout}");
    let json_items = value["attention"]["data"]["items"]
        .as_array()
        .expect("json items");
    assert_eq!(field(json_items, "key"), field(served_items, "key"));
    assert!(!daemon.is_finished(), "the daemon stayed up");
    daemon.abort();
}

async fn await_health(base: &str) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    for _ in 0..400 {
        if client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the daemon never answered /health");
}

// --- Proxy environment ---

/// Request lines received by a stand-in proxy.
type Recorded = Arc<Mutex<Vec<String>>>;

/// A stand-in HTTP proxy that records each request line and answers 502.
fn recording_proxy() -> (u16, Recorded) {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let recorded: Recorded = Arc::default();
    let log = Arc::clone(&recorded);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0_u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = text.lines().next().unwrap_or_default().to_string();
            log.lock().expect("log lock").push(line);
            let _ = stream.write_all(
                b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    (port, recorded)
}

/// Path a control request sends through the proxy, so its record is told
/// apart from anything the doctor read might send.
const PROXY_CONTROL_PATH: &str = "/proxy-control";

#[test]
fn a_proxy_in_the_environment_never_carries_the_daemon_read() {
    const TEST: &str = "a_proxy_in_the_environment_never_carries_the_daemon_read";
    if std::env::var_os(CHILD_ENV).is_some_and(|named| named == TEST) {
        in_child(TEST, &[], |xdg| Box::pin(proxied_child_case(xdg)));
        return;
    }

    // Arrange
    let (proxy_port, recorded) = recording_proxy();
    let proxy = format!("http://127.0.0.1:{proxy_port}");
    let env = [
        ("HTTP_PROXY", proxy.clone()),
        ("http_proxy", proxy.clone()),
        ("ALL_PROXY", proxy),
    ];

    // Act
    in_child(TEST, &env, |xdg| Box::pin(proxied_child_case(xdg)));

    // Assert
    let lines = recorded.lock().expect("log lock").clone();
    assert_eq!(
        lines.len(),
        1,
        "only the control request reaches the proxy: {lines:?}"
    );
    assert!(lines[0].contains(PROXY_CONTROL_PATH), "{lines:?}");
}

async fn proxied_child_case(xdg: PathBuf) {
    let served = healthy_envelope();
    let mock = mock_serving(ResponseTemplate::new(200).set_body_json(&served)).await;
    let config_path = write_config(&xdg, "127.0.0.1", port_of(&mock), "");

    let (_, value, _) = json_run(&config_path).await;
    assert_eq!(value["attention"]["source"], json!("daemon"));
    assert_eq!(value["attention"]["data"], served["data"]);

    // Control: a client that honors the environment goes through the proxy.
    let _ = reqwest::Client::new()
        .get(format!("{}{PROXY_CONTROL_PATH}", mock.uri()))
        .send()
        .await;
}
