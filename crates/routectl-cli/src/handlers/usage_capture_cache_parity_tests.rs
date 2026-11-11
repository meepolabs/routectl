//! A request's cache writes are recorded the same whether its response was
//! complete or streamed. Each row's usage runs through the real Anthropic
//! complete parser and the real stream parser into a capture, and both
//! records must carry the same write total, lifetime split and new input.
//! The auto-cache outcome and the per-request cache summary read that total.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use routectl_providers::anthropic_api::{response, sse::SseState};
use routectl_testkit::{CapturedEvent, capture_events};
use routectl_usage::{UsageCounters, WriterMessage};
use serde_json::json;
use tokio::sync::mpsc;

/// A capture over a channel the test owns, so finalize can queue its row
/// without a writer thread. The receiver keeps the channel open.
fn capture() -> (UsageCapture, mpsc::Receiver<WriterMessage>) {
    let (tx, rx) = mpsc::channel(8);
    let handle = routectl_usage::handle_over_channel_with(tx, Arc::new(UsageCounters::default()));
    let req = routectl_core::ChatRequest {
        model: "m".to_string(),
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "req-1".to_string(), Harness::Other);
    let capture = UsageCapture::new(
        draft,
        handle,
        "ingress-1".to_string(),
        RequestObservation::new(),
    );
    (capture, rx)
}

fn complete_body(usage: &str) -> Value {
    let usage: Value = serde_json::from_str(usage).expect("usage json");
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage,
    })
}

fn stream_events(opener: &str, delta: &str) -> [String; 2] {
    [
        format!(
            r#"{{"type":"message_start","message":{{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-opus-4-8","stop_reason":null,"stop_sequence":null,"usage":{opener}}}}}"#
        ),
        format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"usage":{delta}}}"#
        ),
    ]
}

fn record_from_complete(usage: &str) -> UsageRecord {
    let (mut capture, _rx) = capture();
    let resp = response::normalize("test", complete_body(usage)).expect("complete parses");
    capture.observe_response(&resp);
    capture.record.clone()
}

fn record_from_stream(opener: &str, delta: &str) -> UsageRecord {
    let (mut capture, _rx) = capture();
    let mut state = SseState::default();
    for event in stream_events(opener, delta) {
        if let Some(chunk) = state.parse_event("test", &event).expect("stream parses") {
            capture.observe_chunk(&chunk);
        }
    }
    capture.record.clone()
}

/// One request's usage as a complete body and as a stream's opener and
/// closing delta, with the cache-write columns both records must carry.
struct ParityRow {
    name: &'static str,
    complete: &'static str,
    opener: &'static str,
    delta: &'static str,
    total: Option<u64>,
    split: Option<&'static [(u32, u64)]>,
}

/// Every way a record's cache-write columns differ from the row, as text.
fn mismatches(row: &ParityRow, path: &str, record: &UsageRecord) -> Vec<String> {
    let split = row
        .split
        .map(|pairs| pairs.iter().copied().collect::<BTreeMap<_, _>>());
    let mut out = Vec::new();
    if record.cache_write_tokens != row.total {
        out.push(format!(
            "{} ({path}): total {:?}, want {:?}",
            row.name, record.cache_write_tokens, row.total
        ));
    }
    if record.cache_write_lifetimes != split {
        out.push(format!(
            "{} ({path}): split {:?}, want {split:?}",
            row.name, record.cache_write_lifetimes
        ));
    }
    if record.input_tokens != Some(2) {
        out.push(format!(
            "{} ({path}): input {:?}, want Some(2)",
            row.name, record.input_tokens
        ));
    }
    if record.cache_write_5m.is_some() || record.cache_write_1h.is_some() {
        out.push(format!(
            "{} ({path}): legacy columns {:?} / {:?}, want none",
            row.name, record.cache_write_5m, record.cache_write_1h
        ));
    }
    out
}

/// The total-only closing delta follows the documented Anthropic streaming
/// sample's `message_delta.usage` shape: input, both cache totals and output,
/// no `cache_creation` object
/// (<https://platform.claude.com/docs/en/build-with-claude/streaming>).
const TOTAL_ONLY_2523: &str = r#"{"input_tokens":2,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0,"output_tokens":7}"#;

