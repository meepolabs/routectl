//! Unit tests for `usage_capture` token-column stamping. Split out so
//! `usage_capture.rs` stays under the project's 800-line file ceiling.
//! Loaded via `#[cfg(test)] #[path = "usage_capture_tests.rs"] mod tests;`
//! from `usage_capture.rs`. `super::*` resolves to the `usage_capture`
//! module, so these tests reach the guard's private `record` field
//! directly to pin the persisted token columns.
//!
//! Coverage: the DB `input_tokens` column must hold cache-EXCLUSIVE new
//! input -- the inverse of `sum_prompt_tokens` -- so the disjoint cost
//! dimensions (input / cache_read / cache_write_*) do not double-count
//! cached tokens. Both capture sites (`observe_response`,
//! `observe_chunk`) must agree, and the None-vs-Some(0) contract must
//! survive the subtraction.

use super::*;
use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatChunk, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent, Role, Usage,
    UsageDelta,
};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

/// A throwaway usage handle for guard construction. The tests assert on
/// the in-memory `record` before `finalize`, so the writer is never
/// drained -- it only has to exist so `UsageCapture::new` has a handle.
/// Returns the `TempDir` so the caller holds it to drop-at-scope-end;
/// these tests never touch the DB file, only the in-memory record.
fn dummy_handle() -> (UsageHandle, UsageWriter, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path, CHANNEL_CAPACITY, 0, true);
    (handle, writer, dir)
}

/// A `UsageCapture` over a minimal draft, ready for `observe_*` calls.
fn capture() -> (UsageCapture, UsageWriter, tempfile::TempDir) {
    let req = routectl_core::ChatRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }],
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "req-1".to_string(), None);
    let (handle, writer, dir) = dummy_handle();
    (
        UsageCapture::new(draft, handle, "ingress-1".to_string()),
        writer,
        dir,
    )
}

/// A non-streaming response whose usage carries the Anthropic-style
/// cache-inclusive `prompt_tokens` plus the disjoint cache columns.
fn response_with_cache(
    prompt: Option<u32>,
    cache_read: Option<u32>,
    cache_creation_aggregate: Option<u32>,
    cache_write_5m: Option<u32>,
) -> ChatResponse {
    let usage = prompt.map(|p| Usage {
        prompt_tokens: p,
        completion_tokens: 50,
        total_tokens: p + 50,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation_aggregate,
        cache_creation: cache_write_5m.map(|v| CacheCreation {
            ephemeral_5m_input_tokens: Some(v),
            ephemeral_1h_input_tokens: None,
        }),
        ..Default::default()
    });
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
        usage,
        ..Default::default()
    }
}

/// A terminal stream chunk carrying the same cumulative usage delta
/// Anthropic emits on `message_delta`.
fn chunk_with_cache(
    prompt: Option<u32>,
    cache_read: Option<u32>,
    cache_creation_aggregate: Option<u32>,
    cache_write_5m: Option<u32>,
) -> ChatChunk {
    let usage = prompt.map(|p| UsageDelta {
        prompt_tokens: Some(p),
        completion_tokens: Some(50),
        total_tokens: Some(p + 50),
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation_aggregate,
        cache_creation: cache_write_5m.map(|v| CacheCreation {
            ephemeral_5m_input_tokens: Some(v),
            ephemeral_1h_input_tokens: None,
        }),
        ..Default::default()
    });
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage,
        ..Default::default()
    }
}

#[test]
fn observe_response_stores_cache_exclusive_input() {
    // Arrange: prompt is cache-INCLUSIVE (100 new + 600 read + 300 create).
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), Some(600), Some(300), Some(300));

    // Act
    cap.observe_response(&resp);

    // Assert: input is the cache-EXCLUSIVE new input only.
    assert_eq!(cap.record.input_tokens, Some(100_u64));
    // The disjoint cache columns are stamped exactly as before.
    assert_eq!(cap.record.cache_read, Some(600_u64));
    assert_eq!(cap.record.cache_write_5m, Some(300_u64));
    assert_eq!(cap.record.cache_write_1h, None);
    assert_eq!(cap.record.output_tokens, Some(50_u64));
}

#[test]
fn observe_chunk_matches_response_for_same_usage() {
    // Arrange: a terminal chunk carrying the identical cumulative usage.
    let (mut cap, _w, _dir) = capture();
    let chunk = chunk_with_cache(Some(1000), Some(600), Some(300), Some(300));

    // Act
    cap.observe_chunk(&chunk);

    // Assert: parity with observe_response -- cache-exclusive new input.
    assert_eq!(cap.record.input_tokens, Some(100_u64));
    assert_eq!(cap.record.cache_read, Some(600_u64));
    assert_eq!(cap.record.cache_write_5m, Some(300_u64));
}

#[test]
fn observe_response_absent_usage_leaves_input_none() {
    // Arrange: no usage on the response at all.
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(None, None, None, None);

    // Act
    cap.observe_response(&resp);

    // Assert: None preserved (never coerced to Some(0)).
    assert_eq!(cap.record.input_tokens, None);
}

