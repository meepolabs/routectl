//! The Anthropic stream's opening count across a real network boundary: a
//! routectl server on a loopback `TcpListener` (isolated HOME, XDG config
//! dir and usage DB), a scripted loopback upstream, and a `reqwest` client.
//! Pins what an in-process body poll cannot: when the first body byte
//! reaches a network client, the HTTP status it sees, and that a client
//! hanging up mid-stream leaves no anchor behind. Anchors are observed from
//! outside by the next turn's opening count.

mod common;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::anthropic::context_anchor::{RequestIdentity, anchored_input};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const GRACE: Duration = Duration::from_millis(2_500);
const PAST_GRACE: Duration = Duration::from_millis(3_200);
const SESSION_HEADER: &str = "x-claude-code-session-id";

// ------------------------------------------------------------ isolation

/// HOME and XDG config dir pointed at a fresh tempdir for the test's life.
struct Isolated {
    _home: ScopedEnv,
    _xdg: ScopedEnv,
    _dir: tempfile::TempDir,
}

fn isolate() -> Isolated {
    let dir = tempfile::tempdir().expect("isolated home");
    let xdg = dir.path().join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg dir");
    Isolated {
        _home: ScopedEnv::set("HOME", dir.path()),
        _xdg: ScopedEnv::set("XDG_CONFIG_HOME", &xdg),
        _dir: dir,
    }
}

// ------------------------------------------------------------ upstream

/// One scripted response: a held-back head, then body pieces spaced out.
#[derive(Clone)]
struct Script {
    head_delay: Duration,
    status: u16,
    content_type: &'static str,
    pieces: Vec<(Duration, String)>,
}

impl Script {
    fn now(body: String) -> Self {
        Self {
            head_delay: Duration::ZERO,
            status: 200,
            content_type: "text/event-stream",
            pieces: vec![(Duration::ZERO, body)],
        }
    }

    /// An immediate Anthropic `overloaded_error` (HTTP 529) JSON response.
    fn overloaded() -> Self {
        Self {
            status: 529,
            content_type: "application/json",
            ..Self::now(
                json!({"type": "error",
                       "error": {"type": "overloaded_error", "message": "busy"}})
                .to_string(),
            )
        }
    }
}

type Scripts = Arc<Vec<(String, String, Script)>>;

/// Serve `scripts` (path, body marker, script; first match wins, an empty
/// marker matches any body) on an ephemeral loopback port.
async fn upstream(scripts: Vec<(&str, &str, Script)>) -> String {
    let scripts: Scripts = Arc::new(
        scripts
            .into_iter()
            .map(|(p, m, s)| (p.to_string(), m.to_string(), s))
            .collect(),
    );
    let app = axum::Router::new()
        .fallback(serve_script)
        .with_state(scripts);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    base
}

async fn serve_script(
    axum::extract::State(scripts): axum::extract::State<Scripts>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let text = String::from_utf8_lossy(&body).into_owned();
    let Some(script) = scripts
        .iter()
        .find(|(path, marker, _)| path == uri.path() && text.contains(marker.as_str()))
        .map(|(_, _, script)| script.clone())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    tokio::time::sleep(script.head_delay).await;
    let pieces = futures::stream::iter(script.pieces).then(|(gap, piece)| async move {
        tokio::time::sleep(gap).await;
        Ok::<Bytes, Infallible>(Bytes::from(piece))
    });
    Response::builder()
        .status(script.status)
        .header("content-type", script.content_type)
        .body(Body::from_stream(pieces))
        .expect("response builds")
}

/// A loopback upstream that accepts each connection and closes it without
/// a response. Returns its base URL and a count of accepted connections.
async fn closing_upstream() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(socket);
        }
    });
    (base, accepted)
}

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

const OPENER: &str = r#"{"input_tokens":13,"output_tokens":1,"cache_creation_input_tokens":317,"cache_read_input_tokens":41000,"cache_creation":{"ephemeral_5m_input_tokens":300,"ephemeral_1h_input_tokens":17}}"#;

fn anthropic_stream() -> String {
    let opener: Value = serde_json::from_str(OPENER).unwrap();
    [
        sse(
            "message_start",
            &json!({"type": "message_start", "message": {
                "id": "msg_net", "type": "message", "role": "assistant", "content": [],
                "model": "claude-opus-4-7", "stop_reason": null, "stop_sequence": null,
                "usage": opener}}),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "ok"}}),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
        sse(
            "message_delta",
            &json!({"type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"input_tokens": 29, "output_tokens": 9,
                              "cache_creation_input_tokens": 317,
                              "cache_read_input_tokens": 41000}}),
        ),
        sse("message_stop", &json!({"type": "message_stop"})),
    ]
    .concat()
}

