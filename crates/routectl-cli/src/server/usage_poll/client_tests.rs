use std::net::SocketAddr;
use std::time::Duration;

use routectl_auth::oauth::SecretToken;
use routectl_router::MAX_USAGE_BODY_BYTES;
use routectl_router::config::ProviderEntry;
use routectl_testkit::tls_front::TlsFront;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::{
    ANTHROPIC_USAGE_URL, CODEX_USAGE_URL, FetchResult, MAX_USAGE_RETRY_AFTER, SeatCredential,
    UsagePollClient,
};

const ANTHROPIC_HOST: &str = "api.anthropic.com";
const CODEX_HOST: &str = "chatgpt.com";
const TOKEN_SENTINEL: &str = "synthetic-usage-token-5d1e";
const ACCOUNT_SENTINEL: &str = "synthetic-account-id-a83c";
const BODY_MARKER: &str = "synthetic-body-marker-77b0";

/// One mock per vendor host, each reached through a TLS front for that
/// host, and a client that resolves both hosts to their fronts.
struct Stubs {
    anthropic: MockServer,
    codex: MockServer,
    client: UsagePollClient,
    _fronts: (TlsFront, TlsFront),
}

impl Stubs {
    async fn start() -> Self {
        let anthropic = MockServer::start().await;
        let codex = MockServer::start().await;
        let anthropic_front = TlsFront::start(ANTHROPIC_HOST, *anthropic.address()).await;
        let codex_front = TlsFront::start(CODEX_HOST, *codex.address()).await;
        let client = UsagePollClient::with_fronts_for_tests(&[
            (ANTHROPIC_HOST, &anthropic_front),
            (CODEX_HOST, &codex_front),
        ]);
        Self {
            anthropic,
            codex,
            client,
            _fronts: (anthropic_front, codex_front),
        }
    }

    async fn answer_anthropic(&self, response: ResponseTemplate) {
        self.anthropic.reset().await;
        Mock::given(any())
            .respond_with(response)
            .mount(&self.anthropic)
            .await;
    }

    async fn fetch_anthropic(&self) -> FetchResult {
        let token = SecretToken::new(TOKEN_SENTINEL);
        self.client
            .fetch(SeatCredential::Anthropic { token: &token })
            .await
    }

    async fn fetch_codex(&self) -> FetchResult {
        let token = SecretToken::new(TOKEN_SENTINEL);
        self.client
            .fetch(SeatCredential::Codex {
                token: &token,
                account_id: Some(ACCOUNT_SENTINEL),
            })
            .await
    }
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

fn managed_base_host(vendor: &str) -> String {
    let entry = ProviderEntry::for_managed_login(vendor, format!("oauth://{vendor}"))
        .expect("a managed vendor");
    let base = entry.base_url.expect("a managed entry has a base URL");
    reqwest::Url::parse(&base)
        .expect("the managed base URL parses")
        .host_str()
        .expect("the managed base URL has a host")
        .to_string()
}

#[test]
fn each_usage_url_is_https_on_its_vendors_managed_host() {
    for (vendor, url) in [
        ("anthropic", ANTHROPIC_USAGE_URL),
        ("codex", CODEX_USAGE_URL),
    ] {
        let parsed = reqwest::Url::parse(url).expect("the usage URL parses");

        assert_eq!(parsed.scheme(), "https", "{vendor}");
        assert_eq!(parsed.port(), None, "{vendor}");
        assert_eq!(parsed.username(), "", "{vendor}");
        assert_eq!(
            parsed.host_str(),
            Some(managed_base_host(vendor).as_str()),
            "{vendor}"
        );
    }
}

#[test]
fn the_production_client_targets_only_the_usage_constants() {
    let client = UsagePollClient::new().expect("the production client builds");
    let token = SecretToken::new(TOKEN_SENTINEL);
    let credentials = [
        (
            ANTHROPIC_USAGE_URL,
            SeatCredential::Anthropic { token: &token },
        ),
        (
            CODEX_USAGE_URL,
            SeatCredential::Codex {
                token: &token,
                account_id: None,
            },
        ),
    ];

    for (url, credential) in credentials {
        let request = client.request(credential).build().expect("request builds");

        assert_eq!(request.url().as_str(), url);
        assert_eq!(request.method(), reqwest::Method::GET);
        assert!(request.headers().get("chatgpt-account-id").is_none());
    }
}

#[test]
fn the_bearer_and_account_id_are_sensitive_and_absent_from_request_debug() {
    let client = UsagePollClient::new().expect("the production client builds");
    let token = SecretToken::new(TOKEN_SENTINEL);
    let credentials = [
        ("anthropic", SeatCredential::Anthropic { token: &token }),
        (
            "codex",
            SeatCredential::Codex {
                token: &token,
                account_id: Some(ACCOUNT_SENTINEL),
            },
        ),
    ];

    for (vendor, credential) in credentials {
        let request = client.request(credential).build().expect("request builds");

        let headers = request.headers();
        assert!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .expect("a bearer")
                .is_sensitive(),
            "{vendor}: authorization"
        );
        if vendor == "codex" {
            assert!(
                headers
                    .get("chatgpt-account-id")
                    .expect("an account id")
                    .is_sensitive(),
                "{vendor}: account id"
            );
        }
        let rendered = format!("{request:?}");
        for secret in [TOKEN_SENTINEL, ACCOUNT_SENTINEL] {
            assert!(
                !rendered.contains(secret),
                "{vendor}: the request Debug carried a credential"
            );
        }
    }
}