#[test]
fn observe_response_no_cache_fields_keeps_full_prompt() {
    // Arrange: prompt present, both cache_read and cache_creation absent.
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), None, None, None);

    // Act
    cap.observe_response(&resp);

    // Assert: nothing to subtract => input equals the full prompt.
    assert_eq!(cap.record.input_tokens, Some(1000_u64));
}

#[test]
fn observe_response_fully_cached_prompt_stores_zero() {
    // Arrange: prompt == cache_read + cache_creation exactly (700+300).
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), Some(700), Some(300), None);

    // Act
    cap.observe_response(&resp);

    // Assert: a fully-cached prompt is a real Some(0), not None.
    assert_eq!(cap.record.input_tokens, Some(0_u64));
}

/// Spin until the writer has persisted `want` rows or a deadline passes.
fn wait_persisted(handle: &UsageHandle, want: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().persisted() < want {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

/// A `UsageCapture` plus the handle (so tests can poll the persisted
/// counter) and the writer (so tests can drain it to disk on shutdown).
fn capture_with_handle() -> (UsageCapture, UsageHandle, UsageWriter, tempfile::TempDir) {
    let req = routectl_core::ChatRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }],
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "req-take".to_string(), None);
    let (handle, writer, dir) = dummy_handle();
    let cap = UsageCapture::new(draft, handle.clone(), "ingress-1".to_string());
    (cap, handle, writer, dir)
}

#[tokio::test]
async fn finalize_moves_record_and_emits_one_row() {
    // Arrange
    let (mut cap, handle, writer, _dir) = capture_with_handle();

    // Act: a single finalize moves the owned record into the channel.
    cap.finalize(Outcome::Ok);
    drop(cap);

    // Assert: exactly one row reaches the writer.
    assert!(wait_persisted(&handle, 1), "row not persisted");
    writer.shutdown();
    assert_eq!(
        handle.counters().persisted(),
        1,
        "finalize must emit exactly one row"
    );
}

#[tokio::test]
async fn finalize_then_drop_still_one_row() {
    // Arrange
    let (mut cap, handle, writer, _dir) = capture_with_handle();

    // Act: explicit finalize sets `finalized`; the trailing Drop sees the
    // flag and short-circuits (never touches the moved-out record).
    cap.finalize(Outcome::Ok);
    drop(cap);

    // Assert: the Drop guard does NOT emit a second row.
    assert!(wait_persisted(&handle, 1), "row not persisted");
    writer.shutdown();
    assert_eq!(
        handle.counters().persisted(),
        1,
        "finalize-then-drop must persist exactly one row"
    );
}

#[test]
fn thrash_fires_only_for_auto_emitted_create_without_read() {
    // Thrash: routectl auto-emitted, a cache entry was created, no read.
    assert!(is_cache_thrash(Some("auto_emitted"), 300, 0));

    // Healthy: created AND read -> the cache is working, not thrash.
    assert!(!is_cache_thrash(Some("auto_emitted"), 300, 600));

    // Auto-emitted but nothing created -> not thrash.
    assert!(!is_cache_thrash(Some("auto_emitted"), 0, 0));

    // Read-only hit on a pre-existing entry (no new creation this request)
    // -> not thrash; the cache is being used.
    assert!(!is_cache_thrash(Some("auto_emitted"), 0, 600));

    // Caller-supplied breakpoint -> routectl did not decide; never thrash.
    assert!(!is_cache_thrash(Some("caller_supplied"), 300, 0));

    // Skipped strategies -> never thrash.
    assert!(!is_cache_thrash(Some("auto_skipped:no_capability"), 300, 0));
    assert!(!is_cache_thrash(Some("volatile_vetoed"), 300, 0));

    // No strategy recorded (request never dispatched) -> never thrash.
    assert!(!is_cache_thrash(None, 300, 0));
}

#[test]
fn cache_hit_pct_zero_read_is_zero() {
    // Arrange / Act / Assert: no read against a real prompt is 0%.
    assert_eq!(cache_hit_pct(0, 1000), 0);
}

#[test]
fn cache_hit_pct_full_read_is_hundred() {
    // Arrange / Act / Assert: read == prompt is exactly 100%.
    assert_eq!(cache_hit_pct(1000, 1000), 100);
}

#[test]
fn cache_hit_pct_partial_read_is_integer_percent() {
    // Arrange / Act / Assert: 600 of 1000 -> 60% (integer truncation).
    assert_eq!(cache_hit_pct(600, 1000), 60);
    // 1 of 3 -> 33% (floor, not round).
    assert_eq!(cache_hit_pct(1, 3), 33);
}

#[test]
fn cache_hit_pct_zero_prompt_guards_divide() {
    // Arrange / Act / Assert: prompt == 0 yields 0%, never a panic.
    assert_eq!(cache_hit_pct(0, 0), 0);
    assert_eq!(cache_hit_pct(500, 0), 0);
}