fn data(v: &Value) -> String {
    format!("data: {v}\n\n")
}

/// OpenAI-compatible content chunk.
fn openai_content() -> String {
    data(&json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}))
}

/// OpenAI-compatible finish, terminal usage and `[DONE]`.
fn openai_end(prompt: u64) -> String {
    data(&json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {}, "finish_reason": "stop"}]}))
        + &data(
            &json!({"id": "c1", "model": "glm-4.6", "choices": [], "usage": {
            "prompt_tokens": prompt, "completion_tokens": 3, "total_tokens": prompt + 3}}),
        )
        + "data: [DONE]\n\n"
}

// ------------------------------------------------------------ routectl

fn config(provider: ProviderEntry, upstream_model: &str) -> Arc<Config> {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut providers = BTreeMap::new();
    providers.insert("p".to_string(), provider);
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", upstream_model));
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "claude-opus".to_string(),
        AliasValue::Single("m".to_string()),
    );
    Arc::new(Config {
        providers,
        models,
        aliases,
        retry,
        ..Default::default()
    })
}

fn anthropic_config(base: &str) -> Arc<Config> {
    config(
        ProviderEntry::anthropic_api(common::file_ref("k")).with_base_url(base.to_string()),
        "claude-opus-4-7",
    )
}

fn translated_config(base: &str) -> Arc<Config> {
    config(
        ProviderEntry::openai_compat(format!("{base}/v1"), common::file_ref("k")),
        "glm-4.6",
    )
}

/// Start routectl on a loopback listener and wait for `/health`.
async fn routectl(config: Arc<Config>) -> String {
    routectl_with_ledger(config).await.0
}

/// [`routectl`], also returning the path of its isolated usage ledger.
async fn routectl_with_ledger(config: Arc<Config>) -> (String, std::path::PathBuf) {
    let config = common::isolate_usage_db(config);
    let ledger = config.usage.db_path.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = routectl_cli::server::serve_on_listener(config, listener, None).await;
    });
    let client = reqwest::Client::new();
    for _ in 0..250 {
        if client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return (base, ledger);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("routectl did not come up at {base}");
}

fn turn(messages: &[(&str, &str)]) -> Value {
    json!({
        "model": "claude-opus",
        "max_tokens": 64,
        "stream": true,
        "messages": messages
            .iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect::<Vec<_>>(),
    })
}

fn canonical(body: &Value) -> routectl_core::ChatRequest {
    AnthropicIngress
        .parse_request(
            &axum::http::HeaderMap::new(),
            &serde_json::to_vec(body).unwrap(),
        )
        .expect("fixture parses")
}

fn raw_estimate(body: &Value) -> u64 {
    routectl_router::estimate_meter_tokens(&canonical(body))
}

fn expected_anchor(prior: &Value, next: &Value, actual: u64) -> u64 {
    let prior = RequestIdentity::measure(&canonical(prior), None);
    let next = RequestIdentity::measure(&canonical(next), Some(prior.message_count()));
    anchored_input(
        actual,
        prior.normalized_estimate(),
        next.normalized_estimate(),
    )
}

/// One network turn read to its end. `headers` is when the response head
/// reached the client (the `send` future resolved); `first_body` is when
/// the first nonempty body piece did, measured from the same start.
struct NetTurn {
    status: StatusCode,
    content_type: String,
    headers: Duration,
    first_body: Duration,
    text: String,
}