#[tokio::test]
async fn anthropic_request_reaches_the_production_host_with_the_seat_bearer() {
    let stubs = Stubs::start().await;
    stubs
        .answer_anthropic(ResponseTemplate::new(200).set_body_string(BODY_MARKER))
        .await;

    let result = stubs.fetch_anthropic().await;

    assert_eq!(result, FetchResult::Ok(BODY_MARKER.as_bytes().to_vec()));
    let requests = stubs
        .anthropic
        .received_requests()
        .await
        .expect("recording");
    let [request] = requests.as_slice() else {
        panic!("expected one request, saw {}", requests.len());
    };
    assert_eq!(request.method.as_str(), "GET");
    assert_eq!(request.url.path(), "/api/oauth/usage");
    assert_eq!(header(request, "host"), Some(ANTHROPIC_HOST));
    let expected_bearer = format!("Bearer {TOKEN_SENTINEL}");
    assert_eq!(
        header(request, "authorization"),
        Some(expected_bearer.as_str())
    );
    assert_eq!(
        header(request, "user-agent"),
        Some(routectl_core::identity::anthropic::default_claude_code_user_agent())
    );
    assert_eq!(
        header(request, "anthropic-beta"),
        Some(routectl_core::identity::anthropic::OAUTH_ANTHROPIC_BETA)
    );
    assert!(header(request, "chatgpt-account-id").is_none());
}

#[tokio::test]
async fn codex_request_reaches_the_production_host_with_bearer_and_account_id() {
    let stubs = Stubs::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(BODY_MARKER))
        .mount(&stubs.codex)
        .await;

    let result = stubs.fetch_codex().await;

    assert_eq!(result, FetchResult::Ok(BODY_MARKER.as_bytes().to_vec()));
    let requests = stubs.codex.received_requests().await.expect("recording");
    let [request] = requests.as_slice() else {
        panic!("expected one request, saw {}", requests.len());
    };
    assert_eq!(request.url.path(), "/backend-api/wham/usage");
    assert_eq!(header(request, "host"), Some(CODEX_HOST));
    let expected_bearer = format!("Bearer {TOKEN_SENTINEL}");
    assert_eq!(
        header(request, "authorization"),
        Some(expected_bearer.as_str())
    );
    assert_eq!(
        header(request, "chatgpt-account-id"),
        Some(ACCOUNT_SENTINEL)
    );
    assert_eq!(
        header(request, "user-agent"),
        Some(routectl_core::identity::codex::codex_user_agent())
    );
    for (name, value) in routectl_core::identity::codex::default_identity_headers() {
        assert_eq!(header(request, name), Some(value), "{name}");
    }
    assert!(
        stubs
            .anthropic
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
}

#[tokio::test]
async fn a_redirect_to_another_reachable_host_is_not_followed() {
    let stubs = Stubs::start().await;
    stubs
        .answer_anthropic(
            ResponseTemplate::new(302).insert_header("location", "https://chatgpt.com/elsewhere"),
        )
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(BODY_MARKER))
        .mount(&stubs.codex)
        .await;

    let result = stubs.fetch_anthropic().await;

    assert_eq!(result, FetchResult::Redirect);
    let origin_hits = stubs
        .anthropic
        .received_requests()
        .await
        .expect("recording");
    assert_eq!(origin_hits.len(), 1);
    let target_hits = stubs.codex.received_requests().await.expect("recording");
    assert!(
        target_hits.is_empty(),
        "the redirect target saw {} request(s)",
        target_hits.len()
    );
}

