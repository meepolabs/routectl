use super::{
    AUTH_HEADERS, CONNECT_TIMEOUT, MANAGED_HEADERS, MAX_RESPONSE_BODY_BYTES, STREAM_READ_TIMEOUT,
    apply_header_extras, insert_header, is_auth_header, is_managed_header, read_body_capped,
};
use reqwest::header::HeaderMap;

#[test]
fn stream_read_timeout_is_generous_idle_cap() {
    assert_eq!(
        STREAM_READ_TIMEOUT,
        std::time::Duration::from_mins(5),
        "streaming idle read timeout must be 300s",
    );
}

#[test]
fn connect_timeout_is_short_handshake_cap() {
    assert_eq!(
        CONNECT_TIMEOUT,
        std::time::Duration::from_secs(10),
        "connect (TCP + TLS) timeout must be 10s",
    );
}

#[test]
fn common_builder_applies_without_panicking_with_read_timeout() {
    let _client = super::common_builder(Some("test-ua"), "https://api.example.test")
        .build()
        .expect("client build must not fail on a sane TLS store");
}

#[test]
fn https_base_urls_keep_system_proxy_discovery() {
    for base in [
        "https://api.anthropic.com",
        "HTTPS://api.example.test/v1",
        "https://bedrock-mantle.us-west-2.api.aws/openai/v1",
        "https://[2001:db8::1]:8443/v1",
    ] {
        assert!(
            super::uses_system_proxy(base),
            "{base:?} must keep system proxy discovery"
        );
    }
}

#[test]
fn cleartext_base_urls_are_dialed_directly() {
    for base in [
        "http://127.0.0.1:8080/v1",
        "http://localhost:11434",
        "http://[::1]:8080",
        "http://[::ffff:127.0.0.1]:8080",
        "HTTP://127.0.0.1/v1",
    ] {
        assert!(
            !super::uses_system_proxy(base),
            "{base:?} must never use a system proxy"
        );
    }
}

#[test]
fn malformed_or_unknown_scheme_base_urls_never_gain_a_proxy() {
    for base in [
        "",
        "   ",
        "api.anthropic.com",
        "https//api.example.test",
        "//api.example.test",
        "ftp://api.example.test",
        "socks5://127.0.0.1:1080",
        "https-evil://api.example.test",
        "http://\u{0}",
        "https://exa mple.test",
    ] {
        assert!(
            !super::uses_system_proxy(base),
            "{base:?} must fail safe to a direct client"
        );
    }
}

fn with_scheme(url: &str, scheme: &str) -> String {
    url.replacen("http://", &format!("{scheme}://"), 1)
}

#[test]
fn loopback_base_urls_bypass_system_proxies_over_http_and_https() {
    for url in routectl_testkit::loopback_vectors::LOOPBACK_BASE_URLS {
        for scheme in ["http", "https"] {
            let base = with_scheme(url, scheme);

            let uses_proxy = super::uses_system_proxy(&base);

            assert!(
                !uses_proxy,
                "{base:?} is loopback and must be dialed directly"
            );
        }
    }
}

#[test]
fn non_loopback_base_urls_keep_system_proxy_discovery_over_http_and_https() {
    for url in routectl_testkit::loopback_vectors::NON_LOOPBACK_BASE_URLS {
        for scheme in ["http", "https"] {
            let base = with_scheme(url, scheme);

            let uses_proxy = super::uses_system_proxy(&base);

            assert!(
                uses_proxy,
                "{base:?} is not loopback and must keep system proxy discovery"
            );
        }
    }
}

#[test]
fn insert_header_inserts_valid_pair() {
    let mut map = HeaderMap::new();
    insert_header(&mut map, "p", "x-custom", "value");
    assert_eq!(map.get("x-custom").unwrap(), "value");
}

#[test]
fn insert_header_replaces_existing_same_name() {
    let mut map = HeaderMap::new();
    insert_header(&mut map, "p", "x-custom", "first");
    insert_header(&mut map, "p", "x-custom", "second");
    // insert (not append) -> exactly one value, the latest.
    assert_eq!(map.get_all("x-custom").iter().count(), 1);
    assert_eq!(map.get("x-custom").unwrap(), "second");
}

