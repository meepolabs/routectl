//! How a request's row ends when the request does not finish normally, and
//! whether a window stays marked incomplete across a restart, through the
//! assembled daemon.
//!
//! - A stream client that hangs up mid-stream writes `client_disconnect`,
//!   and the cut it caused is not a missing terminal.
//! - A complete request whose client goes away while the upstream holds it
//!   has its handler future dropped by the server: the row is `cancelled`
//!   and names the target the walk reached.
//! - Rows lost to a full writer queue in one run leave a gap row that the
//!   next run's `/status/usage` reads back: a window spanning both runs is
//!   incomplete although the second process lost nothing.

use std::sync::Arc;
use std::time::Duration;

use routectl_usage::{UsageCounters, UsageRecord, UsageWriter, WriterMessage};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::observation_reconcile_daemon_tests::{
    Counts, Daemon, WAIT, anthropic_api_at, anthropic_message, plain_body, row_counts,
    stream_without_terminal, upstream_answering, usage_dir,
};

/// An upstream that reads each request, signals its arrival, writes
/// `head` (when given) and then holds the connection open without another
/// byte.
async fn holding_upstream(head: Option<String>) -> (String, Arc<tokio::sync::Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let arrived = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&arrived);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let signal = Arc::clone(&signal);
            let head = head.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 64 * 1024];
                let _ = socket.read(&mut buf).await;
                signal.notify_one();
                if let Some(head) = head {
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.flush().await;
                }
                tokio::time::sleep(Duration::from_mins(1)).await;
                drop(socket);
            });
        }
    });
    (base, arrived)
}

/// A chunked `text/event-stream` response head plus one chunk carrying the
/// opening frames of a stream that never reaches its terminal event.
fn open_stream_head() -> String {
    let frames = stream_without_terminal();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
         transfer-encoding: chunked\r\n\r\n{:x}\r\n{frames}\r\n",
        frames.len()
    )
}

/// The outcome, identity and missing-terminal columns of one row.
#[derive(Debug, PartialEq, Eq)]
struct EndedRow {
    outcome: String,
    alias: String,
    provider: Option<String>,
    model: Option<String>,
    upstream: Option<String>,
    provider_kind: Option<String>,
    missing_terminals: Option<i64>,
}

fn ended_row(db_path: &std::path::Path, request_id: &str) -> EndedRow {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    db.conn()
        .query_row(
            "SELECT outcome, alias, provider, model, upstream, provider_kind, \
                    missing_terminals FROM requests WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok(EndedRow {
                    outcome: r.get(0)?,
                    alias: r.get(1)?,
                    provider: r.get(2)?,
                    model: r.get(3)?,
                    upstream: r.get(4)?,
                    provider_kind: r.get(5)?,
                    missing_terminals: r.get(6)?,
                })
            },
        )
        .expect("exactly one row")
}

fn row_at_the_served_target(outcome: &str) -> EndedRow {
    EndedRow {
        outcome: outcome.to_string(),
        alias: "a".to_string(),
        provider: Some("p".to_string()),
        model: Some("m".to_string()),
        upstream: Some("upstream".to_string()),
        provider_kind: Some("anthropic-api".to_string()),
        missing_terminals: Some(0),
    }
}

/// A stream client reads the first frames and hangs up while the upstream
/// stream is still open.
///
/// Mutation check: finalize the stream's `tx.closed()` arm as `Ok` instead
/// of `client_disconnect` -> red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_client_hangup_writes_client_disconnect_and_no_missing_terminal() {
    // Arrange
    let (base, arrived) = holding_upstream(Some(open_stream_head())).await;
    let (_usage_dir, usage_db) = usage_dir();
    let (config, router) = anthropic_api_at(&base, &usage_db).await;
    let daemon = Daemon::spawn(config, router).await;
    const ID: &str = "hangup-stream";

    // Act: read the first bytes, then drop the response.
    let mut response = daemon
        .messages(ID, &plain_body(true))
        .send()
        .await
        .expect("the daemon answers");
    tokio::time::timeout(WAIT, arrived.notified())
        .await
        .expect("the upstream receives the request");
    let first = tokio::time::timeout(WAIT, response.chunk())
        .await
        .expect("the first frames arrive")
        .expect("read a chunk");
    drop(response);
    daemon.await_row(ID).await;

    // Assert
    assert!(
        first.is_some_and(|b| !b.is_empty()),
        "premise: the stream had started"
    );
    assert_eq!(
        ended_row(&daemon.usage_db, ID),
        row_at_the_served_target("client_disconnect")
    );
    daemon.shutdown().await;
}

