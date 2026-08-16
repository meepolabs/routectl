//! Integration tests for the MITM front-proxy's forward-leg transport
//! primitive (`proxy::forward`), exercised against a real upstream
//! rather than the in-process unit tests already covering the pure
//! helper functions.
//!
//! Two upstream doubles are used:
//!   - `wiremock` for the simple request/response scenarios (status +
//!     body verbatim, `..` rejection, hop-by-hop stripping on both
//!     legs).
//!   - a hand-rolled raw-TCP HTTP/1.1 server for the SSE-survives-
//!     streaming scenario, because wiremock has no way to make an
//!     individual mocked response emit its body in separate chunks
//!     with a real gap in between -- which is exactly the behavior
//!     under test (a forwarder that buffered the whole body before
//!     returning would collapse that gap).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::BodyExt;
use routectl_cli::proxy::forward::{ForwardRequest, ForwardState, build_client, forward};
use routectl_cli::proxy::metrics::{Leg, PathClass, ProxyMetrics, ResultClass};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_state() -> ForwardState {
    ForwardState::new(build_client().unwrap(), 8, Duration::from_secs(30))
}

fn get_request(raw_path_and_query: &str) -> ForwardRequest {
    ForwardRequest {
        method: Method::GET,
        raw_path_and_query: raw_path_and_query.to_string(),
        headers: HeaderMap::new(),
        body: reqwest::Body::from(Vec::new()),
    }
}

#[tokio::test]
async fn forwards_upstream_status_and_body_verbatim() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"hello upstream".to_vec(), "text/plain"),
        )
        .mount(&mock_server)
        .await;

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = collect_body(response).await;
    assert_eq!(body, Bytes::from_static(b"hello upstream"));
}

#[tokio::test]
async fn rejects_dotdot_tail_with_400_without_touching_upstream() {
    let mock_server = MockServer::start().await;
    // No mock mounted at all: if the forwarder ever reached the
    // network, wiremock's default 404-on-unmatched would still not be
    // a 400, making a wrong implementation observable either way.
    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/../secret"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock_server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn accepts_a_clean_tail_with_the_same_upstream_base() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages?beta=1"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn returns_the_upstream_redirect_verbatim_instead_of_following_it() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://example.invalid/elsewhere"),
        )
        .mount(&mock_server)
        .await;

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    // A dumb forwarder must hand back the 3xx + Location verbatim, not
    // silently chase the redirect and return whatever is at the other
    // end (which here doesn't even resolve).
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "https://example.invalid/elsewhere"
    );
}

#[tokio::test]
async fn returns_clean_502_when_upstream_is_unreachable() {
    // Unreachability is produced by PRESENCE, not absence: the listener
    // stays bound for the whole test, so no sibling test's ephemeral
    // bind can claim the port and answer the request, and every
    // connection it accepts is reset instead of served. Reserving a port
    // by dropping a listener leaves it free for the taking in the window
    // between the drop and the request.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap();

    let resetter = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            // Closed without reading the request and without writing any
            // response, so the peer's send fails at the transport level
            // rather than parsing an orderly reply. Whether it observes a
            // reset or an orderly close depends on scheduling; both are
            // send errors, which is what the assertions below pin.
            drop(socket);
        }
    });

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&format!("http://{dead_addr}")).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // The 502 has to be the transport-failure one specifically: a 502
    // booked as a client or server fault would mean the forwarder
    // misattributed an unreachable upstream, and a 502 synthesized from
    // an upstream response would carry a body.
    assert_eq!(
        metrics.request_count(
            Leg::Inference,
            ResultClass::Unreachable,
            PathClass::Inference
        ),
        1
    );
    assert_eq!(metrics.requests_total(), 1);
    assert!(collect_body(response).await.is_empty());

    resetter.abort();
}

#[tokio::test]
async fn strips_hop_by_hop_headers_on_the_outbound_leg() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let mut request = get_request("/v1/messages");
    request
        .headers
        .insert("connection", "close".parse().unwrap());
    request
        .headers
        .insert("proxy-authorization", "Basic evil".parse().unwrap());
    request
        .headers
        .insert("x-request-id", "keep-me".parse().unwrap());

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        request,
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].headers.get("connection").is_none());
    assert!(received[0].headers.get("proxy-authorization").is_none());
    assert_eq!(received[0].headers.get("x-request-id").unwrap(), "keep-me");
}

#[tokio::test]
async fn strips_hop_by_hop_headers_on_the_inbound_leg() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("connection", "close")
                .insert_header("x-request-id", "keep-me"),
        )
        .mount(&mock_server)
        .await;

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&mock_server.uri()).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("connection").is_none());
    assert_eq!(response.headers().get("x-request-id").unwrap(), "keep-me");
}

/// A raw HTTP/1.1 server (not wiremock) that writes a chunked SSE-
/// shaped body in two separate `write_all` calls with a real sleep in
/// between, then confirms the forwarder yields the first chunk before
/// the second one has even been sent -- the signal that the body is
/// streamed through and not buffered in full before `forward` returns.
#[tokio::test]
async fn sse_response_survives_streaming_without_being_buffered() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request_buf = [0u8; 1024];
        // Drain the request line + headers (up to the blank line)
        // well enough for this single-shot test double.
        let _ = socket.read(&mut request_buf).await.unwrap();

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  \r\n",
            )
            .await
            .unwrap();

        let first_chunk = b"data: first\n\n";
        socket
            .write_all(format!("{:x}\r\n", first_chunk.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(first_chunk).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        socket.flush().await.unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;

        let second_chunk = b"data: second\n\n";
        socket
            .write_all(format!("{:x}\r\n", second_chunk.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(second_chunk).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        socket.flush().await.unwrap();
    });

    let state = test_state();
    let metrics = Arc::new(ProxyMetrics::new());
    let upstream_base = reqwest::Url::parse(&format!("http://{addr}")).unwrap();

    let response = forward(
        &state,
        &metrics,
        &upstream_base,
        get_request("/v1/messages"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let started = tokio::time::Instant::now();
    let mut body = response.into_body().into_data_stream();

    let first = body.next().await.unwrap().unwrap();
    let elapsed_for_first = started.elapsed();
    assert_eq!(first, Bytes::from_static(b"data: first\n\n"));
    assert!(
        elapsed_for_first < Duration::from_millis(150),
        "the first chunk must arrive before the second one is even sent \
         (buffered elapsed: {elapsed_for_first:?})"
    );

    let second = body.next().await.unwrap().unwrap();
    assert_eq!(second, Bytes::from_static(b"data: second\n\n"));
    assert!(body.next().await.is_none());

    server.await.unwrap();
}

async fn collect_body(
    response: http::Response<routectl_cli::proxy::forward::ForwardBody>,
) -> Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}
