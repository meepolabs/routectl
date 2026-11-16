//! `ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN` across the real `serve` boot, in a
//! CHILD process: the variable is set on the child only, so no test in this
//! binary or any other reads or writes this process's environment.
//!
//! Isolation: `HOME` and `XDG_CONFIG_HOME` point into a fresh tempdir on the
//! child, the usage ledger is pinned inside it, and the only upstream is a
//! loopback mock.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use routectl_router::CURRENT_CONFIG_VERSION as CURRENT;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_routectl");
const VAR: &str = "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN";
const READY_DEADLINE: Duration = Duration::from_secs(30);
const STATUS_LINE: &str = "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN resolved";

/// Past the 8 attempts first-N always checks, so a daemon sampling 1 in 100
/// checks only 8 of them.
const REQUESTS: u64 = 12;

/// A child `routectl serve`, killed and reaped when dropped, whose stderr is
/// collected on a reader thread.
struct Daemon {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn spawn(home: &Path, config: &Path, sample_one_in: Option<&str>) -> Self {
        let mut command = Command::new(BIN);
        match sample_one_in {
            Some(value) => command.env(VAR, value),
            None => command.env_remove(VAR),
        };
        let mut child = command
            .arg("--config")
            .arg(config)
            .arg("serve")
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env_remove("ROUTECTL_CONFIG")
            .env("ROUTECTL_LOG", "info")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {BIN} serve: {e}"));
        let pipe = child.stderr.take().expect("stderr is piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut text = sink.lock().expect("stderr lock");
                text.push_str(&line);
                text.push('\n');
            }
        });
        Self {
            child,
            stderr,
            reader: Some(reader),
        }
    }

    /// Everything the child wrote to stderr, once it has exited.
    fn exited_stderr(&mut self) -> String {
        if let Some(reader) = self.reader.take() {
            reader.join().expect("stderr reader");
        }
        self.stderr()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    /// The exit status once the child exits on its own, or `None` past the
    /// deadline.
    fn exit_within(&mut self, deadline: Duration) -> Option<ExitStatus> {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    async fn await_health(&mut self, base_url: &str) {
        let until = Instant::now() + READY_DEADLINE;
        let client = reqwest::Client::new();
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                panic!("serve exited during boot ({status}): {}", self.stderr());
            }
            let probe = client
                .get(format!("{base_url}/health"))
                .timeout(Duration::from_secs(2))
                .send()
                .await;
            if probe.is_ok_and(|r| r.status().is_success()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("serve never answered /health: {}", self.stderr());
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// A hermetic home holding a config with one openai-compat model aimed at
/// `upstream`.
fn hermetic_home(port: u16, upstream: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_dir = dir.path().join(".config").join("routectl");
    std::fs::create_dir_all(&config_dir).expect("mkdir config dir");
    let key_ref = routectl_testkit::secret_file_ref("test-key");
    let usage_db = dir.path().join("usage.db");
    let config_path = config_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT}\n\
             [server]\nhost = \"127.0.0.1\"\nport = {port}\n\n\
             [usage]\ndb_path = \"{}\"\n\n\
             [providers.fast]\nkind = \"openai-compat\"\nbase_url = \"{upstream}/v1\"\n\
             api_key_ref = \"{key_ref}\"\n\n\
             [models.fast]\nprovider = \"fast\"\nupstream = \"gpt-4o-mini\"\n\n\
             [aliases]\ndefault = \"fast\"\n",
            usage_db.display()
        ),
    )
    .expect("write config");
    (dir, config_path)
}

async fn chat_upstream() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000_i64,
            "model": "gpt-4o-mini",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(&mock)
        .await;
    mock
}

/// Checks the daemon has counted, summed over every lane and outcome.
async fn checks_counted(base_url: &str) -> u64 {
    let body: Value = reqwest::get(format!("{base_url}/status/health"))
        .await
        .expect("GET /status/health")
        .json()
        .await
        .expect("health panel json");
    body["data"]["wire_check"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("health panel has wire_check checks: {body}"))
        .iter()
        .map(|row| row["count"].as_u64().unwrap_or_default())
        .sum()
}