#[test]
fn insert_header_skips_malformed_name_without_panic() {
    let mut map = HeaderMap::new();
    // A space is illegal in a header name; WARN+skip, no insert.
    insert_header(&mut map, "p", "bad name", "value");
    assert!(map.is_empty());
}

#[test]
fn insert_header_skips_malformed_value_without_panic() {
    let mut map = HeaderMap::new();
    // A newline is illegal in a header value; WARN+skip, no insert.
    insert_header(&mut map, "p", "x-custom", "bad\nvalue");
    assert!(map.is_empty());
}

#[test]
fn apply_header_extras_inserts_plain_headers() {
    let mut map = HeaderMap::new();
    let extras = vec![("x-foo".to_string(), "1".to_string())];
    apply_header_extras(&mut map, &extras, "p", &[]);
    assert_eq!(map.get("x-foo").unwrap(), "1");
}

#[test]
fn apply_header_extras_skips_auth_reserved() {
    let mut map = HeaderMap::new();
    let extras = vec![
        ("authorization".to_string(), "Bearer x".to_string()),
        ("x-api-key".to_string(), "k".to_string()),
        ("x-amz-date".to_string(), "20260101".to_string()),
        ("x-foo".to_string(), "1".to_string()),
    ];
    apply_header_extras(&mut map, &extras, "p", &[]);
    assert!(map.get("authorization").is_none());
    assert!(map.get("x-api-key").is_none());
    assert!(map.get("x-amz-date").is_none());
    // Non-reserved entry still lands.
    assert_eq!(map.get("x-foo").unwrap(), "1");
}

#[test]
fn apply_header_extras_skips_managed() {
    let mut map = HeaderMap::new();
    let extras = vec![
        ("content-type".to_string(), "text/plain".to_string()),
        ("host".to_string(), "evil".to_string()),
    ];
    apply_header_extras(&mut map, &extras, "p", &[]);
    assert!(map.is_empty());
}

#[test]
fn apply_header_extras_skips_list_valued_names() {
    let mut map = HeaderMap::new();
    let extras = vec![
        ("anthropic-beta".to_string(), "ctx-1m".to_string()),
        ("x-foo".to_string(), "1".to_string()),
    ];
    // anthropic-beta is list-valued (composed by routectl) -> skip;
    // x-foo is plain -> insert.
    apply_header_extras(&mut map, &extras, "p", &["anthropic-beta"]);
    assert!(map.get("anthropic-beta").is_none());
    assert_eq!(map.get("x-foo").unwrap(), "1");
}

#[test]
fn apply_header_extras_list_valued_is_case_insensitive() {
    let mut map = HeaderMap::new();
    let extras = vec![("Anthropic-Beta".to_string(), "ctx-1m".to_string())];
    apply_header_extras(&mut map, &extras, "p", &["anthropic-beta"]);
    assert!(map.get("anthropic-beta").is_none());
}

#[test]
fn apply_header_extras_empty_list_valued_keeps_anthropic_beta() {
    // With list_valued = &[], anthropic-beta is just a plain header
    // (the non-anthropic providers don't compose it themselves).
    let mut map = HeaderMap::new();
    let extras = vec![("anthropic-beta".to_string(), "ctx-1m".to_string())];
    apply_header_extras(&mut map, &extras, "p", &[]);
    assert_eq!(map.get("anthropic-beta").unwrap(), "ctx-1m");
}

#[test]
fn is_auth_header_matches_auth_names() {
    for name in ["authorization", "Authorization", "AUTHORIZATION"] {
        assert!(is_auth_header(name), "{name:?} should classify as auth");
    }
    for name in ["x-api-key", "X-Api-Key", "X-API-KEY"] {
        assert!(is_auth_header(name), "{name:?} should classify as auth");
    }
    for name in [
        "anthropic-version",
        "Anthropic-Version",
        "ANTHROPIC-VERSION",
    ] {
        assert!(is_auth_header(name), "{name:?} should classify as auth");
    }
    for name in [
        "chatgpt-account-id",
        "ChatGPT-Account-Id",
        "CHATGPT-ACCOUNT-ID",
    ] {
        assert!(is_auth_header(name), "{name:?} should classify as auth");
    }
    for name in ["anthropic-beta", "content-type", "host", "x-request-id"] {
        assert!(!is_auth_header(name), "{name:?} must NOT classify as auth");
    }
}