#[tokio::test]
async fn every_3xx_status_is_a_redirect() {
    let stubs = Stubs::start().await;
    for status in 300..=399 {
        stubs
            .answer_anthropic(
                ResponseTemplate::new(status).insert_header("location", "https://chatgpt.com/x"),
            )
            .await;

        let result = stubs.fetch_anthropic().await;

        assert_eq!(result, FetchResult::Redirect, "status {status}");
    }
    assert!(
        stubs
            .codex
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
}

#[tokio::test]
async fn status_codes_map_to_their_outcomes() {
    let cases = [
        ("unauthorized", 401, FetchResult::Auth),
        ("forbidden", 403, FetchResult::Auth),
        ("not found", 404, FetchResult::Http(404)),
        ("server error", 500, FetchResult::Http(500)),
        ("no content", 204, FetchResult::Http(204)),
    ];
    let stubs = Stubs::start().await;
    for (name, status, expected) in cases {
        stubs
            .answer_anthropic(ResponseTemplate::new(status).set_body_string(BODY_MARKER))
            .await;

        let result = stubs.fetch_anthropic().await;

        assert_eq!(result, expected, "{name}");
    }
}

#[tokio::test]
async fn a_body_of_exactly_the_cap_is_returned_whole() {
    let stubs = Stubs::start().await;
    let body = vec![b' '; MAX_USAGE_BODY_BYTES];
    stubs
        .answer_anthropic(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .await;

    let result = stubs.fetch_anthropic().await;

    assert_eq!(result, FetchResult::Ok(body));
}

#[tokio::test]
async fn a_body_one_byte_over_the_cap_is_too_large() {
    let stubs = Stubs::start().await;
    stubs
        .answer_anthropic(
            ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_USAGE_BODY_BYTES + 1]),
        )
        .await;

    let result = stubs.fetch_anthropic().await;

    assert_eq!(result, FetchResult::TooLarge);
}

