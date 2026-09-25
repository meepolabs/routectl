//! The opening-accuracy report read back from a ledger the real request
//! path wrote, across a daemon restart: every row persisted by either
//! process is in the report, categorized by the labels the producer actually
//! wrote, and the anchored row's verdict is the one its own numbers imply.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use serde_json::json;

use super::opening_rig::*;
use crate::commands::opening_accuracy::{
    PROVENANCE_NOTICE, Verdict, build_report, opening_accuracy_for_path,
};
use crate::commands::usage::WindowBounds;
use crate::ingress::openai::OpenAiIngress;
use crate::server::AppState;

const ACTUAL: u64 = 12_345;
const EVERYTHING: WindowBounds = WindowBounds {
    from_ms: 0,
    to_ms: i64::MAX,
};

/// One daemon process: its state and the writer persisting into `path`.
struct Process {
    state: Arc<AppState>,
    writer: routectl_usage::UsageWriter,
    usage: routectl_usage::UsageHandle,
}

impl Process {
    async fn start(path: &std::path::Path, upstream: &Upstream) -> Self {
        let (usage, writer) = routectl_usage::UsageWriter::start(
            path.to_path_buf(),
            routectl_usage::CHANNEL_CAPACITY,
            0,
            true,
        );
        let router = build(translated_config(upstream.base(), "glm-4.6")).await;
        router.publish_probe_incarnation();
        let state =
            AppState::for_test_with_usage(Arc::new(ArcSwap::from_pointee(router)), usage.clone());
        Self {
            state,
            writer,
            usage,
        }
    }

    /// Wait for `n` rows from this process, then stop it the way the daemon
    /// does: drain the writer and drop everything in memory.
    async fn stop_after(self, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.usage.counters().persisted() < n {
            assert!(Instant::now() < deadline, "{n} rows never landed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(self.state);
        drop(self.usage);
        let writer = self.writer;
        tokio::task::spawn_blocking(move || writer.shutdown())
            .await
            .expect("writer shutdown");
    }
}

async fn anthropic(state: &Arc<AppState>, messages: &[serde_json::Value], id: &str) -> Turn {
    let turn = send_as(
        state,
        crate::ingress::anthropic::AnthropicIngress,
        session_headers(Some(SESSION)),
        &body(messages),
        Some(id),
    )
    .await;
    assert_eq!(turn.status.as_u16(), 200, "{}", turn.raw);
    turn
}

#[tokio::test]
async fn rows_from_before_and_after_a_restart_are_all_read_back_from_disk() {
    // Arrange: one process anchors turn two on turn one; a second process on
    // the same file starts cold (anchors are not kept across a restart).
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("usage.db");
    let first = Process::start(&path, &upstream).await;
    anthropic(&first.state, &turn_one(), "p1-cold").await;
    let warm = anthropic(&first.state, &turn_two(), "p1-anchored").await;
    let chat = json!({"model": "claude-opus", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]});
    send_as(
        &first.state,
        OpenAiIngress,
        session_headers(None),
        &chat,
        Some("p1-openai"),
    )
    .await;
    first.stop_after(3).await;
    let second = Process::start(&path, &upstream).await;
    anthropic(&second.state, &turn_three(), "p2-cold").await;
    second.stop_after(1).await;

    // Act: a fresh read-only open of the file both processes wrote.
    let db = routectl_usage::open_readonly(&path).expect("read-only");
    let report = build_report(&db, EVERYTHING).expect("report");
    let (rendered, verdict) =
        opening_accuracy_for_path(&path, "== t ==", EVERYTHING).expect("render");
    assert_eq!(verdict, report.verdict());

    // Assert: every Anthropic row, and only those, from both processes.
    assert_eq!(report.universe, 3);
    assert_eq!(report.classified(), 3);
    assert_eq!(report.legacy, 0);
    assert!(report.unclassifiable.is_empty(), "{report:?}");
    let sources: Vec<(&str, u64)> = report.known_by_source.clone().into_iter().collect();
    assert_eq!(sources, vec![("anchor", 1), ("raw", 2)]);
    let lane = &report.lanes["openai-compat:compat/glm@glm-4.6"];
    assert_eq!(lane.rows, 3);
    assert_eq!(lane.reasons.get("cold"), Some(&2), "{lane:?}");
    assert_eq!(lane.terminals.get("explicit_final"), Some(&3), "{lane:?}");
    assert_eq!(report.cold_start["raw"].not_comparable, 0);
    // The anchored row's verdict follows from what the client was shown.
    let opening = warm.opening_input();
    assert_eq!(
        opening,
        expected_anchor(&body(&turn_one()), &body(&turn_two()), ACTUAL)
    );
    let within = u128::from(opening.abs_diff(ACTUAL)) * 100 <= 5 * u128::from(ACTUAL);
    assert_eq!(report.anchored.eligible, 1);
    assert_eq!(report.anchored.pass, u64::from(within));
    // A compatible endpoint's explicit report is not vendor-verified.
    assert_eq!(report.anchored.pass_unverified, u64::from(within));
    // Every terminal was relayed by a compatible endpoint: never a clean pass.
    assert_eq!(report.unverified_reports, 3);
    assert_eq!(
        report.verdict(),
        if within {
            Verdict::BackendPending
        } else {
            Verdict::Fail
        }
    );
    assert!(
        rendered.contains("universe: 3 anthropic streaming rows"),
        "{rendered}"
    );
    assert!(rendered.contains(PROVENANCE_NOTICE), "{rendered}");
}

#[tokio::test]
async fn a_pre_opening_error_row_is_counted_as_no_opening_not_lost() {
    // Arrange: the upstream refuses before any opening.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::error(
            503,
            &json!({"error": {"message": "busy", "type": "server_error"}}),
        ),
    );
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("usage.db");
    let process = Process::start(&path, &upstream).await;
    send_as(
        &process.state,
        crate::ingress::anthropic::AnthropicIngress,
        session_headers(Some(SESSION)),
        &body(&turn_one()),
        Some("refused"),
    )
    .await;
    process.stop_after(1).await;

    // Act
    let db = routectl_usage::open_readonly(&path).expect("read-only");
    let report = build_report(&db, EVERYTHING).expect("report");

    // Assert
    assert_eq!(report.universe, 1);
    assert_eq!(report.no_opening, 1);
    assert_eq!(report.verdict(), Verdict::Insufficient);
}
