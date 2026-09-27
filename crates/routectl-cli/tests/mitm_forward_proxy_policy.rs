//! Process-isolated proof of the MITM forwarder's proxy policy: the
//! credential-bearing inference re-inject leg is always dialed directly and
//! never through an environment-configured proxy, while the control-plane leg
//! keeps reqwest's stock system-proxy discovery.
//!
//! # Why this is its own test binary
//!
//! reqwest reads `HTTP_PROXY` / `ALL_PROXY` / `HTTPS_PROXY` (and their
//! lower-case spellings, `NO_PROXY`, and the CGI `REQUEST_METHOD` guard) from
//! the PROCESS environment when a client is built. Setting them inside a
//! shared test binary would redirect unrelated siblings' HTTP calls, so this
//! binary carries only these cases, pulls in no shared harness, and marks every
//! case `#[serial_test::serial]` so no two of them touch the environment at
//! once.
//!
//! Every case clears all competing proxy variables before setting the one
//! under test and before building any client. An inherited `NO_PROXY` covering
//! loopback would otherwise make reqwest bypass the proxy for its own reasons
//! and pass the re-inject cases vacuously.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use routectl_cli::proxy::forward::{ForwardRequest, ForwardState, forward};
use routectl_cli::proxy::metrics::{Leg, PathClass, ProxyMetrics};
use routectl_testkit::ScopedEnv;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every variable reqwest's system-proxy matcher consults.
const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// Proxy variables that apply to an `http://` destination.
const CLEARTEXT_PROXY_KEYS: &[&str] = &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];

const BEARER_SENTINEL: &str = "Bearer synthetic-reinject-bearer-7f3a";
const BODY_SENTINEL: &str = "{\"synthetic\":\"reinject-body-sentinel-91c2\"}";
const SEAM_HEADER: &str = "x-routectl-mitm-proxied";
const SEAM_SENTINEL: &str = "synthetic-seam-nonce-4be0";

/// Synthetic external authority for the CONNECT control. The trap answers the
/// CONNECT itself, so nothing ever resolves it.
const SYNTHETIC_UPSTREAM_HOST: &str = "control.example.test";

/// Upper bound on any single forward or read in this binary, so a regression
/// fails with a named assertion instead of hanging the suite.
const WAIT: Duration = Duration::from_secs(10);

/// Clear every proxy variable, then set exactly `key` to `proxy_url`.
fn proxy_env_only(key: &str, proxy_url: &str) -> Vec<ScopedEnv> {
    let mut guards: Vec<ScopedEnv> = PROXY_ENV_KEYS.iter().map(ScopedEnv::unset).collect();
    guards.push(ScopedEnv::set(key, proxy_url));
    guards
}

/// A proxy stand-in that records the full raw bytes (head and any
/// `content-length` body) of every connection it accepts, then refuses it
/// with a 403. Never dials the requested authority.
struct ProxyTrap {
    url: String,
    captures: Arc<Mutex<Vec<String>>>,
}

impl ProxyTrap {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind trap");
        let addr = listener.local_addr().expect("trap addr");
        let captures = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captures);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let raw = read_request(&mut socket).await;
                sink.lock().expect("trap lock").push(raw);
                let _ = socket
                    .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            url: format!("http://{addr}"),
            captures,
        }
    }

    fn captures(&self) -> Vec<String> {
        self.captures.lock().expect("trap lock").clone()
    }
}

/// Reads the request head, then as many body bytes as its `content-length`
/// announces, each read bounded by [`WAIT`].
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        match tokio::time::timeout(WAIT, socket.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return String::from_utf8_lossy(&buf).into_owned(),
        }
    };
    let body_len = content_length(&String::from_utf8_lossy(&buf[..head_end]));
    while buf.len() < head_end + body_len {
        match tokio::time::timeout(WAIT, socket.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

fn credential_request() -> ForwardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static(BEARER_SENTINEL),
    );
    headers.insert(SEAM_HEADER, HeaderValue::from_static(SEAM_SENTINEL));
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    ForwardRequest {
        method: Method::POST,
        raw_path_and_query: "/v1/messages".to_string(),
        headers,
        body: reqwest::Body::from(BODY_SENTINEL),
    }
}

fn forward_state() -> ForwardState {
    ForwardState::new(8, Duration::from_secs(30)).expect("forward clients build")
}

async fn forward_bounded(
    state: &ForwardState,
    upstream_base: &str,
    leg: Leg,
    path_class: PathClass,
) -> StatusCode {
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(upstream_base).expect("upstream base parses");
    let response = tokio::time::timeout(
        WAIT,
        forward(
            state,
            &metrics,
            &upstream_base,
            credential_request(),
            leg,
            path_class,
        ),
    )
    .await
    .expect("forward must finish within the bounded wait");
    response.status()
}

async fn reinject_target() -> MockServer {
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&target)
        .await;
    target
}

