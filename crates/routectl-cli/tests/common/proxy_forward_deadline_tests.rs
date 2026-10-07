// Real loopback regressions for the two forward deadlines. Included from
// proxy_forward.rs to reuse the transport test helpers.

#[tokio::test]
async fn withheld_response_headers_timeout_and_release_the_shared_slot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        accepted_tx.send(()).unwrap();
        futures::future::pending::<()>().await; // hold FD; never send headers
    });
    let state = ForwardState::with_response_head_timeout(
        1,
        Duration::from_secs(1),
        Duration::from_millis(200),
    )
    .unwrap();
    let metrics = Arc::new(ProxyMetrics::new());
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        forward(
            &state,
            &metrics,
            &base,
            get_request("/stalled"),
            Leg::Inference,
            PathClass::Inference,
        ),
    )
    .await
    .expect("header acquisition must be bounded");
    accepted_rx.await.unwrap(); // positive control: the upstream was reached
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(metrics.streams_open(), 0);
    assert_eq!(
        metrics.request_count(
            Leg::Inference,
            ResultClass::Unreachable,
            PathClass::Inference
        ),
        1
    );

    // Other leg shares the single permit. Success proves the timed-out send
    // was cancelled and released it, not merely returned a synthetic status.
    let healthy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthy"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&healthy)
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        forward(
            &state,
            &metrics,
            &reqwest::Url::parse(&healthy.uri()).unwrap(),
            get_request("/healthy"),
            Leg::ControlPlane,
            PathClass::ControlPlane,
        ),
    )
    .await
    .expect("expired send must release the shared concurrency permit");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(collect_body(response).await, Bytes::from_static(b"ok"));
    assert_eq!(metrics.streams_open(), 0);
    upstream.abort();
}

#[tokio::test]
async fn stalled_upload_is_also_bounded_by_the_response_head_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let upstream = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        futures::future::pending::<()>().await;
        drop(socket);
    });
    let state = ForwardState::with_response_head_timeout(
        1,
        Duration::from_secs(1),
        Duration::from_millis(100),
    )
    .unwrap();
    let metrics = Arc::new(ProxyMetrics::new());
    let mut request = get_request("/upload");
    request.method = Method::POST;
    request.body =
        reqwest::Body::wrap_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        forward(
            &state,
            &metrics,
            &base,
            request,
            Leg::Inference,
            PathClass::Inference,
        ),
    )
    .await
    .expect("upload must be bounded even before it can finish sending");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(metrics.streams_open(), 0);
    upstream.abort();
}

#[tokio::test]
async fn healthy_slow_body_outlives_header_deadline_but_each_gap_resets_watchdog() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        for byte in *b"abcd" {
            tokio::time::sleep(Duration::from_millis(150)).await;
            socket
                .write_all(&[b'1', b'\r', b'\n', byte, b'\r', b'\n'])
                .await
                .unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let state = ForwardState::with_response_head_timeout(
        1,
        Duration::from_millis(500),
        Duration::from_millis(250),
    )
    .unwrap();
    let metrics = Arc::new(ProxyMetrics::new());
    let response = forward(
        &state,
        &metrics,
        &base,
        get_request("/events"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let start = tokio::time::Instant::now();
    assert_eq!(collect_body(response).await, Bytes::from_static(b"abcd"));
    assert!(start.elapsed() > Duration::from_millis(250));
    assert_eq!(metrics.stream_idle_aborts_total(), 0);
    assert_eq!(metrics.streams_open(), 0);
    upstream.await.unwrap();
}

#[tokio::test]
async fn body_stall_after_headers_hits_idle_watchdog_and_releases_slot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n")
            .await
            .unwrap();
        futures::future::pending::<()>().await;
    });
    let state = ForwardState::with_response_head_timeout(
        1,
        Duration::from_millis(100),
        Duration::from_secs(1),
    )
    .unwrap();
    let metrics = Arc::new(ProxyMetrics::new());
    let response = forward(
        &state,
        &metrics,
        &base,
        get_request("/events"),
        Leg::Inference,
        PathClass::Inference,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        Bytes::from_static(b"a")
    );
    assert!(matches!(
        body.frame().await.unwrap(),
        Err(routectl_cli::proxy::forward::ForwardBodyError::IdleTimeout)
    ));
    assert_eq!(metrics.stream_idle_aborts_total(), 1);
    assert_eq!(metrics.streams_open(), 0);
    assert!(body.frame().await.is_none());
    upstream.abort();
}
