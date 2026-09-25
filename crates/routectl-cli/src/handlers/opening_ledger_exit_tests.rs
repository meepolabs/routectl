//! The opening diagnostics a metered stream persists when it ends early:
//! explicit no-opening rows for every pre-opening error or disconnect, an
//! opening kept on a row that loses its client afterwards, and an accepted
//! terminal kept on a row that errors afterwards. Read back from a real
//! ledger; no test touches a live port, config or DB.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use futures::StreamExt;
use routectl_core::ChatChunk;
use routectl_providers::anthropic_api::sse::SseState;
use serde_json::{Value, json};

use super::opening_ledger_rig::*;
use super::opening_rig::*;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::confirmation_advance::ConfirmationTracker;

// ------------------------------------------------------------ no opening

#[tokio::test]
async fn a_pre_opening_http_error_is_recorded_as_no_opening() {
    // Arrange
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::error(
            529,
            &json!({"type": "error",
                    "error": {"type": "overloaded_error", "message": "busy"}}),
        ),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    let turn = send_as(
        &state,
        AnthropicIngress,
        session_headers(Some(SESSION)),
        &body(&turn_one()),
        Some("http-error"),
    )
    .await;
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(turn.status.as_u16(), 529, "{}", turn.raw);
    assert_eq!(row.outcome, "upstream_error");
    assert_eq!(row.extra, json!({"opening_present": false}));
}

#[tokio::test]
async fn a_client_leaving_after_the_opening_keeps_it_on_the_disconnect_row() {
    // Arrange: the terminal usage would arrive a second after first content.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::split(
            anthropic_start(Some(OPENER)) + &anthropic_content(),
            Duration::from_secs(1),
            anthropic_end(Some(TERMINAL)),
        ),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;
    let bytes = axum::body::Bytes::from(serde_json::to_vec(&body(&turn_one())).unwrap());

    // Act: read the first body piece, then hang up.
    let resp = super::ingress_handle(
        Arc::clone(&state),
        session_headers(Some(SESSION)),
        Some(crate::server::request_id::RequestId("hangup".into())),
        Ok(bytes),
        AnthropicIngress,
    )
    .await;
    let mut stream = resp.into_body().into_data_stream();
    let first = stream.next().await.expect("first piece").expect("readable");
    drop(stream);
    let row = ledger.only_row().await;

    // Assert
    assert!(
        String::from_utf8_lossy(&first).contains("message_start"),
        "the opening reached the client"
    );
    assert_eq!(row.outcome, "client_disconnect");
    assert_eq!(row.extra["opening_present"], true);
    assert_eq!(row.extra["opening_source"], "upstream_wire_unverified");
    assert_eq!(row.extra["opening_input"], OPENER.total());
    assert_eq!(row.extra["terminal_source"], "missing");
}

#[tokio::test]
async fn a_client_leaving_after_the_early_frame_keeps_the_provisional_opening() {
    // Arrange: the warm path with a dispatch that is still pending.
    let upstream = Upstream::start().await;
    let router = Arc::new(anthropic_router(&upstream).await);
    let ledger = Ledger::new();
    let (turn, capture) = metered_turn(&router, &ledger, "warm-hangup");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    // Act: take the early frame, then hang up while the dispatch waits.
    let task = tokio::spawn(super::warm_render_task(
        pending_dispatch(),
        AnthropicIngress,
        capture,
        tx,
        Arc::clone(&router),
        Arc::new(ConfirmationTracker::new()),
        turn,
    ));
    let early = rx.recv().await.expect("early frame");
    drop(rx);
    task.await.expect("warm task");
    let row = ledger.only_row().await;

    // Assert
    let frame: Value = serde_json::from_str(&early.data).expect("json");
    assert_eq!(early.event.as_deref(), Some("message_start"));
    assert_eq!(row.outcome, "client_disconnect");
    assert_eq!(row.extra["opening_present"], true);
    assert_eq!(row.extra["opening_provisional"], true);
    assert_eq!(
        row.extra["opening_input"],
        frame_input(&frame["message"]["usage"])
    );
}

#[tokio::test]
async fn a_client_gone_before_the_early_frame_is_recorded_as_no_opening() {
    // Arrange: the grace expires with the dispatch still pending, and the
    // client has already gone when the warm task tries its first send. The
    // test runtime is single-threaded, so the spawned warm task first runs
    // after the response is dropped.
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();

    // Act
    let resp = gate_stream_response(
        anthropic_router(&upstream).await,
        futures::stream::pending().boxed(),
        PAST_GRACE * 10,
        &ledger,
        "warm-gone",
    )
    .await;
    drop(resp);
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(row.outcome, "client_disconnect");
    assert_eq!(row.extra, json!({"opening_present": false}));
}

