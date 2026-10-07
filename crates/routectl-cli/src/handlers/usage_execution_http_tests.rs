//! An x-request-id is correlation, not idempotency. Two real HTTP executions
//! with the same value must account independently while echoing it unchanged.
use super::*;

#[tokio::test]
async fn reused_http_request_id_records_two_executions_and_retains_correlation() {
    use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"chatcmpl-accounting", "object":"chat.completion", "model":"gpt-4o",
            "created":0, "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
        }))).expect(2).mount(&upstream).await;
    let mut config = Config::default();
    config.providers.insert(
        "p".into(),
        ProviderEntry::openai_compat(
            upstream.uri(),
            crate::test_secret::file_ref("synthetic-ledger-key"),
        ),
    );
    config
        .models
        .insert("m".into(), ModelEntry::new("p", "gpt-4o"));
    config
        .aliases
        .insert("a".into(), AliasValue::Single("m".into()));
    let router = build_test_router(Arc::new(config)).await;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state = crate::server::AppState::for_test_with_usage(
        Arc::new(arc_swap::ArcSwap::from(Arc::new(router))),
        usage.clone(),
    );
    let app = axum::Router::new()
        .route(
            "/v1/chat/completions",
            axum::routing::post(crate::handlers::chat_completions::chat_completions),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn(
            crate::server::request_id::middleware,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for _ in 0..2 {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header("x-request-id", "shared-correlation")
            .json(&json!({"model":"a", "messages":[{"role":"user", "content":"hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-request-id"], "shared-correlation");
        let _: Value = response.json().await.unwrap();
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    drop(usage);
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .unwrap();
    let db = rusqlite::Connection::open(db_path).unwrap();
    let rows: Vec<(String, String, String, u64, u64)> = db.prepare(
        "SELECT request_id, extra, outcome, input_tokens, output_tokens FROM requests ORDER BY rowid"
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].0, rows[1].0);
    for (ledger_key, extra, outcome, input, output) in rows {
        assert_ne!(ledger_key, "shared-correlation");
        assert_eq!(
            serde_json::from_str::<Value>(&extra).unwrap()["correlation_request_id"],
            "shared-correlation"
        );
        assert_eq!(outcome, "ok");
        assert_eq!((input, output), (7, 3));
    }
}
