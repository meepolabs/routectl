//! A handler future dropped mid-dispatch (the runtime aborts it while the
//! upstream is still holding the request) writes exactly one `cancelled`
//! row naming the target the walk reached, with no missing terminal: the
//! capture saw no client hangup, so it does not claim one.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, header};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

use super::tests::{CHECK_EVERY, Dialect, router};
use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

const WAIT: Duration = Duration::from_secs(10);

/// An upstream that accepts each connection, reads the request, signals
/// that a request arrived, and never answers.
async fn held_upstream() -> (String, Arc<tokio::sync::Notify>) {
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let arrived = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&arrived);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let signal = Arc::clone(&signal);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 64 * 1024];
                let _ = socket.read(&mut buf).await;
                signal.notify_one();
                tokio::time::sleep(Duration::from_mins(1)).await;
                drop(socket);
            });
        }
    });
    (base, arrived)
}

/// The identity and terminal columns of one persisted row.
#[derive(Debug, PartialEq, Eq)]
struct CancelRow {
    outcome: String,
    alias: String,
    provider: Option<String>,
    model: Option<String>,
    upstream: Option<String>,
    provider_kind: Option<String>,
    missing_terminals: Option<i64>,
}

fn read_rows(db_path: &std::path::Path) -> Vec<CancelRow> {
    let conn = rusqlite::Connection::open(db_path).expect("read open");
    let mut query = conn
        .prepare(
            "SELECT outcome, alias, provider, model, upstream, provider_kind, \
             missing_terminals FROM requests ORDER BY rowid",
        )
        .expect("prepare");
    query
        .query_map([], |r| {
            Ok(CancelRow {
                outcome: r.get(0)?,
                alias: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                upstream: r.get(4)?,
                provider_kind: r.get(5)?,
                missing_terminals: r.get(6)?,
            })
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers
}

/// Send one Anthropic request (`stream` or complete) through the ingress
/// against an upstream that never answers, abort the handler once the
/// upstream has the request, and return every row the ledger persisted.
async fn rows_after_aborting_mid_dispatch(stream: bool) -> Vec<CancelRow> {
    let (base, arrived) = held_upstream().await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(
        router(Dialect::Anthropic, &base, CHECK_EVERY).await,
    ));
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state = AppState::for_test_with_usage(Arc::clone(&swap), usage.clone());
    let body =
        Bytes::from(serde_json::to_vec(&Dialect::Anthropic.request_body(stream)).expect("json"));

    let handler = tokio::spawn(ingress_handle(
        state,
        headers(),
        None,
        Ok(body),
        AnthropicIngress,
    ));
    tokio::time::timeout(WAIT, arrived.notified())
        .await
        .expect("the upstream receives the request");
    handler.abort();
    let joined = tokio::time::timeout(WAIT, handler)
        .await
        .expect("the aborted handler finishes");

    assert!(
        joined.is_err_and(|e| e.is_cancelled()),
        "the handler was dropped mid-dispatch, not completed"
    );
    drop(usage);
    drop(swap);
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("usage writer shutdown");
    read_rows(&db_path)
}

fn cancelled_at_the_held_target() -> Vec<CancelRow> {
    vec![CancelRow {
        outcome: "cancelled".to_string(),
        alias: "a".to_string(),
        provider: Some("p".to_string()),
        model: Some("m".to_string()),
        upstream: Some("upstream".to_string()),
        provider_kind: Some("anthropic-api".to_string()),
        missing_terminals: Some(0),
    }]
}

#[tokio::test]
async fn a_complete_handler_aborted_mid_dispatch_writes_one_cancelled_row_at_its_last_target() {
    let rows = rows_after_aborting_mid_dispatch(false).await;

    assert_eq!(rows, cancelled_at_the_held_target());
}

/// The abort lands inside the early-flush grace, while the handler still
/// owns the capture and the pending dispatch.
#[tokio::test]
async fn a_stream_handler_aborted_mid_dispatch_writes_one_cancelled_row_at_its_last_target() {
    let rows = rows_after_aborting_mid_dispatch(true).await;

    assert_eq!(rows, cancelled_at_the_held_target());
}