/// A complete request whose client goes away while the upstream holds it:
/// the server drops the handler future, which no hangup observer saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_complete_handler_writes_cancelled_at_its_last_target() {
    // Arrange
    let (base, arrived) = holding_upstream(None).await;
    let (_usage_dir, usage_db) = usage_dir();
    let (config, router) = anthropic_api_at(&base, &usage_db).await;
    let daemon = Daemon::spawn(config, router).await;
    const ID: &str = "aborted-complete";
    let request = daemon.messages(ID, &plain_body(false));

    // Act: drop the in-flight client request once the upstream holds it.
    let client = tokio::spawn(async move { request.send().await });
    tokio::time::timeout(WAIT, arrived.notified())
        .await
        .expect("the upstream receives the request");
    client.abort();
    let _ = client.await;
    daemon.await_row(ID).await;

    // Assert
    assert_eq!(
        ended_row(&daemon.usage_db, ID),
        row_at_the_served_target("cancelled")
    );
    daemon.shutdown().await;
}

/// The window observation `/status/usage?window=all` reports.
async fn all_time_observation(daemon: &Daemon) -> (Value, Value) {
    let body: Value = reqwest::get(format!("{}/status/usage?window=all", daemon.base_url))
        .await
        .expect("GET /status/usage")
        .json()
        .await
        .expect("usage panel json");
    let data = &body["data"];
    (
        data["observation"].clone(),
        data["totals"]["requests"].clone(),
    )
}

fn first_run_record(request_id: &str) -> UsageRecord {
    UsageRecord {
        request_id: request_id.to_string(),
        alias: "a".to_string(),
        ts_start: chrono::Local::now().timestamp_millis(),
        outcome: routectl_usage::Outcome::Ok,
        ..UsageRecord::default()
    }
}

/// The first run: a writer over `db_path` behind a one-slot channel. Its
/// consumer starts only after `offered` rows were offered, so every row
/// past the first is refused as queue-full; then it drains and stops.
/// Returns the first run's queue-full drop count.
async fn first_run(db_path: &std::path::Path, offered: usize) -> u64 {
    let (tx, rx) = tokio::sync::mpsc::channel::<WriterMessage>(1);
    let counters = Arc::new(UsageCounters::default());
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
    for n in 0..offered {
        handle.try_send(first_run_record(&format!("first-run-{n}")));
    }
    let writer = UsageWriter::start_over_channel(db_path.to_path_buf(), 0, rx, counters.clone());
    drop(handle);
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("first-run writer shutdown");
    counters.dropped_full()
}

/// Run one writer over the ledger, offering `offered` rows to a one-slot
/// queue; restart the assembled daemon on the same file, serve one
/// request, and read the all-time window's observation.
async fn observation_after_restart(offered: usize) -> (u64, Value, Value, Counts) {
    let upstream = upstream_answering(anthropic_message()).await;
    let (_usage_dir, usage_db) = usage_dir();
    let (config, router) = anthropic_api_at(&upstream.uri(), &usage_db).await;
    let dropped_full = first_run(&config.usage.db_path, offered).await;

    let daemon = Daemon::spawn(config, router).await;
    const ID: &str = "second-run";
    let status = daemon
        .messages(ID, &plain_body(false))
        .send()
        .await
        .expect("the daemon answers")
        .status();
    assert_eq!(status, reqwest::StatusCode::OK);
    daemon.await_row(ID).await;
    let (observation, requests) = all_time_observation(&daemon).await;
    let second_run_counts = row_counts(&daemon.usage_db, ID);
    daemon.shutdown().await;
    (dropped_full, observation, requests, second_run_counts)
}

/// Mutation check: drop `!totals.has_gap()` from `window_observation` ->
/// red here (the second process's live counters are all zero, so only the
/// persisted gap row can mark the window).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queue_full_run_then_a_restart_reads_the_spanning_window_incomplete() {
    // Act
    let (dropped_full, observation, requests, second_run) = observation_after_restart(3).await;

    // Assert
    assert_eq!(dropped_full, 2, "premise: two of three rows were refused");
    assert_eq!(requests, 2, "one row from each run: {observation}");
    assert_eq!(second_run, Counts::default());
    assert_eq!(observation["gap_rows"], 1, "{observation}");
    assert_eq!(observation["gap_dropped"], 2, "{observation}");
    assert_eq!(observation["complete"], false, "{observation}");
}

/// The control: the same two runs with nothing refused read complete, so the
/// verdict above comes from the refused rows and not from the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lossless_run_then_a_restart_reads_the_spanning_window_complete() {
    // Act
    let (dropped_full, observation, requests, _) = observation_after_restart(1).await;

    // Assert
    assert_eq!(dropped_full, 0, "premise: nothing was refused");
    assert_eq!(requests, 2, "one row from each run: {observation}");
    assert_eq!(observation["gap_rows"], 0, "{observation}");
    assert_eq!(observation["complete"], true, "{observation}");
}