/// A plain-http backend that answers every connection with a chunked 200
/// (no `content-length`) carrying `total` bytes in 4 KiB chunks.
async fn chunked_backend(total: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let mut out = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
                let mut left = total;
                while left > 0 {
                    let n = left.min(4096);
                    out.extend_from_slice(format!("{n:x}\r\n").as_bytes());
                    out.extend(std::iter::repeat_n(b' ', n));
                    out.extend_from_slice(b"\r\n");
                    left -= n;
                }
                out.extend_from_slice(b"0\r\n\r\n");
                let _ = socket.write_all(&out).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn an_unannounced_body_is_cut_at_the_cap_while_streaming() {
    for (name, total, too_large) in [
        ("at the cap", MAX_USAGE_BODY_BYTES, false),
        ("one byte over", MAX_USAGE_BODY_BYTES + 1, true),
    ] {
        let backend = chunked_backend(total).await;
        let front = TlsFront::start(ANTHROPIC_HOST, backend).await;
        let client = UsagePollClient::with_fronts_for_tests(&[(ANTHROPIC_HOST, &front)]);
        let token = SecretToken::new(TOKEN_SENTINEL);

        let result = client
            .fetch(SeatCredential::Anthropic { token: &token })
            .await;

        if too_large {
            assert_eq!(result, FetchResult::TooLarge, "{name}");
        } else {
            assert_eq!(result, FetchResult::Ok(vec![b' '; total]), "{name}");
        }
    }
}

#[tokio::test]
async fn a_reply_slower_than_the_total_timeout_is_a_timeout() {
    let stubs = Stubs::start().await;
    stubs
        .answer_anthropic(ResponseTemplate::new(200).set_delay(Duration::from_secs(12)))
        .await;

    let result = stubs.fetch_anthropic().await;

    assert_eq!(result, FetchResult::Timeout);
}

/// A plain-http backend that sends a chunked 200 head and one chunk at
/// once, then holds the connection open without finishing the body.
async fn dripping_backend(hold: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let out = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n";
                if socket.write_all(out).await.is_err() || socket.flush().await.is_err() {
                    return;
                }
                tokio::time::sleep(hold).await;
                let _ = socket.write_all(b"1\r\n}\r\n0\r\n\r\n").await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_body_that_drips_past_the_total_timeout_is_a_timeout() {
    let backend = dripping_backend(Duration::from_secs(12)).await;
    let front = TlsFront::start(ANTHROPIC_HOST, backend).await;
    let client = UsagePollClient::with_fronts_for_tests(&[(ANTHROPIC_HOST, &front)]);
    let token = SecretToken::new(TOKEN_SENTINEL);

    let result = client
        .fetch(SeatCredential::Anthropic { token: &token })
        .await;

    assert_eq!(result, FetchResult::Timeout);
}

#[tokio::test]
async fn an_unreachable_host_is_a_transport_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let backend = listener.local_addr().expect("addr");
    drop(listener);
    let front = TlsFront::start(ANTHROPIC_HOST, backend).await;
    let client = UsagePollClient::with_fronts_for_tests(&[(ANTHROPIC_HOST, &front)]);
    let token = SecretToken::new(TOKEN_SENTINEL);

    let result = client
        .fetch(SeatCredential::Anthropic { token: &token })
        .await;

    assert_eq!(result, FetchResult::Transport);
}

fn http_date_in(wait: Duration) -> String {
    let when = time::OffsetDateTime::now_utc() + wait;
    let format = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    when.format(format).expect("an HTTP-date")
}

#[tokio::test]
async fn a_429_carries_its_retry_after_wait() {
    let stubs = Stubs::start().await;

    stubs
        .answer_anthropic(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .await;
    assert_eq!(
        stubs.fetch_anthropic().await,
        FetchResult::RateLimited(Some(Duration::from_mins(2))),
        "delta seconds"
    );

    let date = http_date_in(Duration::from_hours(1));
    stubs
        .answer_anthropic(ResponseTemplate::new(429).insert_header("retry-after", date.as_str()))
        .await;
    match stubs.fetch_anthropic().await {
        FetchResult::RateLimited(Some(wait)) => assert!(
            wait > Duration::from_mins(58) && wait <= Duration::from_hours(1),
            "HTTP-date wait {wait:?}"
        ),
        other => panic!("HTTP-date: expected a parsed wait, got {other:?}"),
    }

    stubs.answer_anthropic(ResponseTemplate::new(429)).await;
    assert_eq!(
        stubs.fetch_anthropic().await,
        FetchResult::RateLimited(None),
        "absent"
    );
}

#[tokio::test]
async fn a_429_wait_is_capped_at_one_hour() {
    let cases = [
        ("an hour and a second", "retry-after", "3601"),
        (
            "the largest delta seconds",
            "retry-after",
            "18446744073709551615",
        ),
        (
            "the largest milliseconds",
            "retry-after-ms",
            "18446744073709551615",
        ),
    ];
    let stubs = Stubs::start().await;
    for (name, header, value) in cases {
        stubs
            .answer_anthropic(ResponseTemplate::new(429).insert_header(header, value))
            .await;

        let result = stubs.fetch_anthropic().await;

        assert_eq!(
            result,
            FetchResult::RateLimited(Some(MAX_USAGE_RETRY_AFTER)),
            "{name}"
        );
    }
    assert_eq!(MAX_USAGE_RETRY_AFTER, Duration::from_hours(1));
}

#[tokio::test]
async fn no_result_rendering_carries_the_token_account_id_or_body() {
    let stubs = Stubs::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(BODY_MARKER))
        .mount(&stubs.codex)
        .await;
    let fetched = stubs.fetch_codex().await;
    assert_eq!(fetched, FetchResult::Ok(BODY_MARKER.as_bytes().to_vec()));
    let results = [
        fetched,
        FetchResult::Redirect,
        FetchResult::Auth,
        FetchResult::Http(500),
        FetchResult::Timeout,
        FetchResult::Transport,
        FetchResult::TooLarge,
        FetchResult::RateLimited(Some(Duration::from_secs(1))),
        FetchResult::RateLimited(None),
    ];

    for result in &results {
        for rendered in [format!("{result:?}"), format!("{result}")] {
            for secret in [TOKEN_SENTINEL, ACCOUNT_SENTINEL, BODY_MARKER] {
                assert!(
                    !rendered.contains(secret),
                    "{} rendering leaked a sentinel: {rendered}",
                    result.as_str()
                );
            }
        }
    }
    let client_debug = format!("{:?}", stubs.client);
    for secret in [TOKEN_SENTINEL, ACCOUNT_SENTINEL, BODY_MARKER] {
        assert!(!client_debug.contains(secret));
    }
}