const PARITY_ROWS: [ParityRow; 8] = [
    ParityRow {
        name: "split",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":4,"ephemeral_1h_input_tokens":6}}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":4,"ephemeral_1h_input_tokens":6}}"#,
        delta: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":4,"ephemeral_1h_input_tokens":6}}"#,
        total: Some(10),
        split: Some(&[(300, 4), (3600, 6)]),
    },
    ParityRow {
        name: "total only",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0}"#,
        delta: TOTAL_ONLY_2523,
        total: Some(2523),
        split: None,
    },
    ParityRow {
        name: "observed zero",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}"#,
        delta: r#"{"input_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":7}"#,
        total: Some(0),
        split: Some(&[(300, 0), (3600, 0)]),
    },
    ParityRow {
        name: "absent",
        complete: r#"{"input_tokens":2,"output_tokens":7}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1}"#,
        delta: r#"{"output_tokens":7}"#,
        total: None,
        split: None,
    },
    ParityRow {
        name: "30m lifetime",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":640,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_30m_input_tokens":640}}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":640,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_30m_input_tokens":640}}"#,
        delta: r#"{"input_tokens":2,"cache_creation_input_tokens":640,"cache_read_input_tokens":0,"output_tokens":7}"#,
        total: Some(640),
        split: Some(&[(1800, 640)]),
    },
    ParityRow {
        name: "refused name keeps the total and leaves the split",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":1000,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":600,"ephemeral_input_tokens":400}}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":1000,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":600,"ephemeral_input_tokens":400}}"#,
        delta: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":1000,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":600,"ephemeral_input_tokens":400}}"#,
        total: Some(1000),
        split: Some(&[(300, 600)]),
    },
    ParityRow {
        name: "opener split with a total-only delta",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":2523}}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":2523}}"#,
        delta: TOTAL_ONLY_2523,
        total: Some(2523),
        split: Some(&[(300, 2523)]),
    },
    ParityRow {
        name: "mismatching total",
        complete: r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":2523,"cache_read_input_tokens":0}"#,
        opener: r#"{"input_tokens":2,"output_tokens":1,"cache_creation_input_tokens":2000,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":2000}}"#,
        delta: TOTAL_ONLY_2523,
        total: Some(2523),
        split: None,
    },
];

#[test]
fn complete_and_streamed_usage_record_the_same_cache_writes() {
    // Arrange / Act
    let failures: Vec<String> = PARITY_ROWS
        .iter()
        .flat_map(|row| {
            let complete = record_from_complete(row.complete);
            let stream = record_from_stream(row.opener, row.delta);
            let mut found = mismatches(row, "complete", &complete);
            found.extend(mismatches(row, "stream", &stream));
            found
        })
        .collect();

    // Assert
    assert!(failures.is_empty(), "{failures:#?}");
}

fn finalized_events(usage: &str) -> Vec<CapturedEvent> {
    capture_events(|| {
        let (mut capture, _rx) = capture();
        capture.cache_strategy = Some(AUTO_EMITTED);
        let resp = response::normalize("test", complete_body(usage)).expect("complete parses");
        capture.observe_response(&resp);
        capture.finalize(Outcome::Ok);
    })
}

fn outcome_levels(events: &[CapturedEvent]) -> Vec<tracing::Level> {
    events
        .iter()
        .filter(|e| e.message == "cache_auto_outcome")
        .map(|e| e.level)
        .collect()
}

fn summary(events: &[CapturedEvent]) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e.message.starts_with("cache="))
        .map(|e| e.message.as_str())
        .collect()
}

#[test]
fn the_cache_logs_read_the_write_total_of_any_lifetime() {
    // Arrange: an auto-emitted breakpoint that wrote only at a 30m lifetime
    // and read nothing (the thrash signature), and a zero write as the
    // control that must stay silent.
    let rows: [(&str, &str, Vec<tracing::Level>, &str); 2] = [
        (
            "30m-only write with no read",
            r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":640,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_30m_input_tokens":640}}"#,
            vec![tracing::Level::WARN],
            "cache=0/642 (0%)",
        ),
        (
            "zero write",
            r#"{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}"#,
            Vec::new(),
            "cache=0/2 (0%)",
        ),
    ];

    for (name, usage, want_outcome, want_summary) in rows {
        // Act
        let events = finalized_events(usage);

        // Assert
        assert_eq!(outcome_levels(&events), want_outcome, "{name}: {events:#?}");
        assert_eq!(summary(&events), vec![want_summary], "{name}");
    }
}
