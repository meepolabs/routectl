//! Every finished request writes exactly one INFO `request finished` line,
//! rendered from its row before the row is queued, so a row the writer's
//! queue refuses still has its line. The line carries a fixed key set, never
//! request content, and bounds and escapes the routing names a client can
//! influence. The per-request cache breadcrumb stays at DEBUG.

use std::sync::Arc;

use super::*;
use crate::handlers::request_finished_log::REQUEST_FINISHED;
use routectl_core::{
    ChatChunk, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent, Role, Usage,
    UsageDelta,
};
use routectl_testkit::{CapturedEvent, capture_events};
use routectl_usage::{UsageCounters, WriterMessage};
use tokio::sync::mpsc;

const ROW_ID: &str = "finished-row";

/// The pinned key set, in emission order.
const FINISHED_KEYS: [&str; 23] = [
    "request_id",
    "operation",
    "outcome",
    "http_status",
    "harness",
    "probe",
    "alias",
    "model",
    "upstream",
    "provider",
    "seat",
    "attempt_count",
    "fallback_count",
    "latency_ms",
    "ttfb_ms",
    "input_tokens",
    "output_tokens",
    "cache_read",
    "wire_losses",
    "translation_drops",
    "local_refusals",
    "field_repairs",
    "missing_terminals",
];

fn request(model: &str, text: &str) -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

/// A handle over a channel this test owns. `capacity` bounds the queue; the
/// receiver is returned so the channel stays open, and the counters so a
/// refusal is observable.
fn owned_handle(
    capacity: usize,
) -> (
    UsageHandle,
    mpsc::Receiver<WriterMessage>,
    Arc<UsageCounters>,
) {
    let (tx, rx) = mpsc::channel(capacity);
    let counters = Arc::new(UsageCounters::default());
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
    (handle, rx, counters)
}

fn capture_over(handle: UsageHandle, req: &routectl_core::ChatRequest) -> UsageCapture {
    let draft = build_usage_draft("anthropic", req, ROW_ID.to_string(), Harness::Other);
    UsageCapture::new(
        draft,
        handle,
        "ingress-1".to_string(),
        RequestObservation::new(),
    )
}