impl NetTurn {
    fn opening_usage(&self) -> Value {
        let start = self
            .text
            .split("\n\n")
            .find(|f| f.contains("event: message_start"))
            .and_then(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .unwrap_or_else(|| panic!("a message_start: {}", self.text));
        let start: Value = serde_json::from_str(start).expect("json");
        start["message"]["usage"].clone()
    }

    fn opening_input(&self) -> u64 {
        self.opening_usage()["input_tokens"]
            .as_u64()
            .expect("input_tokens")
    }

    /// A 200 SSE stream whose head arrived in `[from, before)` and whose
    /// first body byte did not precede its head.
    fn assert_sse_head_within(&self, from: Duration, before: Duration) {
        assert_eq!(self.status, StatusCode::OK, "{}", self.text);
        assert!(
            self.content_type.starts_with("text/event-stream"),
            "content-type {:?}",
            self.content_type
        );
        assert!(
            self.headers >= from && self.headers < before,
            "response head at {:?}, expected in [{from:?}, {before:?})",
            self.headers
        );
        assert!(self.first_body >= self.headers);
    }
}

async fn post(base: &str, session: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(SESSION_HEADER, session)
        .json(body)
        .send()
        .await
        .expect("request sent")
}

async fn read_turn(base: &str, session: &str, body: &Value) -> NetTurn {
    let started = Instant::now();
    let resp = post(base, session, body).await;
    let headers = started.elapsed();
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut stream = resp.bytes_stream();
    let mut text = String::new();
    let mut first_body = None;
    while let Some(piece) = stream.next().await {
        let piece = piece.expect("body piece");
        if first_body.is_none() && !piece.is_empty() {
            first_body = Some(started.elapsed());
        }
        text.push_str(&String::from_utf8_lossy(&piece));
    }
    NetTurn {
        status,
        content_type,
        headers,
        first_body: first_body.expect("some body arrived"),
        text,
    }
}

/// The `extra` of the ledger row with `request_id`, once it lands.
async fn ledger_extra(ledger: &std::path::Path, request_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(db) = routectl_usage::open_readonly(ledger)
            && let Ok(extra) = db.conn().query_row(
                "SELECT extra FROM requests WHERE request_id = ?1",
                [request_id],
                |r| r.get::<_, Option<String>>(0),
            )
        {
            return extra.map_or(Value::Null, |text| {
                serde_json::from_str(&text).expect("json")
            });
        }
        assert!(Instant::now() < deadline, "row {request_id} never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ------------------------------------------------------------ tests

#[tokio::test]
#[serial_test::serial]
async fn a_served_daemon_persists_the_opening_its_network_client_received() {
    // Arrange
    let _env = isolate();
    let up = upstream(vec![("/v1/messages", "", Script::now(anthropic_stream()))]).await;
    let (base, ledger) = routectl_with_ledger(anthropic_config(&up)).await;

    // Act
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(SESSION_HEADER, "net-ledger")
        .header("x-request-id", "net-ledger-1")
        .json(&turn(&[("user", "hi")]))
        .send()
        .await
        .expect("request sent");
    let text = resp.text().await.expect("body");
    let extra = ledger_extra(&ledger, "net-ledger-1").await;

    // Assert: the persisted count is the frame the client read.
    let usage = NetTurn {
        status: StatusCode::OK,
        content_type: String::new(),
        headers: Duration::ZERO,
        first_body: Duration::ZERO,
        text,
    }
    .opening_usage();
    let rendered: u64 = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|field| usage[field].as_u64().expect("field"))
    .sum();
    assert_eq!(extra["opening_input"], rendered, "{extra}");
    assert_eq!(extra["opening_source"], "upstream_wire_unverified");
    assert_eq!(extra["terminal_source"], "explicit_final");
    assert_eq!(extra["terminal_vendor_verified"], false);
}

#[tokio::test]
#[serial_test::serial]
async fn a_fast_winner_reaches_the_network_client_with_its_exact_opener() {
    // Arrange
    let _env = isolate();
    let up = upstream(vec![("/v1/messages", "", Script::now(anthropic_stream()))]).await;
    let base = routectl(anthropic_config(&up)).await;

    // Act
    let turn = read_turn(&base, "net-fast", &turn(&[("user", "hi")])).await;

    // Assert
    turn.assert_sse_head_within(Duration::ZERO, GRACE);
    assert!(
        turn.first_body < GRACE,
        "first body at {:?}",
        turn.first_body
    );
    let mut expected: Value = serde_json::from_str(OPENER).unwrap();
    expected["output_tokens"] = json!(0);
    assert_eq!(turn.opening_usage(), expected, "{}", turn.text);
}

#[tokio::test]
#[serial_test::serial]
async fn a_slow_dispatch_sends_the_first_body_byte_to_the_network_at_the_grace() {
    // Arrange: the upstream head is held past the grace.
    let _env = isolate();
    let slow = Script {
        head_delay: PAST_GRACE,
        ..Script::now(anthropic_stream())
    };
    let up = upstream(vec![("/v1/messages", "", slow)]).await;
    let base = routectl(anthropic_config(&up)).await;
    let body = turn(&[("user", "a slow question")]);

    // Act
    let turn = read_turn(&base, "net-slow", &body).await;

    // Assert: the head and the provisional first frame both reach the
    // client at the grace, before the upstream head arrived.
    turn.assert_sse_head_within(GRACE, PAST_GRACE);
    assert!(
        turn.first_body >= GRACE && turn.first_body < PAST_GRACE,
        "first body at {:?}",
        turn.first_body
    );
    let first_frame = turn
        .text
        .split("\n\n")
        .find(|f| f.lines().any(|l| l.starts_with("data:")))
        .expect("a first frame");
    assert!(
        first_frame.lines().any(|l| l == "event: message_start"),
        "the first frame is message_start: {}",
        turn.text
    );
    assert_eq!(turn.opening_input(), raw_estimate(&body));
    assert_eq!(turn.text.matches("event: message_start").count(), 1);
}

/// POST one turn and read the whole reply as a JSON error envelope,
/// timing when the head arrived.
async fn error_turn(base: &str) -> (StatusCode, Duration, String, Value) {
    let started = Instant::now();
    let resp = post(base, "net-err", &turn(&[("user", "hi")])).await;
    let headers = started.elapsed();
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body: Value = resp.json().await.expect("json error envelope");
    (status, headers, content_type, body)
}

#[tokio::test]
#[serial_test::serial]
async fn a_fast_upstream_error_reaches_the_network_client_as_its_http_status() {
    // Arrange: a test-owned upstream answering 529 overloaded at once.
    let _env = isolate();
    let up = upstream(vec![("/v1/messages", "", Script::overloaded())]).await;
    let base = routectl(anthropic_config(&up)).await;

    // Act
    let (status, headers, content_type, body) = error_turn(&base).await;

    // Assert: a real HTTP error before the grace, never a 200 stream.
    assert_eq!(status.as_u16(), 529, "{body}");
    assert!(headers < GRACE, "head at {headers:?}");
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
#[serial_test::serial]
async fn an_upstream_that_closes_the_connection_reaches_the_client_as_a_gateway_error() {
    // Arrange: a test-owned upstream that accepts and closes every connection.
    let _env = isolate();
    let (up, accepted) = closing_upstream().await;
    let base = routectl(anthropic_config(&up)).await;

    // Act
    let (status, headers, content_type, body) = error_turn(&base).await;

    // Assert: premise -- the upstream was actually reached.
    assert!(accepted.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(headers < GRACE, "head at {headers:?}");
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
}

#[tokio::test]
#[serial_test::serial]
async fn a_network_client_hanging_up_mid_stream_leaves_no_anchor() {
    // Arrange: turns carrying "hangup" send content, then their terminal
    // usage a second later; every other turn completes at once.
    const ACTUAL: u64 = 31_337;
    let _env = isolate();
    let delayed = Script {
        pieces: vec![
            (Duration::ZERO, openai_content()),
            (Duration::from_secs(1), openai_end(ACTUAL)),
        ],
        ..Script::now(String::new())
    };
    let prompt = "/v1/chat/completions";
    let up = upstream(vec![
        (prompt, "hangup", delayed),
        (
            prompt,
            "",
            Script::now(openai_content() + &openai_end(ACTUAL)),
        ),
    ])
    .await;
    let base = routectl(translated_config(&up)).await;
    let hung = turn(&[("user", "hangup question")]);
    let hung_next = turn(&[
        ("user", "hangup question"),
        ("assistant", "ok"),
        ("user", "again"),
    ]);
    let kept = turn(&[("user", "kept question")]);
    let kept_next = turn(&[
        ("user", "kept question"),
        ("assistant", "ok"),
        ("user", "again"),
    ]);

    // Act: hang up after the first body piece, while terminal usage is
    // still in flight.
    let resp = post(&base, "net-hangup", &hung).await;
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    stream.next().await.expect("first piece").expect("readable");
    drop(stream);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let after_hangup = read_turn(&base, "net-hangup", &hung_next).await;
    // Positive control: the same shape read to the end does anchor.
    read_turn(&base, "net-kept", &kept).await;
    let after_kept = read_turn(&base, "net-kept", &kept_next).await;

    // Assert
    assert_eq!(after_hangup.opening_input(), raw_estimate(&hung_next));
    let anchored = expected_anchor(&kept, &kept_next, ACTUAL);
    assert_ne!(
        anchored,
        raw_estimate(&kept_next),
        "fixture separates tiers"
    );
    assert_eq!(after_kept.opening_input(), anchored);
}