/// Any header with an `x-amz-` prefix is auth-reserved on the
/// Bedrock path because SigV4 signs the request before these
/// headers are added. An extra `x-amz-*` injected after signing
/// would not appear in the signed string, invalidating the
/// signature.
#[test]
fn is_auth_header_treats_x_amz_prefix_as_reserved() {
    for name in [
        "x-amz-date",
        "X-Amz-Date",
        "X-AMZ-DATE",
        "x-amz-security-token",
        "x-amz-content-sha256",
        "x-amz-target",
    ] {
        assert!(
            is_auth_header(name),
            "{name:?} with x-amz- prefix must classify as auth-reserved"
        );
    }
    // Sanity: a non-x-amz header is not affected.
    assert!(!is_auth_header("x-custom-header"));
}

#[test]
fn is_managed_header_does_not_contain_anthropic_beta() {
    // v0.6.0 removed `anthropic-beta` from the managed list.
    // Operators now own the per-provider and per-model values
    // via `header_extras`; the router's dispatch-layer compose
    // unions inbound HTTP header + provider + model into one
    // comma-joined header.
    assert!(
        !is_managed_header("anthropic-beta"),
        "anthropic-beta MUST NOT classify as managed in v0.6.0+",
    );
    assert!(
        !is_managed_header("Anthropic-Beta"),
        "case-insensitive: Anthropic-Beta MUST NOT be managed",
    );
}

#[test]
fn is_managed_header_matches_managed_names() {
    for name in ["host", "Host", "HOST"] {
        assert!(
            is_managed_header(name),
            "{name:?} should classify as managed"
        );
    }
    for name in ["content-type", "Content-Type", "CONTENT-TYPE"] {
        assert!(
            is_managed_header(name),
            "{name:?} should classify as managed"
        );
    }
    for name in ["content-length", "Content-Length"] {
        assert!(
            is_managed_header(name),
            "{name:?} should classify as managed"
        );
    }
    for name in [
        "authorization",
        "x-api-key",
        "anthropic-version",
        "x-request-id",
    ] {
        assert!(
            !is_managed_header(name),
            "{name:?} must NOT classify as managed"
        );
    }
}

#[test]
fn is_auth_and_managed_are_disjoint() {
    // No header should be classified as BOTH auth and managed --
    // the WARN/DEBUG branch in caller code depends on this, and a
    // future addition that lands in both lists would double-log.
    for &h in AUTH_HEADERS {
        assert!(
            !MANAGED_HEADERS.contains(&h),
            "header {h:?} appears in both AUTH and MANAGED lists",
        );
    }
    for &h in MANAGED_HEADERS {
        assert!(
            !AUTH_HEADERS.contains(&h),
            "header {h:?} appears in both AUTH and MANAGED lists",
        );
    }
}

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spawn a one-shot raw TCP server that replies with a chunked
/// (no `Content-Length`) body of `total` bytes split into `chunk_size`
/// pieces, then returns the base URL to GET. wiremock always sets an
/// honest `Content-Length` -- which the fast-reject guard would
/// short-circuit -- so a chunked upstream is the only way to drive the
/// mid-transfer running-total guard against a real socket.
async fn spawn_chunked_server(total: usize, chunk_size: usize) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await;
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: application/octet-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  \r\n",
            )
            .await;
        let mut sent = 0usize;
        while sent < total {
            let this = chunk_size.min(total - sent);
            let _ = socket.write_all(format!("{this:x}\r\n").as_bytes()).await;
            let _ = socket.write_all(&vec![b'a'; this]).await;
            let _ = socket.write_all(b"\r\n").await;
            sent += this;
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
        let _ = socket.flush().await;
    });
    format!("http://{addr}")
}

#[test]
fn max_response_body_cap_is_16_mib() {
    assert_eq!(MAX_RESPONSE_BODY_BYTES, 16 * 1024 * 1024);
}

#[tokio::test]
async fn read_body_capped_fast_rejects_honest_content_length_over_cap() {
    // wiremock computes an honest Content-Length from the body, so a
    // body over the cap is rejected by the header check before a single
    // body byte is streamed -- `bytes` comes back empty.
    let cap = 100;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; cap * 4]))
        .mount(&server)
        .await;
    let resp = reqwest::get(server.uri()).await.unwrap();

    let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

    assert!(hit_cap, "honest Content-Length over cap must trip hit_cap");
    assert!(
        bytes.is_empty(),
        "fast-reject must not read the body: got {} bytes",
        bytes.len()
    );
}