fn response() -> ChatResponse {
    ChatResponse {
        id: "resp-1".into(),
        model: "m".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 30,
            completion_tokens: 5,
            total_tokens: 35,
            cache_read_input_tokens: Some(20),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn terminal_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(UsageDelta {
            prompt_tokens: Some(30),
            completion_tokens: Some(5),
            total_tokens: Some(35),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn finished(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == REQUEST_FINISHED)
        .collect()
}

/// One finish path: what it does to a capture, the outcome its line must
/// carry, and the operation.
struct Row {
    name: &'static str,
    outcome: &'static str,
    operation: &'static str,
    run: fn(),
}

fn complete_ok() {
    let (handle, _rx, _) = owned_handle(8);
    let mut capture = capture_over(handle, &request("m", "hi"));
    capture.observe_response(&response());
    capture.finalize(Outcome::Ok);
}

fn stream_ok() {
    let (handle, _rx, _) = owned_handle(8);
    let mut capture = capture_over(handle, &request("m", "hi"));
    capture.mark_first_byte();
    capture.observe_chunk(&terminal_chunk());
    capture.mark_stream_http_committed();
    capture.finalize(Outcome::Ok);
}

fn upstream_error() {
    let (handle, _rx, _) = owned_handle(8);
    let mut capture = capture_over(handle, &request("m", "hi"));
    capture.observe_error(&Error::upstream("p", 500, "boom"));
    capture.finalize(Outcome::UpstreamError);
}

fn cancelled_drop() {
    let (handle, _rx, _) = owned_handle(8);
    drop(capture_over(handle, &request("m", "hi")));
}

fn count_tokens() {
    let (handle, _rx, _) = owned_handle(8);
    let req = request("m", "hi");
    let draft = UsageRecord {
        operation: Operation::CountTokens,
        ..build_usage_draft("anthropic", &req, ROW_ID.to_string(), Harness::Other)
    };
    let mut capture = UsageCapture::new(
        draft,
        handle,
        "ingress-1".to_string(),
        RequestObservation::new(),
    );
    capture.finalize(Outcome::Ok);
}

fn queue_refused() {
    let (handle, _rx, counters) = owned_handle(1);
    handle.try_send(UsageRecord::default());
    let mut capture = capture_over(handle, &request("m", "hi"));
    capture.finalize(Outcome::Ok);
    assert_eq!(
        counters.dropped_full(),
        1,
        "premise: the queue refused the row"
    );
}

const ROWS: [Row; 6] = [
    Row {
        name: "complete ok",
        outcome: "ok",
        operation: "inference",
        run: complete_ok,
    },
    Row {
        name: "stream ok",
        outcome: "ok",
        operation: "inference",
        run: stream_ok,
    },
    Row {
        name: "upstream error",
        outcome: "upstream_error",
        operation: "inference",
        run: upstream_error,
    },
    Row {
        name: "cancelled drop",
        outcome: "cancelled",
        operation: "inference",
        run: cancelled_drop,
    },
    Row {
        name: "count_tokens",
        outcome: "ok",
        operation: "count_tokens",
        run: count_tokens,
    },
    Row {
        name: "queue refused",
        outcome: "ok",
        operation: "inference",
        run: queue_refused,
    },
];

#[test]
fn every_finish_path_writes_exactly_one_info_request_finished_line_with_the_pinned_keys() {
    for row in ROWS {
        // Act
        let events = capture_events(row.run);

        // Assert
        let lines = finished(&events);
        assert_eq!(lines.len(), 1, "{}: {events:#?}", row.name);
        let line = lines[0];
        assert_eq!(line.level, tracing::Level::INFO, "{}", row.name);
        let keys: Vec<&str> = line.fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, FINISHED_KEYS, "{}", row.name);
        assert_eq!(line.field("request_id"), Some(ROW_ID), "{}", row.name);
        assert_eq!(line.field("outcome"), Some(row.outcome), "{}", row.name);
        assert_eq!(line.field("operation"), Some(row.operation), "{}", row.name);
    }
}

#[test]
fn a_finalized_capture_writes_no_second_line_when_dropped() {
    let events = capture_events(|| {
        let (handle, _rx, _) = owned_handle(8);
        let mut capture = capture_over(handle, &request("m", "hi"));
        capture.finalize(Outcome::Ok);
        capture.finalize(Outcome::UpstreamError);
        drop(capture);
    });

    assert_eq!(finished(&events).len(), 1, "{events:#?}");
}

#[test]
fn the_finished_line_renders_the_rows_columns_and_marks_unset_ones() {
    let events = capture_events(complete_ok);

    let line = finished(&events)[0];
    assert_eq!(line.field("http_status"), Some("200"));
    assert_eq!(line.field("input_tokens"), Some("10"));
    assert_eq!(line.field("output_tokens"), Some("5"));
    assert_eq!(line.field("cache_read"), Some("20"));
    assert_eq!(line.field("model"), Some("-"));
    assert_eq!(line.field("seat"), Some("-"));
    assert_eq!(line.field("ttfb_ms"), Some("-"));
}

/// A requested model that is long and carries control characters, reaching
/// the line through both carriers: the pre-dispatch alias seeded from the
/// request, and the slot's last target (a forwarded route sends the
/// requested id upstream).
#[test]
fn a_long_control_character_requested_model_is_escaped_and_bounded_on_the_line() {
    let hostile = format!("evil\x1b[31m\nmodel{}", "x".repeat(1000));
    let events = capture_events(|| {
        let (handle, _rx, _) = owned_handle(8);
        let slot = RequestObservation::new();
        slot.set_last_target(ServedIdentity {
            alias: Some(hostile.clone()),
            model: Some(hostile.clone()),
            upstream: Some(hostile.clone()),
            ..Default::default()
        });
        let draft = build_usage_draft(
            "anthropic",
            &request(&hostile, "hi"),
            ROW_ID.to_string(),
            Harness::Other,
        );
        drop(UsageCapture::new(
            draft,
            handle,
            "ingress-1".to_string(),
            slot,
        ));
    });

    let line = finished(&events)[0];
    for key in ["alias", "model", "upstream"] {
        let value = line.field(key).expect("key present");
        assert!(value.starts_with("evil?[31m?model"), "{key}: {value:?}");
        assert!(
            value.chars().count() <= 256,
            "{key}: {} chars",
            value.chars().count()
        );
        assert!(!value.chars().any(char::is_control), "{key}: {value:?}");
    }
}

#[test]
fn the_finished_line_carries_no_session_error_detail_or_body_text() {
    const SESSION: &str = "planted-session-7f3a";
    const DETAIL: &str = "planted-upstream-detail-91c2";
    const BODY: &str = "planted-prompt-text-4e8d";
    const REQUESTED: &str = "planted-requested-model-b5a0";
    let events = capture_events(|| {
        let (handle, _rx, _) = owned_handle(8);
        let mut req = request(REQUESTED, BODY);
        req.routectl_internal.inbound_session_key = Some(SESSION.to_string());
        let mut capture = capture_over(handle, &req);
        capture.observe_error(&Error::upstream("p", 500, DETAIL));
        capture.finalize(Outcome::UpstreamError);
    });

    let line = finished(&events)[0];
    let rendered = format!("{} {:?}", line.message, line.fields);
    assert!(
        rendered.contains(REQUESTED),
        "control: the capture sees planted client text on the line: {rendered}"
    );
    for planted in [SESSION, DETAIL, BODY] {
        assert!(!rendered.contains(planted), "{planted} leaked: {rendered}");
    }
}

#[test]
fn the_cache_summary_never_logs_at_info() {
    // Arrange: an auto-emitted breakpoint that read the cache, the case that
    // used to log the breadcrumb at INFO.
    let events = capture_events(|| {
        let (handle, _rx, _) = owned_handle(8);
        let mut capture = capture_over(handle, &request("m", "hi"));
        capture.cache_strategy = Some(AUTO_EMITTED);
        capture.observe_response(&response());
        capture.finalize(Outcome::Ok);
    });

    let cache_lines: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| e.message.starts_with("cache="))
        .collect();
    assert_eq!(
        cache_lines.len(),
        1,
        "control: the breadcrumb fired: {events:#?}"
    );
    assert_eq!(cache_lines[0].message, "cache=20/30 (66%)");
    assert_eq!(cache_lines[0].level, tracing::Level::DEBUG);
}