/// Premise control: the trap can observe exactly what the re-inject leg must
/// never hand it. A stock reqwest client under `HTTP_PROXY` sends a loopback
/// cleartext request to the trap in absolute form, bearer and body included.
/// Without this, a trap that never saw anything would prove nothing.
#[tokio::test]
#[serial_test::serial]
async fn stock_client_under_http_proxy_hands_loopback_bearer_and_body_to_the_trap() {
    let trap = ProxyTrap::start().await;
    let _env = proxy_env_only("HTTP_PROXY", &trap.url);
    let target = reinject_target().await;
    let client = reqwest::Client::new();

    let status = tokio::time::timeout(
        WAIT,
        client
            .post(format!("{}/v1/messages", target.uri()))
            .header(http::header::AUTHORIZATION, BEARER_SENTINEL)
            .body(BODY_SENTINEL)
            .send(),
    )
    .await
    .expect("stock send must finish within the bounded wait")
    .expect("trap answers the proxied request")
    .status();

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the trap, not the target, answered"
    );
    let captures = trap.captures();
    assert_eq!(
        captures.len(),
        1,
        "exactly one proxied request: {captures:?}"
    );
    let raw = &captures[0];
    assert!(
        raw.starts_with(&format!("POST {}/v1/messages HTTP/1.1", target.uri())),
        "absolute-form request line expected: {raw:?}"
    );
    assert!(
        raw.contains(BEARER_SENTINEL),
        "bearer reached the trap: {raw:?}"
    );
    assert!(
        raw.contains(BODY_SENTINEL),
        "body reached the trap: {raw:?}"
    );
    assert!(target.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn inference_reinject_bypasses_every_cleartext_proxy_variable() {
    for key in CLEARTEXT_PROXY_KEYS {
        let trap = ProxyTrap::start().await;
        let _env = proxy_env_only(key, &trap.url);
        let target = reinject_target().await;
        let state = forward_state();

        let status =
            forward_bounded(&state, &target.uri(), Leg::Inference, PathClass::Inference).await;

        assert_eq!(
            trap.captures(),
            Vec::<String>::new(),
            "{key}: the re-inject leg must never reach the proxy"
        );
        assert_eq!(
            status,
            StatusCode::OK,
            "{key}: the loopback target answered"
        );
        let received = target.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "{key}: exactly one direct request");
        let request = &received[0];
        assert_eq!(request.url.path(), "/v1/messages");
        assert_eq!(
            request.headers.get(http::header::AUTHORIZATION).unwrap(),
            BEARER_SENTINEL
        );
        assert_eq!(request.headers.get(SEAM_HEADER).unwrap(), SEAM_SENTINEL);
        assert_eq!(request.body, BODY_SENTINEL.as_bytes());
    }
}

/// Same loopback target, different leg: the control-plane client still
/// honors `HTTP_PROXY`. Pins that the direct dial is keyed on the leg, not on
/// the destination host.
#[tokio::test]
#[serial_test::serial]
async fn control_plane_leg_to_the_same_loopback_target_still_follows_http_proxy() {
    let trap = ProxyTrap::start().await;
    let _env = proxy_env_only("HTTP_PROXY", &trap.url);
    let target = reinject_target().await;
    let state = forward_state();

    let status = forward_bounded(
        &state,
        &target.uri(),
        Leg::ControlPlane,
        PathClass::ControlPlane,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "the trap answered");
    let captures = trap.captures();
    assert_eq!(
        captures.len(),
        1,
        "exactly one proxied request: {captures:?}"
    );
    assert!(
        captures[0].starts_with(&format!("POST {}/v1/messages HTTP/1.1", target.uri())),
        "absolute-form request line expected: {:?}",
        captures[0]
    );
    assert!(target.received_requests().await.unwrap().is_empty());
}

/// Positive control for the external leg: an `https://` control-plane
/// forward under `HTTPS_PROXY` opens exactly one CONNECT tunnel through the
/// proxy. The trap refuses the tunnel, so the forward itself ends as a 502.
#[tokio::test]
#[serial_test::serial]
async fn control_plane_https_leg_issues_one_connect_through_https_proxy() {
    let trap = ProxyTrap::start().await;
    let _env = proxy_env_only("HTTPS_PROXY", &trap.url);
    let state = forward_state();

    let status = forward_bounded(
        &state,
        &format!("https://{SYNTHETIC_UPSTREAM_HOST}"),
        Leg::ControlPlane,
        PathClass::ControlPlane,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "refused tunnel is a send error"
    );
    let request_lines: Vec<String> = trap
        .captures()
        .iter()
        .map(|raw| raw.lines().next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        request_lines,
        vec![format!("CONNECT {SYNTHETIC_UPSTREAM_HOST}:443 HTTP/1.1")]
    );
    assert!(
        !trap.captures()[0].contains(BEARER_SENTINEL),
        "the tunnel request must not carry the forwarded bearer"
    );
}