#[tokio::test]
async fn read_body_capped_returns_under_cap_body_intact() {
    let cap = 1024;
    let body = vec![b'x'; 256];
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    let resp = reqwest::get(server.uri()).await.unwrap();

    let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

    assert!(!hit_cap, "an under-cap body must not trip hit_cap");
    assert_eq!(bytes, body, "under-cap body must be returned intact");
}

#[tokio::test]
async fn read_body_capped_trips_mid_transfer_on_chunked_body_over_cap() {
    // A chunked upstream sends no Content-Length, so the fast-reject
    // cannot see the size -- the running-total guard must catch it and
    // truncate the prefix to the cap. This is the "content-length lie":
    // an absent/understated length only the mid-transfer check defends.
    let cap = 512;
    let url = spawn_chunked_server(cap * 8, 128).await;
    let resp = reqwest::get(url).await.unwrap();

    let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

    assert!(hit_cap, "a chunked body over cap must trip mid-transfer");
    assert!(
        bytes.len() <= cap,
        "prefix must be truncated to cap: got {} > {cap}",
        bytes.len()
    );
}

#[tokio::test]
async fn read_body_capped_reads_chunked_body_under_cap_fully() {
    // The streaming path (no Content-Length) reads every chunk when the
    // running total stays under the cap.
    let cap = 4096;
    let total = 900;
    let url = spawn_chunked_server(total, 128).await;
    let resp = reqwest::get(url).await.unwrap();

    let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

    assert!(!hit_cap, "an under-cap chunked body must not trip hit_cap");
    assert_eq!(bytes.len(), total, "all chunks must be read");
}

#[tokio::test]
async fn read_body_capped_bounds_peak_at_cap_when_one_chunk_straddles_it() {
    // A single chunk larger than the cap must be truncated to exactly
    // `cap` -- peak buffered bytes never exceed the ceiling even when
    // one chunk alone would cross it.
    let cap = 500;
    let url = spawn_chunked_server(cap * 3, cap * 3).await;
    let resp = reqwest::get(url).await.unwrap();

    let (bytes, hit_cap) = read_body_capped(resp, cap).await.unwrap();

    assert!(hit_cap, "an over-cap single chunk must trip mid-transfer");
    assert_eq!(bytes.len(), cap, "prefix must be bounded to exactly cap");
}

/// Empirical proof that a single oversized HTTP/1.1 chunked frame does
/// NOT materialize as one giant `Bytes` from `resp.chunk()`. An
/// upstream declares one 4 MiB wire chunk; hyper's HTTP/1 read buffer
/// (adaptive strategy, capped at DEFAULT_MAX_BUFFER_SIZE ~= 408 KiB for
/// the pinned hyper + reqwest, which do not override http1_max_buf_size)
/// slices that frame into a sequence of small `Bytes`. This is the fact
/// the `read_body_capped` loop relies on: transient per-iteration
/// allocation stays far below the 16 MiB cap regardless of the wire
/// chunk size, so the cap check trips before any large buffer forms.
#[tokio::test]
async fn single_wire_chunk_is_yielded_as_bounded_frames() {
    // One 4 MiB declared chunk, sent as a single wire chunk.
    let total = 4 * 1024 * 1024;
    let url = spawn_chunked_server(total, total).await;
    let mut resp = reqwest::get(url).await.unwrap();

    let mut seen = 0usize;
    let mut max_frame = 0usize;
    while let Some(chunk) = resp.chunk().await.unwrap() {
        seen += chunk.len();
        max_frame = max_frame.max(chunk.len());
    }

    assert_eq!(seen, total, "the whole body must be delivered");
    // The largest single frame must sit well under the whole wire chunk
    // and far below the 16 MiB response cap. 512 KiB gives headroom over
    // the ~408 KiB hyper read-buffer ceiling without admitting a
    // multi-megabyte single allocation.
    assert!(
        max_frame <= 512 * 1024,
        "a single chunk() frame must stay below the read-buffer bound: \
         got {max_frame} bytes from a {total}-byte wire chunk",
    );
    assert!(
        max_frame < MAX_RESPONSE_BODY_BYTES,
        "per-frame allocation must be far below the response cap",
    );
}
