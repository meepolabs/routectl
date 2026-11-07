//! Token-endpoint transport: the request URL is checked after the request
//! is built and before anything is sent, and the response body is refused
//! once it would cross the cap, whether or not a length is declared.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::{MAX_TOKEN_BODY_BYTES, TokenEndpoint, read_capped_body, send_token_request};
use crate::oauth::OAuthError;

const TOKEN_HOST: &str = "token.example.invalid";
const OTHER_HOST: &str = "other.example.invalid";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A loopback listener that counts and immediately drops every connection.
/// Every test host resolves to it, so a request that escaped the URL check
/// shows up as a counted connection instead of leaving the machine.
struct CountingListener {
    port: u16,
    accepted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl CountingListener {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("listener address").port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(conn);
            }
        });
        Self {
            port,
            accepted,
            task,
        }
    }

    fn client(&self) -> reqwest::Client {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.port));
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(TIMEOUT)
            .timeout(TIMEOUT)
            .resolve(TOKEN_HOST, addr)
            .resolve(OTHER_HOST, addr)
            .build()
            .expect("build client")
    }

    fn connections(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for CountingListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn endpoint(url: String, allowed_host: &'static str) -> TokenEndpoint {
    TokenEndpoint {
        url: Box::leak(url.into_boxed_str()),
        client_id: "client-a",
        allowed_host,
    }
}

async fn post(listener: &CountingListener, endpoint: &TokenEndpoint) -> Result<(), OAuthError> {
    send_token_request(&listener.client(), endpoint, "token endpoint POST", |rb| {
        rb.form(&[("grant_type", "refresh_token")])
    })
    .await
    .map(|_| ())
}

/// Builds a row's endpoint URL from the listener's port.
type UrlForPort = fn(u16) -> String;

#[tokio::test]
async fn send_token_request_refuses_before_sending_off_rule_endpoint() {
    // Each row breaks exactly one rule and names a host that resolves to the
    // listener (the port row names the listener's own port), so a request
    // sent past the check would be counted.
    let rows: [(&str, UrlForPort, &'static str); 4] = [
        ("http", |_| format!("http://{TOKEN_HOST}/token"), TOKEN_HOST),
        (
            "host mismatch",
            |_| format!("https://{OTHER_HOST}/token"),
            TOKEN_HOST,
        ),
        (
            "userinfo",
            |_| format!("https://user:pw@{TOKEN_HOST}/token"),
            TOKEN_HOST,
        ),
        (
            "explicit port",
            |port| format!("https://{TOKEN_HOST}:{port}/token"),
            TOKEN_HOST,
        ),
    ];
    for (row, url, allowed_host) in rows {
        // Arrange
        let listener = CountingListener::start().await;
        let endpoint = endpoint(url(listener.port), allowed_host);

        // Act
        let result = post(&listener, &endpoint).await;

        // Assert
        assert!(
            matches!(result, Err(OAuthError::TokenEndpoint(_))),
            "{row}: {result:?}"
        );
        assert_eq!(listener.connections(), 0, "{row}: a connection was made");
    }
}

#[tokio::test]
async fn send_token_request_sends_an_on_rule_endpoint() {
    // Control for the refusal test: the same fixture does count a request
    // whose URL passes the check.
    let listener = CountingListener::start().await;
    let endpoint = endpoint(format!("https://{TOKEN_HOST}/token"), TOKEN_HOST);

    let result = post(&listener, &endpoint).await;

    assert!(matches!(result, Err(OAuthError::Network(_))), "{result:?}");
    assert_eq!(listener.connections(), 1);
}

/// Serve one raw HTTP/1.1 response to the first connection, then close.
/// Returns the URL to GET.
async fn serve_raw_once(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match conn.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let _ = conn.write_all(&response).await;
        let _ = conn.shutdown().await;
    });
    format!("http://{addr}/token")
}

async fn fetch(response: Vec<u8>) -> reqwest::Response {
    let url = serve_raw_once(response).await;
    reqwest::Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
        .expect("build client")
        .get(url)
        .send()
        .await
        .expect("response head arrives")
}

fn chunked_response(total: usize) -> Vec<u8> {
    const FRAME: usize = 16 * 1024;
    let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let mut left = total;
    while left > 0 {
        let n = left.min(FRAME);
        out.extend_from_slice(format!("{n:x}\r\n").as_bytes());
        out.extend(std::iter::repeat_n(b'x', n));
        out.extend_from_slice(b"\r\n");
        left -= n;
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

fn assert_over_cap(result: Result<String, OAuthError>, row: &str) {
    match result {
        Err(OAuthError::TokenEndpoint(m)) => {
            assert!(m.contains("refusing to load"), "{row}: {m}");
            assert!(m.contains(&MAX_TOKEN_BODY_BYTES.to_string()), "{row}: {m}");
        }
        other => panic!("{row}: expected over-cap refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn capped_body_read_refuses_declared_length_over_cap_before_reading() {
    // The body is short of the declared length and the connection closes, so
    // only the declared-length check can produce the over-cap refusal; a read
    // would end in an incomplete-body transport error instead.
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nxx",
        MAX_TOKEN_BODY_BYTES + 1
    );
    let resp = fetch(head.into_bytes()).await;

    assert_over_cap(read_capped_body(resp).await, "declared length");
}

#[tokio::test]
async fn capped_body_read_refuses_undeclared_chunked_body_over_cap() {
    let resp = fetch(chunked_response(MAX_TOKEN_BODY_BYTES + 1)).await;
    assert_eq!(resp.content_length(), None, "fixture declares no length");

    assert_over_cap(read_capped_body(resp).await, "chunked");
}

#[tokio::test]
async fn capped_body_read_accepts_chunked_body_at_cap() {
    let resp = fetch(chunked_response(MAX_TOKEN_BODY_BYTES)).await;
    assert_eq!(resp.content_length(), None, "fixture declares no length");

    let body = read_capped_body(resp)
        .await
        .expect("at-cap body is accepted");

    assert_eq!(body.len(), MAX_TOKEN_BODY_BYTES);
}

fn synthetic_response(body: Vec<u8>) -> reqwest::Response {
    let http_resp: http::Response<bytes::Bytes> = http::Response::builder()
        .status(200)
        .body(bytes::Bytes::from(body))
        .expect("build http response");
    reqwest::Response::from(http_resp)
}

#[tokio::test]
async fn capped_body_read_bounds_a_known_length_body_at_the_cap() {
    // A buffered body reports its exact length, so these rows sit on the
    // declared-length boundary.
    let at_cap = read_capped_body(synthetic_response(vec![b'x'; MAX_TOKEN_BODY_BYTES])).await;
    assert_eq!(at_cap.map(|b| b.len()).ok(), Some(MAX_TOKEN_BODY_BYTES));

    let over = read_capped_body(synthetic_response(vec![b'x'; MAX_TOKEN_BODY_BYTES + 1])).await;
    assert_over_cap(over, "known length over cap");
}

#[tokio::test]
async fn capped_body_read_rejects_non_utf8_response() {
    // Invalid UTF-8 bytes must hard-error, never silently lossy-decode, and
    // the error text must not echo the body.
    let resp = synthetic_response(vec![0xff, 0xfe, 0x80, 0x00]);

    match read_capped_body(resp).await {
        Err(OAuthError::TokenEndpoint(m)) => {
            assert!(m.contains("not valid UTF-8"), "got: {m}");
            assert!(!m.contains('\u{fffd}'), "must not echo lossy body: {m}");
        }
        other => panic!("expected TokenEndpoint UTF-8 error, got {other:?}"),
    }
}