async fn send_chat(client: &reqwest::Client, base_url: &str, turn: u64) {
    let resp = client
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({"model": "default",
                      "messages": [{"role": "user", "content": format!("hi {turn}")}]}))
        .send()
        .await
        .expect("the daemon answers");
    assert!(resp.status().is_success(), "turn {turn}: {}", resp.status());
}

/// What one boot under `sample_one_in` did with [`REQUESTS`] successful
/// attempts.
struct Run {
    /// Checks one admitted attempt counts (one per checked leg), read off the
    /// first attempt, which first-N always admits.
    per_attempt: u64,
    /// Checks counted after every attempt.
    total: u64,
    stderr: String,
}

async fn run_requests(sample_one_in: Option<&str>) -> Run {
    let upstream = chat_upstream().await;
    let port = free_port();
    let (home, config) = hermetic_home(port, &upstream.uri());
    let base_url = format!("http://127.0.0.1:{port}");
    let mut daemon = Daemon::spawn(home.path(), &config, sample_one_in);
    daemon.await_health(&base_url).await;
    let client = reqwest::Client::new();

    send_chat(&client, &base_url, 0).await;
    let per_attempt = checks_counted(&base_url).await;
    for turn in 1..REQUESTS {
        send_chat(&client, &base_url, turn).await;
    }
    let total = checks_counted(&base_url).await;
    Run {
        per_attempt,
        total,
        stderr: daemon.stderr(),
    }
}

/// The one startup status line for the variable.
fn status_line(stderr: &str) -> &str {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains(STATUS_LINE))
        .collect();
    assert_eq!(lines.len(), 1, "one status line at boot:\n{stderr}");
    assert!(lines[0].contains(" INFO "), "logged at INFO: {}", lines[0]);
    lines[0]
}

#[tokio::test]
async fn a_rate_of_one_from_the_environment_checks_every_attempt_and_is_logged_at_boot() {
    // Act
    let run = run_requests(Some("1")).await;

    // Assert
    assert!(
        run.per_attempt > 0,
        "the first attempt is checked:\n{}",
        run.stderr
    );
    assert_eq!(
        run.total,
        run.per_attempt * REQUESTS,
        "every successful attempt is checked:\n{}",
        run.stderr
    );
    let line = status_line(&run.stderr);
    assert!(
        line.contains("sample_one_in=1 "),
        "the resolved rate: {line}"
    );
    assert!(
        line.contains("source=\"env\""),
        "the rate came from the environment: {line}"
    );
}

#[tokio::test]
async fn with_the_variable_unset_attempts_past_the_first_8_are_sampled_1_in_100() {
    // Act
    let run = run_requests(None).await;

    // Assert
    assert!(
        run.per_attempt > 0,
        "the first attempt is checked:\n{}",
        run.stderr
    );
    assert_eq!(
        run.total,
        run.per_attempt * 8,
        "only first-N is checked within 12 attempts at 1 in 100:\n{}",
        run.stderr
    );
    let line = status_line(&run.stderr);
    assert!(
        line.contains("sample_one_in=100 "),
        "the default rate: {line}"
    );
    assert!(
        line.contains("source=\"default\""),
        "no variable set it: {line}"
    );
}

#[tokio::test]
async fn an_invalid_rate_refuses_to_start_naming_the_variable_but_not_the_value() {
    // Arrange
    let upstream = chat_upstream().await;
    let port = free_port();
    let (home, config) = hermetic_home(port, &upstream.uri());
    let value = "0x2a-SENTINEL";

    // Act
    let mut daemon = Daemon::spawn(home.path(), &config, Some(value));
    let status = daemon.exit_within(READY_DEADLINE);

    // Assert
    let status = status.unwrap_or_else(|| panic!("serve kept running: {}", daemon.stderr()));
    assert!(!status.success(), "start is refused: {status}");
    let stderr = daemon.exited_stderr();
    assert!(
        stderr.contains(&format!("{VAR} must be a whole number")),
        "the refusal names the variable: {stderr}"
    );
    assert!(
        !stderr.contains(value),
        "the value is never echoed: {stderr}"
    );
    assert!(
        !stderr.contains("routectl listening"),
        "refused before the listener serves: {stderr}"
    );
}
