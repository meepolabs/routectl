//! A token count the upstream rejects because of an operator payload extra,
//! driven through the REAL `routectl serve` binary in a child process: the
//! count is re-sent once without the extra and the client gets the count.
//!
//! The revert only fires when the count handler captured the client's body,
//! so this test spans the whole path the router-level tests cannot: HTTP
//! ingress, wire capture, egress body and the upstream's own view of it.
//!
//! Isolation: `HOME` and `XDG_CONFIG_HOME` point at a fresh tempdir on the
//! child only, so its config, usage ledger and catalog state never touch the
//! developer's real `~/.config/routectl`.

mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use routectl_router::CURRENT_CONFIG_VERSION as CURRENT;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_routectl");
const ALIAS: &str = "counted";
const EXTRA_KEY: &str = "mcp_servers";
const COUNT_PATH: &str = "/v1/messages/count_tokens";
const COUNTED: u64 = 42;

/// A child `routectl serve`, killed and reaped however the test exits. Its
/// stderr drains on a reader thread so a chatty child never blocks on a full
/// pipe, and a boot failure can show the daemon's own words.
struct ServeChild {
    child: Child,
    stderr: Option<JoinHandle<String>>,
}

impl ServeChild {
    /// Kill the child, then collect everything it wrote to stderr. The kill
    /// closes the pipe's write end, so the reader thread is sure to finish.
    fn stop_and_read_stderr(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr
            .take()
            .map(|reader| {
                reader
                    .join()
                    .unwrap_or_else(|_| "<stderr reader panicked>".to_owned())
            })
            .unwrap_or_default()
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain(mut stderr: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn spawn_serve(home: &Path, config: &Path) -> ServeChild {
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(config)
        .arg("serve")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("ROUTECTL_CONFIG")
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {BIN} serve: {e}"));
    let stderr = child.stderr.take().map(drain);
    ServeChild { child, stderr }
}

/// A loopback port that was free a moment ago; the child binds it itself.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// One anthropic-api provider at `upstream` whose operator payload extras
/// declare an MCP server, a key the count body forwards and the client never
/// sends.
fn config_text(port: u16, upstream: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"version = {CURRENT}
[server]
host = "127.0.0.1"
port = {port}

[retry]
max_attempts = 1
initial_backoff_ms = 0
jitter_ms = 0

[providers.up]
kind = "anthropic-api"
base_url = "{upstream}"
api_key_ref = "{key_ref}"
payload_extras = {{ {EXTRA_KEY} = [{{ type = "url", name = "s1", url = "https://mcp.example.com/sse" }}] }}

[models.m0]
provider = "up"
upstream = "claude-sonnet-4-6"

[aliases]
{ALIAS} = "m0"
"#
    )
}

fn carries_extra(request: &Request) -> bool {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .is_some_and(|body| body.get(EXTRA_KEY).is_some())
}

/// An upstream that refuses a count carrying the extra, as anthropic-api
/// words it, and counts any other.
async fn refusing_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(COUNT_PATH))
        .respond_with(|request: &Request| {
            if carries_extra(request) {
                ResponseTemplate::new(400).set_body_json(json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": format!("{EXTRA_KEY}: Extra inputs are not permitted"),
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"input_tokens": COUNTED}))
            }
        })
        .mount(&server)
        .await;
    server
}

async fn count_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path() == COUNT_PATH)
        .map(|request| serde_json::from_slice(&request.body).expect("JSON body"))
        .collect()
}

#[tokio::test]
async fn count_tokens_endpoint_reverts_a_rejected_operator_extra() {
    // Arrange
    let upstream = refusing_upstream().await;
    let home = tempfile::tempdir().expect("tempdir");
    let config_dir = home.path().join(".config").join("routectl");
    std::fs::create_dir_all(&config_dir).expect("mkdir config dir");
    let config_path = config_dir.join("config.toml");
    let port = free_port();
    std::fs::write(&config_path, config_text(port, &upstream.uri())).expect("write config");
    let mut daemon = spawn_serve(home.path(), &config_path);
    let base = format!("http://127.0.0.1:{port}");
    let healthy = common::readiness::healthy_within_deadline(&base).await;
    assert!(
        healthy,
        "the daemon did not become healthy at {base}; stderr:\n{}",
        daemon.stop_and_read_stderr()
    );

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}{COUNT_PATH}"))
        .json(&json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("count request");

    // Assert
    let status = resp.status();
    let body: Value = resp.json().await.expect("JSON response");
    assert_eq!(status, 200, "the re-sent count is served: {body}");
    assert_eq!(body["input_tokens"], COUNTED);
    let sent = count_bodies(&upstream).await;
    assert_eq!(sent.len(), 2, "the refused count is re-sent once: {sent:?}");
    assert!(
        sent[0].get(EXTRA_KEY).is_some(),
        "the first count carries the operator's extra"
    );
    assert!(
        sent[1].get(EXTRA_KEY).is_none(),
        "the re-sent count lacks the extra"
    );
}