#[tokio::test]
async fn a_fast_stream_whose_first_item_is_an_error_is_recorded_as_no_opening() {
    // Arrange
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();

    // Act
    let resp = gate_response(
        anthropic_router(&upstream).await,
        vec![Err(upstream_error())],
        Duration::ZERO,
        &ledger,
        "fast-first-err",
    )
    .await;
    let frames = frames_of(resp).await;
    let row = ledger.only_row().await;

    // Assert
    assert!(
        frames
            .iter()
            .all(|(event, _)| event.as_deref() != Some("message_start")),
        "{frames:?}"
    );
    assert_eq!(row.outcome, "upstream_error");
    assert_eq!(row.extra["opening_present"], false, "{}", row.extra);
    assert!(row.extra.get("opening_source").is_none(), "{}", row.extra);
}

#[tokio::test]
async fn a_fast_stream_whose_client_leaves_before_its_first_event_is_no_opening() {
    // Arrange: the dispatch resolved, but its first chunk never arrives.
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();

    // Act
    let resp = gate_stream_response(
        anthropic_router(&upstream).await,
        futures::stream::pending().boxed(),
        Duration::ZERO,
        &ledger,
        "fast-gone",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    drop(resp);
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(row.outcome, "client_disconnect");
    assert_eq!(row.extra, json!({"opening_present": false}));
}

#[tokio::test]
async fn a_first_chunk_that_fails_to_render_is_recorded_as_no_opening() {
    // Arrange: a tool-call index past the renderer's bound.
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();
    let bad = ChatChunk {
        choices: vec![routectl_core::ChunkChoice {
            index: 0,
            delta: routectl_core::ChunkDelta {
                tool_calls: Some(vec![json!({"index": 1_000_000, "id": "t",
                    "function": {"name": "f", "arguments": "{}"}})]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    };

    // Act
    let resp = gate_response(
        anthropic_router(&upstream).await,
        vec![Ok(bad)],
        Duration::ZERO,
        &ledger,
        "render-err",
    )
    .await;
    let frames = frames_of(resp).await;
    let row = ledger.only_row().await;

    // Assert: premise -- the renderer failed (only its error event).
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0.as_deref(), Some("error"));
    assert_eq!(row.outcome, "upstream_error");
    assert_eq!(row.extra["opening_present"], false, "{}", row.extra);
}

#[tokio::test]
async fn a_terminal_accepted_before_a_later_upstream_error_is_kept_on_the_row() {
    // Arrange: a full stream through the real parser, then an error.
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();
    let mut state = SseState::default();
    let mut items: Vec<_> = parse(&anthropic_stream(Some(OPENER), Some(TERMINAL)), &mut state)
        .into_iter()
        .map(Ok)
        .collect();
    items.push(Err(upstream_error()));

    // Act
    let resp = gate_response(
        anthropic_router(&upstream).await,
        items,
        Duration::ZERO,
        &ledger,
        "terminal-then-err",
    )
    .await;
    let frames = frames_of(resp).await;
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(
        row.extra["opening_input"],
        frame_input(opening_usage_of(&frames))
    );
    assert_eq!(row.outcome, "upstream_error");
    assert_eq!(row.extra["opening_present"], true);
    assert_eq!(row.extra["terminal_source"], "explicit_final");
    assert_eq!(row.extra["terminal_input"], TERMINAL.total());
}

#[tokio::test]
async fn an_opening_synthesized_only_at_the_end_of_an_empty_stream_is_recorded() {
    // Arrange: a resolved dispatch whose stream ends without a chunk, so
    // the only opening is the one the end-of-stream render synthesizes.
    let upstream = Upstream::start().await;
    let ledger = Ledger::new();

    // Act
    let frames = gate_turn(
        anthropic_router(&upstream).await,
        Vec::new(),
        Duration::ZERO,
        &ledger,
        "eos-only",
    )
    .await;
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(
        row.extra["opening_input"],
        frame_input(opening_usage_of(&frames))
    );
    assert_eq!(row.outcome, "ok");
    assert_eq!(row.extra["opening_present"], true);
    assert_eq!(row.extra["terminal_source"], "missing");
}
