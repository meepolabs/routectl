//! Process-level proof that starting the daemon makes no network call: every
//! connection the started `routectl serve` opens through the proxy variables,
//! and every one it opens to its loopback provider, is recorded and must be
//! zero.
//!
//! # Why this is its own test binary
//!
//! The proxy variables are per process. They are set only on the spawned
//! child here, never on this process, but the binary still holds nothing else
//! so no shared harness or sibling test can run inside a process whose
//! environment a later edit might widen. The child gets a hermetic `HOME` and
//! `XDG_CONFIG_HOME`, no `ROUTECTL_CONFIG`, and no `NO_PROXY`, so the proxy
//! variables are authoritative.
//!
//! The positive control runs `routectl prices refresh` under the same
//! environment: the one command that does fetch must reach the recorder as
//! exactly one `CONNECT`, which proves the recorder would have seen a fetch
//! made at start.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use routectl_router::CURRENT_CONFIG_VERSION as CURRENT;

const BIN: &str = env!("CARGO_BIN_EXE_routectl");

const READY_DEADLINE: Duration = Duration::from_secs(30);

/// How long the started daemon is watched after it serves. Covers any
/// deferred start-up task; a fetch is a single connect, far inside this.
const QUIET_WINDOW: Duration = Duration::from_secs(2);

const KEY_VAR: &str = "PRICES_NO_NETWORK_TEST_KEY";

/// A loopback listener that records the first request line of every
/// connection it accepts and answers 403, standing in for both the operator's
/// proxy and the configured provider.
struct Recorder {
    port: u16,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind recorder");
        let port = listener.local_addr().expect("recorder address").port();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                sink.lock().unwrap().push(first_line(stream));
            }
        });
        Self { port, lines }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

fn first_line(mut stream: TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let _ = BufReader::new(&mut stream).read_line(&mut line);
    let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    line.trim_end().to_string()
}

/// A hermetic home with a config serving on `port` whose one provider is the
/// loopback `upstream`, plus a valid price table in the config folder.
fn hermetic_home(port: u16, upstream: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_dir = dir.path().join(".config").join("routectl");
    std::fs::create_dir_all(&config_dir).expect("mkdir config dir");
    std::fs::write(
        config_dir.join("prices.json"),
        br#"{"schema_version":1,"generated_at":"2026-11-01","providers":[{"id":"anthropic","models":[{"id":"m","input":1.0,"output":2.0}]}]}"#,
    )
    .expect("write price table");
    let config_path = config_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT}\n\
             [server]\n\
             host = \"127.0.0.1\"\n\
             port = {port}\n\
             [usage]\n\
             enabled = true\n\
             [providers.local]\n\
             kind = \"openai-compat\"\n\
             base_url = \"{upstream}/v1\"\n\
             api_key_ref = \"env://{KEY_VAR}\"\n\
             [models.m]\n\
             provider = \"local\"\n\
             upstream = \"m\"\n\
             [aliases]\n\
             fast = \"m\"\n"
        ),
    )
    .expect("write config");
    (dir, config_path)
}

/// A `routectl` command with the hermetic home and every proxy spelling
/// pointed at `proxy`, and every bypass spelling removed.
fn command(home: &Path, proxy: &str) -> Command {
    let mut command = Command::new(BIN);
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CARGO_ENV_XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("ROUTECTL_CONFIG")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env(KEY_VAR, "test-key")
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        command.env(var, proxy);
    }
    command
}

/// Kills and reaps the child by its own handle when the test leaves.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// The status line of one plain request to the daemon, or `None` when it
/// does not answer.
fn request(port: u16, head: &str, body: &str) -> Option<String> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let text = format!(
        "{head} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(text.as_bytes()).ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    response.lines().next().map(str::to_string)
}

fn await_serving(child: &mut Reaped, port: u16) {
    let until = Instant::now() + READY_DEADLINE;
    while Instant::now() < until {
        if let Some(status) = child.0.try_wait().expect("poll child") {
            panic!("the daemon exited during startup with {status}");
        }
        if request(port, "GET /health", "").is_some_and(|l| l.contains(" 200")) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemon never began serving on port {port}");
}

#[test]
fn a_started_daemon_opens_no_connection_and_prices_refresh_opens_one() {
    // Arrange
    let proxy = Recorder::start();
    let upstream = Recorder::start();
    let port = free_port();
    let (home, config_path) = hermetic_home(port, &upstream.url());

    // Act: start, serve, price one status query (it reads the table), wait.
    let mut serve = Reaped(
        command(home.path(), &proxy.url())
            .arg("--config")
            .arg(&config_path)
            .arg("serve")
            .spawn()
            .expect("spawn serve"),
    );
    await_serving(&mut serve, port);
    let query = request(
        port,
        "QUERY /status/query",
        r#"{"window":"all","group_by":"model"}"#,
    );
    std::thread::sleep(QUIET_WINDOW);
    let started_proxy = proxy.lines();
    let started_upstream = upstream.lines();
    drop(serve);

    // Control: the one command that fetches, under the same environment.
    let refresh = command(home.path(), &proxy.url())
        .args(["prices", "refresh"])
        .status()
        .expect("run prices refresh");
    let refreshed_proxy = proxy.lines();

    // Assert
    assert!(
        query.as_deref().is_some_and(|l| l.contains(" 200")),
        "the priced status query must be answered: {query:?}"
    );
    assert!(
        started_proxy.is_empty(),
        "a started daemon must open no connection through the proxy: {started_proxy:?}"
    );
    assert!(
        started_upstream.is_empty(),
        "a started daemon must not contact its provider unasked: {started_upstream:?}"
    );
    assert!(!refresh.success(), "the recorder refuses the tunnel");
    assert_eq!(
        refreshed_proxy.len(),
        1,
        "control: prices refresh must reach the proxy exactly once: {refreshed_proxy:?}"
    );
    assert!(
        refreshed_proxy[0].starts_with("CONNECT "),
        "control: the refresh tunnels through the proxy: {refreshed_proxy:?}"
    );
}
